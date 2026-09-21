//! WebSocket handler built on the SSE [`EventBus`].
//!
//! axum gives you the upgrade; this module adds what a server needs on
//! top of it:
//!
//! - **Fan-out**: every connected client gets every message sent to the
//!   [`crate::sse::EventBus`].
//! - **JSON**: messages are `Serialize` + `Deserialize` types, encoded
//!   and decoded for you.
//! - **Keep-alive**: a ping interval stops proxies from closing idle
//!   connections.
//! - **Slow clients**: a lagging client gets a `{"_lagged":n}` message
//!   instead of being dropped, so it can resync.
//!
//! The upgrade itself is yours to guard: axum does not check the
//! `Origin` header, so a page on any site can open a socket to you.
//! Check `Origin` (and authenticate the user) before calling
//! `on_upgrade`.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::sse::EventBus;
//! use rustango::ws::{ws_handler, WsHub};
//! use axum::{Router, extract::WebSocketUpgrade, response::Response};
//! use serde::{Serialize, Deserialize};
//!
//! #[derive(Clone, Serialize, Deserialize)]
//! struct Tick { value: i64 }
//!
//! let bus: EventBus<Tick> = EventBus::new(100);
//! let hub = WsHub::new(bus);
//!
//! async fn ws_route(
//!     ws: WebSocketUpgrade,
//!     State(hub): State<WsHub<Tick>>,
//! ) -> Response {
//!     ws.on_upgrade(move |socket| ws_handler(socket, hub.clone()))
//! }
//!
//! let app = Router::new().route("/ws", get(ws_route)).with_state(hub);
//!
//! // From anywhere — fire one message at every connected client:
//! hub.broadcast(Tick { value: 42 });
//! ```
//!
//! [`EventBus`]: crate::sse::EventBus

use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;

use crate::sse::EventBus;

/// Fans messages out to every connected WebSocket. Cheap to clone.
#[derive(Clone)]
pub struct WsHub<T: Clone + Send + 'static> {
    bus: EventBus<T>,
    config: WsConfig,
}

/// Per-connection tuning knobs.
#[derive(Clone, Debug)]
pub struct WsConfig {
    /// How often to send a `Ping` frame when the connection is idle.
    /// Default: 30 seconds.
    pub keepalive: Duration,
    /// Called for every text message from the client. Return
    /// `Some(reply)` to answer, `None` to ignore. Default: `None`,
    /// meaning server-push only.
    pub on_message: Option<fn(&str) -> Option<String>>,
    /// Close the connection if a message is larger than this.
    /// Default: 1 MiB.
    pub max_message_bytes: usize,
}

impl Default for WsConfig {
    fn default() -> Self {
        Self {
            keepalive: Duration::from_secs(30),
            on_message: None,
            max_message_bytes: 1024 * 1024,
        }
    }
}

impl<T: Clone + Send + Serialize + 'static> WsHub<T> {
    #[must_use]
    pub fn new(bus: EventBus<T>) -> Self {
        Self {
            bus,
            config: WsConfig::default(),
        }
    }

    #[must_use]
    pub fn with_config(bus: EventBus<T>, config: WsConfig) -> Self {
        Self { bus, config }
    }

    /// Set the keepalive interval. Shorter spots dead connections
    /// sooner; longer means less traffic. Default: 30 seconds.
    #[must_use]
    pub fn keepalive(mut self, interval: Duration) -> Self {
        self.config.keepalive = interval;
        self
    }

    /// Set the text-message handler. A `Some(reply)` goes back to that
    /// one client only. For fan-out, call [`Self::broadcast`].
    #[must_use]
    pub fn on_message(mut self, f: fn(&str) -> Option<String>) -> Self {
        self.config.on_message = Some(f);
        self
    }

    #[must_use]
    pub fn max_message_bytes(mut self, n: usize) -> Self {
        self.config.max_message_bytes = n;
        self
    }

    /// Send `event` to every connected client and return how many
    /// receivers saw it. With no clients it does nothing.
    pub fn broadcast(&self, event: T) -> usize {
        self.bus.send(event)
    }

    #[must_use]
    pub fn receiver_count(&self) -> usize {
        self.bus.receiver_count()
    }

    /// Borrow the [`EventBus`], for example to share it with an SSE
    /// handler.
    #[must_use]
    pub fn bus(&self) -> &EventBus<T> {
        &self.bus
    }
}

/// Drive one connected WebSocket. Spawn it from your axum handler:
///
/// ```ignore
/// ws.on_upgrade(move |socket| ws_handler(socket, hub.clone()))
/// ```
///
/// It returns when the client disconnects, a ping fails, or the bus
/// closes.
pub async fn ws_handler<T>(mut socket: WebSocket, hub: WsHub<T>)
where
    T: Clone + Send + Serialize + DeserializeOwned + 'static,
{
    let mut rx = hub.bus.subscribe();
    let keepalive = hub.config.keepalive;
    let on_message = hub.config.on_message;
    let max_bytes = hub.config.max_message_bytes;

    loop {
        tokio::select! {
            // Outbound: fan-out from the bus.
            recv = rx.recv() => match recv {
                Ok(event) => {
                    let json = match serde_json::to_string(&event) {
                        Ok(j) => j,
                        Err(e) => {
                            tracing::warn!(error = %e, "ws: serialize event");
                            continue;
                        }
                    };
                    if socket.send(Message::Text(json.into())).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    // Tell the client it missed `n` events so it can resync.
                    let _ = socket
                        .send(Message::Text(
                            format!(r#"{{"_lagged":{n}}}"#).into(),
                        ))
                        .await;
                }
                Err(RecvError::Closed) => return,
            },

            // Inbound: read the socket so disconnects show up quickly
            // and `on_message` can run.
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(t))) => {
                    if t.len() > max_bytes {
                        let _ = socket.send(Message::Close(None)).await;
                        return;
                    }
                    if let Some(handler) = on_message {
                        if let Some(reply) = handler(t.as_str()) {
                            if socket.send(Message::Text(reply.into())).await.is_err() {
                                return;
                            }
                        }
                    }
                }
                Some(Ok(Message::Binary(b))) => {
                    if b.len() > max_bytes {
                        let _ = socket.send(Message::Close(None)).await;
                        return;
                    }
                    // Binary frames are dropped; wrap ws_handler to handle them.
                }
                Some(Ok(Message::Ping(p))) => {
                    if socket.send(Message::Pong(p)).await.is_err() {
                        return;
                    }
                }
                Some(Ok(Message::Pong(_))) => {
                    // Client responded to our keepalive ping.
                }
                Some(Ok(Message::Close(_))) | None => return,
                Some(Err(e)) => {
                    tracing::debug!(error = %e, "ws: recv error, closing");
                    return;
                }
            },

            // Keepalive: send a ping every `keepalive`.
            () = tokio::time::sleep(keepalive) => {
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
    struct Tick {
        value: i64,
    }

    #[tokio::test]
    async fn hub_broadcast_returns_zero_when_no_clients() {
        let bus: EventBus<Tick> = EventBus::new(10);
        let hub = WsHub::new(bus);
        assert_eq!(hub.broadcast(Tick { value: 1 }), 0);
        assert_eq!(hub.receiver_count(), 0);
    }

    #[tokio::test]
    async fn hub_broadcast_reaches_subscribers() {
        let bus: EventBus<Tick> = EventBus::new(10);
        let hub = WsHub::new(bus);
        let mut rx = hub.bus().subscribe();
        let n = hub.broadcast(Tick { value: 99 });
        assert_eq!(n, 1, "should reach 1 subscriber");
        let event = rx.recv().await.unwrap();
        assert_eq!(event, Tick { value: 99 });
    }

    #[tokio::test]
    async fn hub_clone_shares_bus() {
        let bus: EventBus<Tick> = EventBus::new(10);
        let hub = WsHub::new(bus);
        let cloned = hub.clone();
        let mut rx = hub.bus().subscribe();
        cloned.broadcast(Tick { value: 7 });
        assert_eq!(rx.recv().await.unwrap().value, 7);
    }

    #[tokio::test]
    async fn config_defaults() {
        let cfg = WsConfig::default();
        assert_eq!(cfg.keepalive, Duration::from_secs(30));
        assert!(cfg.on_message.is_none());
        assert_eq!(cfg.max_message_bytes, 1024 * 1024);
    }

    #[tokio::test]
    async fn keepalive_builder_overrides() {
        let bus: EventBus<Tick> = EventBus::new(10);
        let hub = WsHub::new(bus).keepalive(Duration::from_secs(5));
        assert_eq!(hub.config.keepalive, Duration::from_secs(5));
    }

    #[tokio::test]
    async fn on_message_builder_sets_handler() {
        fn echo(s: &str) -> Option<String> {
            Some(s.to_owned())
        }
        let bus: EventBus<Tick> = EventBus::new(10);
        let hub = WsHub::new(bus).on_message(echo);
        assert!(hub.config.on_message.is_some());
        // Verify the function pointer round-trips.
        let h = hub.config.on_message.unwrap();
        assert_eq!(h("hi").as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn max_message_bytes_builder_sets_limit() {
        let bus: EventBus<Tick> = EventBus::new(10);
        let hub = WsHub::new(bus).max_message_bytes(2048);
        assert_eq!(hub.config.max_message_bytes, 2048);
    }

    #[tokio::test]
    async fn lagged_subscriber_sees_lagged_error() {
        // The lag behaviour the handler relies on.
        let bus: EventBus<Tick> = EventBus::new(2);
        let mut rx = bus.subscribe();
        // Fill past capacity so the subscriber lags.
        for i in 0..10 {
            bus.send(Tick { value: i });
        }
        match rx.recv().await {
            Err(RecvError::Lagged(n)) => assert!(n > 0),
            other => panic!("expected Lagged, got {other:?}"),
        }
    }
}
