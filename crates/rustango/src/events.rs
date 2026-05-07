//! Domain event bus — typed pub-sub for application-level events
//! decoupled from the ORM. Closes future-backlog item #45 ("internal
//! event bus / domain event dispatch decoupled from ORM signals").
//!
//! ## When to use this vs `crate::signals`
//!
//! - **`signals`** — per-`Model` lifecycle hooks (pre/post save,
//!   pre/post delete). Tied to ORM write paths. Use when you need
//!   "every time row X changes, do Y."
//! - **`events`** — arbitrary application events not tied to a
//!   specific Model. Use when you need "publish that an order was
//!   placed and let mail / billing / audit each react in their own
//!   subscriber." Decouples cross-component fanout.
//!
//! Same shape (typed, async, multi-subscriber, sequential dispatch);
//! different intent.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::events::EventBus;
//! use std::sync::Arc;
//!
//! #[derive(Clone)]
//! struct OrderPlaced { order_id: i64, total_cents: i64 }
//!
//! let bus = EventBus::new();
//!
//! // Subscribe (anywhere — main, app init, a service constructor):
//! bus.subscribe::<OrderPlaced, _>(|e| Box::pin(async move {
//!     println!("billing: charging {} cents for order {}", e.total_cents, e.order_id);
//! })).await;
//!
//! // Publish (from a handler, a job, a worker):
//! bus.publish(OrderPlaced { order_id: 42, total_cents: 9999 }).await;
//! ```
//!
//! ## Semantics
//!
//! - Subscribers run **sequentially**, in subscription order, awaited
//!   one at a time. (For parallel fanout, wrap a subscriber body in
//!   `tokio::spawn`.)
//! - The event is `Clone`d once per subscriber so each receives an
//!   owned value. `E: Clone + Send + Sync + 'static` is required.
//! - A panicking subscriber aborts the dispatch chain and propagates
//!   to the caller of `publish`. Use `tokio::spawn` for isolation if
//!   that's a concern.
//! - The bus is cheap to clone — internal state is `Arc`-shared.
//!   Pass clones into axum State, services, etc.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::Mutex;

/// Future returned by event handlers. `'static` because the handler
/// is stored as `Arc<dyn ...>` and may run after the caller returns.
pub type HandlerFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Opaque identifier returned by [`EventBus::subscribe`] for later
/// use with [`EventBus::unsubscribe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriberId(u64);

type AnyHandler = Arc<dyn Any + Send + Sync>;

/// Sized wrapper around the type-erased handler. `Arc<dyn Any>::downcast`
/// requires the inner type to be `Sized`, which `dyn Fn(E) -> _` isn't —
/// so we store a struct that owns the boxed closure instead and
/// downcast back through this wrapper.
struct TypedHandler<E: 'static> {
    f: Arc<dyn Fn(E) -> HandlerFuture + Send + Sync>,
}

#[derive(Default)]
struct Inner {
    /// Per-event-type vector of `(id, handler)` pairs. The handler
    /// is `Arc<dyn Fn(E) -> HandlerFuture + Send + Sync>` boxed into
    /// `dyn Any` so the registry stays heterogeneous.
    bags: HashMap<TypeId, Vec<(SubscriberId, AnyHandler)>>,
    next_id: u64,
}

/// In-process domain event bus. Cheap to clone — internal state is
/// `Arc`-shared.
#[derive(Default, Clone)]
pub struct EventBus {
    inner: Arc<Mutex<Inner>>,
}

impl EventBus {
    /// Create a new, empty bus. Cheap (no allocation beyond the
    /// `Arc<Mutex<...>>` shell).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `handler` to fire on every [`Self::publish`] of
    /// events of type `E`. Returns a [`SubscriberId`] that can be
    /// passed to [`Self::unsubscribe`] to stop receiving events.
    ///
    /// Handlers run sequentially — see the module rustdoc for the
    /// dispatch contract.
    pub async fn subscribe<E, F>(&self, handler: F) -> SubscriberId
    where
        E: Clone + Send + Sync + 'static,
        F: Fn(E) -> HandlerFuture + Send + Sync + 'static,
    {
        let wrapper: Arc<TypedHandler<E>> = Arc::new(TypedHandler {
            f: Arc::new(handler),
        });
        let any: AnyHandler = wrapper;
        let mut inner = self.inner.lock().await;
        inner.next_id += 1;
        let id = SubscriberId(inner.next_id);
        inner
            .bags
            .entry(TypeId::of::<E>())
            .or_default()
            .push((id, any));
        id
    }

    /// Stop receiving events for the given subscriber. No-op if
    /// `id` was never registered or was already removed.
    pub async fn unsubscribe(&self, id: SubscriberId) {
        let mut inner = self.inner.lock().await;
        for bag in inner.bags.values_mut() {
            bag.retain(|(sid, _)| *sid != id);
        }
    }

    /// Publish `event` — runs every subscriber registered for type
    /// `E`, sequentially, awaiting each in turn. Subscribers
    /// registered for OTHER event types are not invoked. No-op when
    /// no subscribers are registered for `E`.
    pub async fn publish<E>(&self, event: E)
    where
        E: Clone + Send + Sync + 'static,
    {
        // Snapshot the handler list while holding the lock so a
        // subscriber can call back into `publish`/`subscribe` from
        // its body without deadlocking. Subscribers added during
        // dispatch don't fire for the in-flight event (consistent
        // with the canonical Django signals semantics).
        let handlers: Vec<AnyHandler> = {
            let inner = self.inner.lock().await;
            inner
                .bags
                .get(&TypeId::of::<E>())
                .map(|bag| bag.iter().map(|(_, h)| h.clone()).collect())
                .unwrap_or_default()
        };
        for any in handlers {
            // Downcast the type-erased `Arc<dyn Any>` back into
            // `Arc<TypedHandler<E>>` and call the inner closure.
            // Safe by construction — we only insert handlers under
            // the matching TypeId.
            if let Ok(wrapper) = any.downcast::<TypedHandler<E>>() {
                let fut = (wrapper.f)(event.clone());
                fut.await;
            }
        }
    }

    /// Number of subscribers currently registered for type `E`.
    /// Useful for tests + diagnostics.
    pub async fn subscriber_count<E>(&self) -> usize
    where
        E: 'static,
    {
        let inner = self.inner.lock().await;
        inner
            .bags
            .get(&TypeId::of::<E>())
            .map_or(0, std::vec::Vec::len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Debug)]
    struct PingEvent(i32);

    #[derive(Clone, Debug)]
    struct PongEvent(String);

    #[tokio::test]
    async fn subscribe_and_publish_runs_handler() {
        let bus = EventBus::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        bus.subscribe::<PingEvent, _>(move |e| {
            let c = c.clone();
            Box::pin(async move {
                c.fetch_add(e.0 as usize, Ordering::SeqCst);
            })
        })
        .await;

        bus.publish(PingEvent(3)).await;
        bus.publish(PingEvent(7)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 10);
    }

    #[tokio::test]
    async fn handlers_are_typed_no_cross_talk() {
        let bus = EventBus::new();
        let pings = Arc::new(AtomicUsize::new(0));
        let pongs = Arc::new(AtomicUsize::new(0));

        let p1 = pings.clone();
        bus.subscribe::<PingEvent, _>(move |_e| {
            let p1 = p1.clone();
            Box::pin(async move {
                p1.fetch_add(1, Ordering::SeqCst);
            })
        })
        .await;

        let p2 = pongs.clone();
        bus.subscribe::<PongEvent, _>(move |_e| {
            let p2 = p2.clone();
            Box::pin(async move {
                p2.fetch_add(1, Ordering::SeqCst);
            })
        })
        .await;

        bus.publish(PingEvent(1)).await;
        bus.publish(PongEvent("hi".into())).await;
        bus.publish(PingEvent(2)).await;

        assert_eq!(pings.load(Ordering::SeqCst), 2);
        assert_eq!(pongs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn multiple_subscribers_run_sequentially() {
        let bus = EventBus::new();
        // Use a Vec to record the ORDER subscribers run in, not just a count.
        let order: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

        let o1 = order.clone();
        bus.subscribe::<PingEvent, _>(move |_| {
            let o1 = o1.clone();
            Box::pin(async move {
                o1.lock().await.push("first");
            })
        })
        .await;
        let o2 = order.clone();
        bus.subscribe::<PingEvent, _>(move |_| {
            let o2 = o2.clone();
            Box::pin(async move {
                o2.lock().await.push("second");
            })
        })
        .await;

        bus.publish(PingEvent(0)).await;
        let recorded = order.lock().await.clone();
        assert_eq!(recorded, vec!["first", "second"]);
    }

    #[tokio::test]
    async fn unsubscribe_stops_delivery() {
        let bus = EventBus::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let id = bus
            .subscribe::<PingEvent, _>(move |_| {
                let c = c.clone();
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                })
            })
            .await;

        bus.publish(PingEvent(0)).await;
        bus.unsubscribe(id).await;
        bus.publish(PingEvent(0)).await;

        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert_eq!(bus.subscriber_count::<PingEvent>().await, 0);
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_is_noop() {
        let bus = EventBus::new();
        // No panic, no error — just nothing happens.
        bus.publish(PingEvent(123)).await;
        assert_eq!(bus.subscriber_count::<PingEvent>().await, 0);
    }

    #[tokio::test]
    async fn cloned_bus_shares_subscribers() {
        let bus = EventBus::new();
        let bus2 = bus.clone();
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        bus.subscribe::<PingEvent, _>(move |_| {
            let c = c.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
            })
        })
        .await;
        // Publish via the OTHER handle — same underlying state.
        bus2.publish(PingEvent(0)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
}
