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
//! //       while let Ok(event) = rx.recv().await {
//! //           let json = serde_json::to_string(&event).unwrap_or_default();
//! //           yield Ok::<_, std::convert::Infallible>(
//! //               axum::response::sse::Event::default().data(json)
//! //           );
//! //       }
//! //   };
//! //   axum::response::sse::Sse::new(stream)
//! //       .keep_alive(KeepAlive::new())
//! ```
//!
//! rustango does not depend on `async-stream` or `futures`, so add
//! them to your own Cargo.toml when you write the SSE handler.

use std::sync::Arc;

use tokio::sync::broadcast;

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
}
