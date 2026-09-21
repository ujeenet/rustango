//! Domain event bus: typed pub-sub for application events, separate
//! from the ORM.
//!
//! ## This or `crate::signals`?
//!
//! - **`signals`** hook a `Model` lifecycle (pre/post save, pre/post
//!   delete). Use them for "whenever row X changes, do Y".
//! - **`events`** are not tied to a model. Use them for "an order was
//!   placed", where mail, billing and audit each react on their own.
//!
//! Both are typed, async, multi-subscriber and dispatch in order.
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
//! - Subscribers run one at a time, in subscription order. For
//!   parallel fan-out, `tokio::spawn` inside the subscriber.
//! - The event is cloned once per subscriber, so `E` must be
//!   `Clone + Send + Sync + 'static`.
//! - If a subscriber panics, the rest do not run and the panic reaches
//!   the caller of `publish`. `tokio::spawn` isolates it.
//! - The bus is cheap to clone; state is shared. Pass clones into axum
//!   state or your services.

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

/// Sized wrapper around a handler. `Arc<dyn Any>::downcast` needs a
/// `Sized` inner type, and `dyn Fn(E) -> _` is not one, so the closure
/// lives in this struct and we downcast to the struct.
struct TypedHandler<E: 'static> {
    f: Arc<dyn Fn(E) -> HandlerFuture + Send + Sync>,
}

#[derive(Default)]
struct Inner {
    /// `(id, handler)` pairs per event type. Handlers are stored as
    /// `dyn Any` so one map can hold every event type.
    bags: HashMap<TypeId, Vec<(SubscriberId, AnyHandler)>>,
    next_id: u64,
}

/// In-process domain event bus. Cheap to clone; clones share state.
#[derive(Default, Clone)]
pub struct EventBus {
    inner: Arc<Mutex<Inner>>,
}

impl EventBus {
    /// A new, empty bus.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `handler` on every [`Self::publish`] of an event of type
    /// `E`. The returned [`SubscriberId`] goes to
    /// [`Self::unsubscribe`] when you want to stop.
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

    /// Remove a subscriber. Does nothing if `id` is unknown.
    pub async fn unsubscribe(&self, id: SubscriberId) {
        let mut inner = self.inner.lock().await;
        for bag in inner.bags.values_mut() {
            bag.retain(|(sid, _)| *sid != id);
        }
    }

    /// Run every subscriber for type `E`, one at a time. Subscribers
    /// for other types are not called.
    pub async fn publish<E>(&self, event: E)
    where
        E: Clone + Send + Sync + 'static,
    {
        // Copy the handler list, then drop the lock, so a subscriber
        // can call `publish` or `subscribe` without a deadlock. A
        // subscriber added during dispatch misses this event.
        let handlers: Vec<AnyHandler> = {
            let inner = self.inner.lock().await;
            inner
                .bags
                .get(&TypeId::of::<E>())
                .map(|bag| bag.iter().map(|(_, h)| h.clone()).collect())
                .unwrap_or_default()
        };
        for any in handlers {
            // The downcast always succeeds: handlers are only ever
            // stored under their own TypeId.
            if let Ok(wrapper) = any.downcast::<TypedHandler<E>>() {
                let fut = (wrapper.f)(event.clone());
                fut.await;
            }
        }
    }

    /// How many subscribers are registered for type `E`.
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
    #[allow(dead_code)] // the payload is never read; the test checks routing by type.
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
        // Record the order subscribers run in, not just a count.
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
        // Nothing happens, and nothing panics.
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
        // Publish from the other handle: same state.
        bus2.publish(PingEvent(0)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
}
