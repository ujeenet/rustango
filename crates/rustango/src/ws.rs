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
//! - **Shutdown**: a stopping server sends Close 1001 (going away).
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
//! use rustango::shutdown::Shutdown;
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
//!     shutdown: Shutdown,
//!     State(hub): State<WsHub<Tick>>,
//! ) -> Response {
//!     // Applies `max_message_bytes` to the frame reader too.
//!     hub.upgrade(ws, shutdown)
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

use axum::extract::ws::{close_code, CloseFrame, Message, WebSocket};
use futures_util::FutureExt as _;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;

use crate::shutdown::Shutdown;
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
    /// How often to send a `Ping` frame, busy or idle. A peer silent for a
    /// whole interval after a ping is closed. A send may take the larger of
    /// this and 10 seconds. Default: 30 seconds.
    pub keepalive: Duration,
    /// Called for every text message from the client. Return
    /// `Some(reply)` to answer, `None` to ignore. Default: `None`,
    /// meaning server-push only.
    pub on_message: Option<fn(&str) -> Option<String>>,
    /// Close the connection if a message is larger than this.
    /// Default: 1 MiB. Only [`WsHub::upgrade`] enforces it before the
    /// message is buffered; a bare `on_upgrade` checks it afterwards.
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

    /// Upgrade with the frame and message reader capped at
    /// `max_message_bytes`, then run [`ws_handler`]. A bare
    /// `on_upgrade` reads up to 64 MiB before the size check (#1957).
    #[must_use = "the upgrade response must be returned"]
    pub fn upgrade(
        &self,
        ws: axum::extract::WebSocketUpgrade,
        shutdown: Shutdown,
    ) -> axum::response::Response
    where
        T: DeserializeOwned,
    {
        let hub = self.clone();
        let max = self.config.max_message_bytes;
        ws.max_message_size(max)
            .max_frame_size(max)
            .on_upgrade(move |socket| ws_handler(socket, hub, shutdown))
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

/// Drive one connected WebSocket. [`WsHub::upgrade`] calls it with
/// the size cap applied; call it yourself only if you set that cap:
///
/// ```ignore
/// ws.max_message_size(n).on_upgrade(move |socket| ws_handler(socket, hub.clone(), shutdown))
/// ```
///
/// A ping goes out every `keepalive`, traffic or not. It returns when the
/// client disconnects, sends nothing for a whole interval after a ping,
/// a send outlasts `max(keepalive, 10 s)`, or the bus closes. At
/// `shutdown` it sends Close 1001 first.
pub async fn ws_handler<T>(mut socket: WebSocket, hub: WsHub<T>, shutdown: Shutdown)
where
    T: Clone + Send + Serialize + DeserializeOwned + 'static,
{
    let mut rx = hub.bus.subscribe();
    // `interval` panics on zero.
    let keepalive = hub.config.keepalive.max(Duration::from_millis(1));
    let send_limit = send_timeout(keepalive);
    let on_message = hub.config.on_message;
    let max_bytes = hub.config.max_message_bytes;
    // Not reset by traffic, so a busy socket is pinged too.
    let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + keepalive, keepalive);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut awaiting_reply = false;
    let stopped = shutdown.stopped();
    tokio::pin!(stopped);

    loop {
        let step = tokio::select! {
            () = &mut stopped => Step::Stop,
            recv = rx.recv() => Step::Bus(recv),
            incoming = socket.recv() => Step::Peer(incoming),
            // `select!` picks among ready branches at random, so a reply
            // queued behind a long send may lose to the overdue tick: read it first.
            _ = ticks.tick() => match awaiting_reply.then(|| socket.recv().now_or_never()).flatten() {
                Some(incoming) => Step::Peer(incoming),
                None => Step::Tick,
            },
        };
        match step {
            // The server is stopping: say so, so the client reconnects elsewhere.
            Step::Stop => {
                let away = CloseFrame {
                    code: close_code::AWAY,
                    reason: "server shutting down".into(),
                };
                let _ = send(&mut socket, Message::Close(Some(away)), send_limit).await;
                return;
            }

            // Outbound: fan-out from the bus.
            Step::Bus(recv) => match recv {
                Ok(event) => {
                    let json = match serde_json::to_string(&event) {
                        Ok(j) => j,
                        Err(e) => {
                            tracing::warn!(error = %e, "ws: serialize event");
                            continue;
                        }
                    };
                    if !send(&mut socket, Message::Text(json.into()), send_limit).await {
                        return;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    // Tell the client it missed `n` events so it can resync.
                    let lagged = Message::Text(format!(r#"{{"_lagged":{n}}}"#).into());
                    if !send(&mut socket, lagged, send_limit).await {
                        return;
                    }
                }
                Err(RecvError::Closed) => return,
            },

            // Inbound: read the socket so disconnects show up quickly
            // and `on_message` can run. Any frame proves the peer is alive.
            Step::Peer(incoming) => {
                if matches!(incoming, Some(Ok(_))) {
                    awaiting_reply = false;
                }
                match incoming {
                    Some(Ok(Message::Text(t))) => {
                        if t.len() > max_bytes {
                            let _ = send(&mut socket, Message::Close(None), send_limit).await;
                            return;
                        }
                        if let Some(handler) = on_message {
                            if let Some(reply) = handler(t.as_str()) {
                                if !send(&mut socket, Message::Text(reply.into()), send_limit).await
                                {
                                    return;
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Binary(b))) => {
                        if b.len() > max_bytes {
                            let _ = send(&mut socket, Message::Close(None), send_limit).await;
                            return;
                        }
                        // Binary frames are dropped; wrap ws_handler to handle them.
                    }
                    Some(Ok(Message::Ping(p))) => {
                        if !send(&mut socket, Message::Pong(p), send_limit).await {
                            return;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Err(e)) => {
                        tracing::debug!(error = %e, "ws: recv error, closing");
                        return;
                    }
                }
            }

            // Keepalive: no answer to the last ping in a whole interval
            // means a half-open peer.
            Step::Tick => {
                if awaiting_reply {
                    tracing::debug!("ws: no pong within keepalive, closing");
                    return;
                }
                if !send(&mut socket, Message::Ping(Vec::new().into()), send_limit).await {
                    return;
                }
                awaiting_reply = true;
            }
        }
    }
}

/// What the connection loop does next.
enum Step<T> {
    Stop,
    Bus(Result<T, RecvError>),
    Peer(Option<Result<Message, axum::Error>>),
    Tick,
}

/// Shortest time a send may take; a short keepalive must not drop slow readers.
const SEND_TIMEOUT_FLOOR: Duration = Duration::from_secs(10);

fn send_timeout(keepalive: Duration) -> Duration {
    keepalive.max(SEND_TIMEOUT_FLOOR)
}

/// Send `msg`, giving up after `limit` so a peer that stops reading
/// cannot hold the task. `false` means close.
async fn send(socket: &mut WebSocket, msg: Message, limit: Duration) -> bool {
    matches!(
        tokio::time::timeout(limit, socket.send(msg)).await,
        Ok(Ok(()))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
    struct Tick {
        value: i64,
    }

    /// `upgrade` caps the frame reader: a frame header announcing more
    /// than `max_message_bytes` closes the socket at once instead of
    /// waiting to buffer the payload (#1957).
    #[tokio::test]
    async fn upgrade_refuses_an_oversized_frame_before_buffering_it() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let hub = WsHub::new(EventBus::<Tick>::new(4)).max_message_bytes(1024);
        let app = axum::Router::new().route(
            "/ws",
            axum::routing::get(move |ws: axum::extract::WebSocketUpgrade| {
                let hub = hub.clone();
                async move { hub.upgrade(ws, Shutdown::default()) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        tcp.write_all(
            b"GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
              Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        )
        .await
        .unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut b = [0u8; 1];
            tcp.read_exact(&mut b).await.unwrap();
            head.push(b[0]);
        }
        assert!(
            head.starts_with(b"HTTP/1.1 101"),
            "{}",
            String::from_utf8_lossy(&head)
        );
        // Masked binary frame header claiming a 2 MiB payload; no payload follows.
        let mut frame = vec![0x82, 0x80 | 127];
        frame.extend_from_slice(&(2u64 * 1024 * 1024).to_be_bytes());
        frame.extend_from_slice(&[1, 2, 3, 4]);
        tcp.write_all(&frame).await.unwrap();

        let mut sink = Vec::new();
        let closed = tokio::time::timeout(Duration::from_secs(3), tcp.read_to_end(&mut sink)).await;
        assert!(
            closed.is_ok(),
            "the server kept waiting for the oversized payload"
        );
    }

    /// Serve `hub` on a loopback port and finish the client handshake.
    async fn open(hub: WsHub<String>) -> tokio::net::TcpStream {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let app = axum::Router::new().route(
            "/ws",
            axum::routing::get(move |ws: axum::extract::WebSocketUpgrade| {
                let hub = hub.clone();
                async move { hub.upgrade(ws, Shutdown::default()) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        tcp.write_all(
            b"GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
              Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        )
        .await
        .unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut b = [0u8; 1];
            tcp.read_exact(&mut b).await.unwrap();
            head.push(b[0]);
        }
        assert!(head.starts_with(b"HTTP/1.1 101"));
        tcp
    }

    /// A peer that never answers a ping is closed, not kept forever.
    #[tokio::test]
    async fn a_silent_peer_is_closed() {
        use tokio::io::AsyncReadExt as _;
        let hub = WsHub::new(EventBus::<String>::new(4)).keepalive(Duration::from_millis(100));
        let mut tcp = open(hub).await;
        let mut sink = Vec::new();
        let closed = tokio::time::timeout(Duration::from_secs(3), tcp.read_to_end(&mut sink)).await;
        assert!(closed.is_ok(), "a peer that never pongs was kept open");
    }

    /// Steady traffic does not push the ping back forever.
    #[tokio::test]
    async fn a_busy_socket_is_still_pinged() {
        use tokio::io::AsyncReadExt as _;
        let hub = WsHub::new(EventBus::<String>::new(64)).keepalive(Duration::from_millis(150));
        let mut tcp = open(hub.clone()).await;
        let ticker = tokio::spawn(async move {
            loop {
                hub.broadcast("x".into());
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let pinged = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let mut h = [0u8; 2];
                tcp.read_exact(&mut h).await.unwrap();
                let mut body = vec![0u8; usize::from(h[1] & 0x7f)];
                tcp.read_exact(&mut body).await.unwrap();
                if h[0] & 0x0f == 0x9 {
                    return;
                }
            }
        })
        .await;
        ticker.abort();
        assert!(pinged.is_ok(), "no ping while messages flowed");
    }

    /// A peer that keeps talking but stops reading is dropped once a send stalls.
    #[tokio::test]
    async fn a_peer_that_stops_reading_is_dropped() {
        use tokio::io::AsyncWriteExt as _;
        let hub = WsHub::new(EventBus::<String>::new(4)).keepalive(Duration::from_millis(200));
        let tcp = open(hub.clone()).await;
        let (_read, mut write) = tcp.into_split();
        // Masked empty pings keep the peer "alive" without reading anything.
        let talker = tokio::spawn(async move {
            while write.write_all(&[0x89, 0x80, 1, 2, 3, 4]).await.is_ok() {
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        });
        while hub.receiver_count() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let big = "y".repeat(64 * 1024);
        // The send floor is 10 s, so allow well past it.
        let dropped = tokio::time::timeout(Duration::from_secs(30), async {
            while hub.receiver_count() > 0 {
                hub.broadcast(big.clone());
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await;
        talker.abort();
        assert!(dropped.is_ok(), "a stalled send held the connection");
    }

    /// A short keepalive does not shorten the send timeout: a reader that
    /// pauses longer than keepalive but under the floor stays connected.
    #[tokio::test]
    async fn a_short_keepalive_keeps_a_slow_reader() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let hub = WsHub::new(EventBus::<String>::new(4)).keepalive(Duration::from_millis(100));
        let tcp = open(hub.clone()).await;
        let (mut read, mut write) = tcp.into_split();
        let talker = tokio::spawn(async move {
            while write.write_all(&[0x89, 0x80, 1, 2, 3, 4]).await.is_ok() {
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        });
        while hub.receiver_count() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Fill the socket buffers, then stall reading for 1.5 s.
        let big = "y".repeat(64 * 1024);
        let stall_until = tokio::time::Instant::now() + Duration::from_millis(1500);
        while tokio::time::Instant::now() < stall_until {
            hub.broadcast(big.clone());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let drain = tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            while read.read(&mut buf).await.is_ok_and(|n| n > 0) {}
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let alive = hub.receiver_count();
        talker.abort();
        drain.abort();
        assert_eq!(
            alive, 1,
            "a slow reader was dropped at the keepalive interval"
        );
    }

    /// A pong that queued while a send was blocked counts, even when the
    /// keepalive tick is overdue at the same time. Many runs, since the
    /// race is a coin flip per run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_pong_queued_behind_a_blocked_send_beats_an_overdue_tick() {
        let runs: Vec<_> = (0..16)
            .map(|_| tokio::spawn(pong_behind_blocked_send()))
            .collect();
        let mut dropped = 0;
        for run in runs {
            if !run.await.unwrap() {
                dropped += 1;
            }
        }
        assert_eq!(dropped, 0, "{dropped}/16 peers dropped despite a pong");
    }

    /// One run: `true` if the connection survived.
    async fn pong_behind_blocked_send() -> bool {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        const PONG: [u8; 6] = [0x8A, 0x80, 1, 2, 3, 4];
        // Wide enough that a loaded runner still pongs within one interval.
        let keepalive = Duration::from_millis(600);
        let hub = WsHub::new(EventBus::<String>::new(64)).keepalive(keepalive);
        let (mut read, mut write) = open(hub.clone()).await.into_split();
        // Wait for the first ping: the server now awaits a reply.
        loop {
            let mut h = [0u8; 2];
            read.read_exact(&mut h).await.unwrap();
            let mut body = vec![0u8; usize::from(h[1] & 0x7f)];
            read.read_exact(&mut body).await.unwrap();
            if h[0] & 0x0f == 0x9 {
                break;
            }
        }
        // Block the server in a send, queue the pong behind it, let the tick go overdue.
        // 15 MiB, past what loopback buffers hold on Linux and macOS; under the bus's 64.
        let big = "y".repeat(256 * 1024);
        for _ in 0..60 {
            hub.broadcast(big.clone());
        }
        tokio::time::sleep(keepalive / 3).await;
        write.write_all(&PONG).await.unwrap();
        tokio::time::sleep(keepalive * 2).await;
        let drain = tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            while read.read(&mut buf).await.is_ok_and(|n| n > 0) {}
        });
        // Keep answering so only the race can close the socket.
        let ponger = tokio::spawn(async move {
            while write.write_all(&PONG).await.is_ok() {
                tokio::time::sleep(keepalive / 4).await;
            }
        });
        tokio::time::sleep(keepalive * 2).await;
        let alive = hub.receiver_count() == 1;
        drain.abort();
        ponger.abort();
        alive
    }

    #[test]
    fn send_timeout_has_a_floor() {
        assert_eq!(send_timeout(Duration::from_secs(1)), SEND_TIMEOUT_FLOOR);
        assert_eq!(
            send_timeout(Duration::from_secs(60)),
            Duration::from_secs(60)
        );
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
