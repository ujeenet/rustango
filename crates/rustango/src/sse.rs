//! A broadcast bus that sends one message to every subscriber. Use
//! it for Server-Sent Events and other push patterns.
//!
//! It wraps `tokio::sync::broadcast`. The bus knows nothing about
//! the transport, so pair it with `axum::response::sse::Sse` for
//! SSE, or use it for WebSocket fan-out.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::sse::EventBus;
//! use serde::{Serialize, Deserialize};
//!
//! #[derive(Clone, Serialize, Deserialize)]
//! struct Notification { message: String }
//!
//! // At startup
//! let bus: EventBus<Notification> = EventBus::new(100);
//!
//! // From any handler / signal — fan-out is fire-and-forget
//! bus.send(Notification { message: "Welcome".into() });
//!
//! // SSE endpoint (in your handler):
//! //
//! //   let mut rx = bus.subscribe();
//! //   let stream = async_stream::stream! {
//! //       loop {
//! //           let event = match rx.recv().await {
//! //               Ok(event) => event,
//! //               // A slow client skipped messages; keep it connected.
//! //               Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
//! //               Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
//! //           };
//! //           let json = serde_json::to_string(&event).unwrap_or_default();
//! //           yield Ok::<_, std::convert::Infallible>(
//! //               axum::response::sse::Event::default().data(json)
//! //           );
//! //       }
//! //   };
//! //   // `shutdown: rustango::shutdown::Shutdown` is an extractor; the
//! //   // stream then ends when the server stops instead of at its deadline.
//! //   axum::response::sse::Sse::new(shutdown.cut(stream))
//! //       .keep_alive(KeepAlive::new())
//! ```
//!
//! rustango does not depend on `async-stream` or `futures`, so add
//! them to your own Cargo.toml when you write the SSE handler.
//!
//! ## Per-user events and other processes
//!
//! [`KeyedEventBus`](crate::sse::KeyedEventBus) delivers an event only to the subscribers of its key,
//! such as a user or an account id. [`PgEventBus`](crate::sse::PgEventBus) (feature `postgres`)
//! is the same bus across processes, over PostgreSQL `LISTEN`/`NOTIFY`.
//!
//! **Lag contract.** Both hand out a [`Subscription`](crate::sse::Subscription). Its
//! [`Subscription::recv`](crate::sse::Subscription::recv) gives [`Recv::Resync`](crate::sse::Recv::Resync) when events were missed:
//! the subscriber fell behind the buffer, or the database connection
//! dropped. Reload what the client shows (send it an SSE `resync` event),
//! then keep reading. No event is replayed.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Weak};

use tokio::sync::{broadcast, watch};

/// Sends one message to every subscriber.
///
/// A slow subscriber that falls behind the buffer gets a
/// `RecvError::Lagged` and misses messages. It never blocks the
/// others.
pub struct EventBus<T: Clone + Send + 'static> {
    tx: Arc<broadcast::Sender<T>>,
}

impl<T: Clone + Send + 'static> EventBus<T> {
    /// New bus with this buffer size. A bigger buffer tolerates
    /// slower subscribers but uses more memory. The minimum is 1.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity.max(1));
        Self { tx: Arc::new(tx) }
    }

    /// Send a message to every subscriber and return how many got
    /// it. Zero is normal when nobody is connected.
    pub fn send(&self, event: T) -> usize {
        self.tx.send(event).unwrap_or(0)
    }

    /// How many subscribers are connected.
    #[must_use]
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Subscribe to the bus. Drop the receiver to disconnect.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<T> {
        self.tx.subscribe()
    }
}

impl<T: Clone + Send + 'static> Clone for EventBus<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

// ------------------------------------------------------------------ keyed

/// What [`Subscription::recv`] gives.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Recv<T> {
    Event(T),
    /// Events were missed: reload the state they would have changed,
    /// then read on.
    Resync,
}

/// One subscriber's stream of events. Drop it to unsubscribe.
pub struct Subscription<T: Clone + Send + 'static> {
    rx: Option<broadcast::Receiver<T>>,
    /// Bumped each time the cross-process feed reconnects.
    resync: Option<watch::Receiver<u64>>,
    unsubscribe: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl<T: Clone + Send + 'static> Subscription<T> {
    /// The next event, [`Recv::Resync`] after missed ones, or `None` once
    /// the bus is gone.
    pub async fn recv(&mut self) -> Option<Recv<T>> {
        let rx = self.rx.as_mut()?;
        let got = match &mut self.resync {
            Some(resync) => tokio::select! {
                biased;
                changed = resync.changed() => return changed.ok().map(|()| Recv::Resync),
                got = rx.recv() => got,
            },
            None => rx.recv().await,
        };
        match got {
            Ok(event) => Some(Recv::Event(event)),
            Err(broadcast::error::RecvError::Lagged(_)) => Some(Recv::Resync),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }
}

impl<T: Clone + Send + 'static> Drop for Subscription<T> {
    fn drop(&mut self) {
        // The receiver goes first, so the topic sees it gone.
        drop(self.rx.take());
        if let Some(unsubscribe) = self.unsubscribe.take() {
            unsubscribe();
        }
    }
}

struct Topics<K, T> {
    capacity: usize,
    by_key: std::sync::Mutex<HashMap<K, broadcast::Sender<T>>>,
}

impl<K, T> Topics<K, T> {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<K, broadcast::Sender<T>>> {
        self.by_key
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// An [`EventBus`] with one topic per key: an event reaches only the
/// subscribers of its key. A topic exists while someone subscribes to it.
pub struct KeyedEventBus<K, T> {
    topics: Arc<Topics<K, T>>,
}

impl<K, T> Clone for KeyedEventBus<K, T> {
    fn clone(&self) -> Self {
        Self {
            topics: self.topics.clone(),
        }
    }
}

impl<K, T> KeyedEventBus<K, T>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    T: Clone + Send + 'static,
{
    /// `capacity` is each topic's buffer, at least 1. A subscriber that
    /// falls further behind gets [`Recv::Resync`].
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            topics: Arc::new(Topics {
                capacity: capacity.max(1),
                by_key: std::sync::Mutex::default(),
            }),
        }
    }

    /// Send `event` to `key`'s subscribers; returns how many got it.
    pub fn send(&self, key: &K, event: T) -> usize {
        self.topics
            .lock()
            .get(key)
            .map_or(0, |tx| tx.send(event).unwrap_or(0))
    }

    /// Subscribe to `key`'s events.
    #[must_use]
    pub fn subscribe(&self, key: K) -> Subscription<T> {
        let rx = {
            let mut by_key = self.topics.lock();
            let capacity = self.topics.capacity;
            by_key
                .entry(key.clone())
                .or_insert_with(|| broadcast::channel(capacity).0)
                .subscribe()
        };
        // Weak: a subscription must not keep a dropped bus open.
        let topics: Weak<Topics<K, T>> = Arc::downgrade(&self.topics);
        Subscription {
            rx: Some(rx),
            resync: None,
            unsubscribe: Some(Box::new(move || {
                if let Some(topics) = topics.upgrade() {
                    let mut by_key = topics.lock();
                    if by_key.get(&key).is_some_and(|tx| tx.receiver_count() == 0) {
                        by_key.remove(&key);
                    }
                }
            })),
        }
    }

    /// How many subscribe to `key`.
    #[must_use]
    pub fn receiver_count(&self, key: &K) -> usize {
        self.topics
            .lock()
            .get(key)
            .map_or(0, broadcast::Sender::receiver_count)
    }

    /// How many keys have a subscriber.
    #[must_use]
    pub fn topic_count(&self) -> usize {
        self.topics.lock().len()
    }
}

// ------------------------------------------------------------------ postgres

#[cfg(feature = "postgres")]
pub use pg::{BusError, PgEventBus};

#[cfg(feature = "postgres")]
mod pg {
    use std::hash::Hash;
    use std::sync::Arc;
    use std::time::Duration;

    use serde::{de::DeserializeOwned, Deserialize, Serialize};
    use sqlx::postgres::PgListener;
    use tokio::sync::watch;

    use super::{KeyedEventBus, Subscription};
    use crate::sql::Pool;

    /// The largest `NOTIFY` payload PostgreSQL takes, in bytes.
    const MAX_PAYLOAD: usize = 7999;

    /// Why a [`PgEventBus`] call failed.
    #[derive(Debug, thiserror::Error)]
    #[non_exhaustive]
    pub enum BusError {
        #[error("PgEventBus needs a PostgreSQL pool")]
        NotPostgres,
        #[error("invalid channel `{0}`: 1 to 63 bytes")]
        Channel(String),
        #[error("event of {0} bytes is over the {MAX_PAYLOAD}-byte NOTIFY limit")]
        TooLarge(usize),
        #[error("event encoding: {0}")]
        Encode(String),
        #[error("database: {0}")]
        Db(String),
    }

    /// A [`KeyedEventBus`] across processes: [`Self::publish`] sends a
    /// `NOTIFY`, and every process's bus on the same channel delivers it
    /// to its local subscribers, the publisher's included.
    ///
    /// `LISTEN` and `NOTIFY` need no privilege, so the wire channel is an
    /// HMAC of the name and the pool's schema under a shared secret: SQL
    /// that cannot read the secret, a tenant role's included, can neither
    /// listen in nor inject, and each schema gets its own channel.
    pub struct PgEventBus<K, T> {
        local: KeyedEventBus<K, T>,
        pool: Pool,
        channel: Arc<str>,
        resync: watch::Receiver<u64>,
        listener_task: Arc<AbortOnDrop>,
    }

    impl<K, T> Clone for PgEventBus<K, T> {
        fn clone(&self) -> Self {
            Self {
                local: self.local.clone(),
                pool: self.pool.clone(),
                channel: self.channel.clone(),
                resync: self.resync.clone(),
                listener_task: self.listener_task.clone(),
            }
        }
    }

    struct AbortOnDrop(tokio::task::JoinHandle<()>);

    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    #[derive(Serialize)]
    struct Outgoing<'a, K, T> {
        k: &'a K,
        e: &'a T,
    }

    #[derive(Deserialize)]
    struct Incoming<K, T> {
        k: K,
        e: T,
    }

    /// The channel `LISTEN`/`NOTIFY` use for `name` on `scope`'s schema.
    fn wire_channel(secret: &crate::session::SessionSecret, scope: &str, name: &str) -> String {
        let mac = crate::session::sign_tagged(
            secret,
            b"rustango.event-bus.v1",
            format!("{scope}\0{name}").as_bytes(),
        );
        format!("rgo_ev_{}", &crate::hex::hex_encode(&mac)[..40])
    }

    impl<K, T> PgEventBus<K, T>
    where
        K: Serialize + DeserializeOwned + Eq + Hash + Clone + Send + Sync + 'static,
        T: Serialize + DeserializeOwned + Clone + Send + 'static,
    {
        /// `LISTEN` on `channel` and start delivering. `capacity` is each
        /// key's buffer, as for [`KeyedEventBus::new`]. Every process passes
        /// the same `secret`; buses on different secrets never meet.
        ///
        /// # Errors
        /// [`BusError::NotPostgres`], [`BusError::Channel`], or the
        /// connection failing.
        pub async fn connect(
            pool: &Pool,
            channel: &str,
            capacity: usize,
            secret: &crate::session::SessionSecret,
        ) -> Result<Self, BusError> {
            let pg = pool.as_postgres().ok_or(BusError::NotPostgres)?.clone();
            if channel.is_empty() || channel.len() > 63 {
                return Err(BusError::Channel(channel.to_owned()));
            }
            let wire = wire_channel(secret, &Pool::pg_schema_scope(&pg), channel);
            let listener = listen(&pg, &wire).await?;
            let local = KeyedEventBus::new(capacity);
            let (bump, resync) = watch::channel(0);
            let feed = feed::<K, T>(pg, wire.clone(), local.clone(), bump, listener);
            Ok(Self {
                local,
                pool: pool.clone(),
                channel: wire.into(),
                resync,
                listener_task: Arc::new(AbortOnDrop(tokio::spawn(feed))),
            })
        }

        /// The channel name `LISTEN` and `NOTIFY` use on the wire.
        #[must_use]
        pub fn wire_channel(&self) -> &str {
            &self.channel
        }

        /// Send `event` to `key`'s subscribers in every process. Inside an
        /// `atomic` block on this pool it is sent at the commit, and not
        /// at all on a rollback.
        ///
        /// # Errors
        /// [`BusError::TooLarge`] past PostgreSQL's 8000-byte payload, or
        /// the database failing.
        pub async fn publish(&self, key: &K, event: &T) -> Result<(), BusError> {
            use crate::core::SqlValue;
            let out = Outgoing { k: key, e: event };
            let payload =
                serde_json::to_string(&out).map_err(|e| BusError::Encode(e.to_string()))?;
            if payload.len() > MAX_PAYLOAD {
                return Err(BusError::TooLarge(payload.len()));
            }
            let binds = vec![
                SqlValue::String(self.channel.to_string()),
                SqlValue::String(payload),
            ];
            crate::sql::raw_write(&self.pool, "SELECT pg_notify($1, $2)", binds)
                .await
                .map_err(|e| BusError::Db(e.to_string()))?;
            Ok(())
        }

        /// Subscribe to `key`'s events from every process.
        #[must_use]
        pub fn subscribe(&self, key: K) -> Subscription<T> {
            let mut sub = self.local.subscribe(key);
            let mut resync = self.resync.clone();
            resync.mark_unchanged();
            sub.resync = Some(resync);
            sub
        }

        /// How many in this process subscribe to `key`.
        #[must_use]
        pub fn receiver_count(&self, key: &K) -> usize {
            self.local.receiver_count(key)
        }
    }

    async fn listen(pg: &sqlx::PgPool, channel: &str) -> Result<PgListener, BusError> {
        let mut listener = PgListener::connect_with(pg)
            .await
            .map_err(|e| BusError::Db(e.to_string()))?;
        listener
            .listen(channel)
            .await
            .map_err(|e| BusError::Db(e.to_string()))?;
        Ok(listener)
    }

    /// Deliver each notification locally. A lost connection is
    /// re-opened, then every subscriber is told to resync.
    async fn feed<K, T>(
        pg: sqlx::PgPool,
        channel: String,
        local: KeyedEventBus<K, T>,
        bump: watch::Sender<u64>,
        mut listener: PgListener,
    ) where
        K: DeserializeOwned + Eq + Hash + Clone + Send + Sync + 'static,
        T: DeserializeOwned + Clone + Send + 'static,
    {
        loop {
            match listener.try_recv().await {
                Ok(Some(note)) => match serde_json::from_str::<Incoming<K, T>>(note.payload()) {
                    Ok(msg) => {
                        local.send(&msg.k, msg.e);
                    }
                    Err(e) => tracing::warn!(channel, error = %e, "undecodable event; dropped"),
                },
                Ok(None) | Err(_) => {
                    tracing::warn!(channel, "event bus connection lost; reconnecting");
                    let mut wait = Duration::from_millis(100);
                    listener = loop {
                        match listen(&pg, &channel).await {
                            Ok(l) => break l,
                            Err(e) => {
                                tracing::warn!(channel, error = %e, "event bus reconnect failed");
                                tokio::time::sleep(wait).await;
                                wait = (wait * 2).min(Duration::from_secs(10));
                            }
                        }
                    };
                    bump.send_modify(|n| *n += 1);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
    struct TestEvent {
        kind: String,
        value: i32,
    }

    #[tokio::test]
    async fn fresh_bus_has_no_subscribers() {
        let bus: EventBus<TestEvent> = EventBus::new(10);
        assert_eq!(bus.receiver_count(), 0);
    }

    #[tokio::test]
    async fn subscribe_increments_count() {
        let bus: EventBus<TestEvent> = EventBus::new(10);
        let _rx1 = bus.subscribe();
        assert_eq!(bus.receiver_count(), 1);
        let _rx2 = bus.subscribe();
        assert_eq!(bus.receiver_count(), 2);
    }

    #[tokio::test]
    async fn dropped_receiver_decrements_count() {
        let bus: EventBus<TestEvent> = EventBus::new(10);
        let rx = bus.subscribe();
        assert_eq!(bus.receiver_count(), 1);
        drop(rx);
        assert_eq!(bus.receiver_count(), 0);
    }

    #[tokio::test]
    async fn send_with_no_subscribers_returns_zero() {
        let bus: EventBus<TestEvent> = EventBus::new(10);
        let n = bus.send(TestEvent {
            kind: "x".into(),
            value: 1,
        });
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn send_with_subscribers_returns_count() {
        let bus: EventBus<TestEvent> = EventBus::new(10);
        let _rx1 = bus.subscribe();
        let _rx2 = bus.subscribe();
        let n = bus.send(TestEvent {
            kind: "x".into(),
            value: 1,
        });
        assert_eq!(n, 2);
    }

    #[tokio::test]
    async fn message_received_by_subscriber() {
        let bus: EventBus<TestEvent> = EventBus::new(10);
        let mut rx = bus.subscribe();
        let event = TestEvent {
            kind: "ping".into(),
            value: 42,
        };
        bus.send(event.clone());
        let received = rx.recv().await.unwrap();
        assert_eq!(received, event);
    }

    #[tokio::test]
    async fn clone_shares_bus() {
        let bus: EventBus<TestEvent> = EventBus::new(10);
        let bus_clone = bus.clone();
        let mut rx = bus.subscribe();
        bus_clone.send(TestEvent {
            kind: "via_clone".into(),
            value: 1,
        });
        let received = rx.recv().await.unwrap();
        assert_eq!(received.kind, "via_clone");
    }

    #[tokio::test]
    async fn capacity_floor_at_1() {
        let _bus: EventBus<TestEvent> = EventBus::new(0); // must not panic
    }

    #[tokio::test]
    async fn message_fan_out_to_all_subscribers() {
        let bus: EventBus<TestEvent> = EventBus::new(10);
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();
        let mut rx3 = bus.subscribe();
        bus.send(TestEvent {
            kind: "broadcast".into(),
            value: 99,
        });
        for rx in [&mut rx1, &mut rx2, &mut rx3].iter_mut() {
            let received = rx.recv().await.unwrap();
            assert_eq!(received.value, 99);
        }
    }

    #[tokio::test]
    async fn a_keyed_event_reaches_only_its_keys_subscribers() {
        let bus: KeyedEventBus<u64, i32> = KeyedEventBus::new(8);
        let (mut alice, mut bob) = (bus.subscribe(1), bus.subscribe(2));
        assert_eq!(bus.send(&1, 10), 1);
        assert_eq!(bus.send(&3, 30), 0, "nobody listens on 3");
        assert_eq!(alice.recv().await, Some(Recv::Event(10)));
        bus.send(&2, 20);
        assert_eq!(bob.recv().await, Some(Recv::Event(20)));
        assert_eq!(bus.topic_count(), 2, "sending to 3 made no topic");
    }

    /// The last subscriber's drop removes its topic.
    #[tokio::test]
    async fn the_last_unsubscribe_removes_the_topic() {
        let bus: KeyedEventBus<&'static str, i32> = KeyedEventBus::new(8);
        let (a, b) = (bus.subscribe("k"), bus.subscribe("k"));
        assert_eq!(bus.receiver_count(&"k"), 2);
        drop(a);
        assert_eq!(bus.topic_count(), 1);
        drop(b);
        assert_eq!(bus.topic_count(), 0);
    }

    /// A subscriber past the buffer gets `Resync`, then the newest events.
    #[tokio::test]
    async fn a_lagging_subscriber_is_told_to_resync() {
        let bus: KeyedEventBus<u8, i32> = KeyedEventBus::new(2);
        let mut sub = bus.subscribe(0);
        for n in 0..5 {
            bus.send(&0, n);
        }
        assert_eq!(sub.recv().await, Some(Recv::Resync));
        assert_eq!(sub.recv().await, Some(Recv::Event(3)));
    }

    /// A dropped bus ends its subscriptions.
    #[tokio::test]
    async fn a_dropped_bus_ends_its_subscriptions() {
        let bus: KeyedEventBus<u8, i32> = KeyedEventBus::new(2);
        let mut sub = bus.subscribe(0);
        drop(bus);
        assert_eq!(sub.recv().await, None);
    }
}
