# WebSockets & SSE

Most requests are one-shot: ask, answer, done. **Real-time** features are the
exception — a live notification, a progress bar, a dashboard that updates
itself, a chat. The server needs to *push* to the browser without being asked.
**Rustango** gives you both push transports on one foundation:

- **SSE (Server-Sent Events)** — a one-way stream, server → browser, over plain
  HTTP. The browser's built-in `EventSource` auto-reconnects. Simplest thing
  that works; use it for notifications, progress, live counters, feeds.
- **WebSockets** — a full-duplex channel, both directions. Use it when the
  client also sends (chat, collaborative editing, presence).

Both fan out through the same in-process **broadcast bus** ([`EventBus`]), so
"send this to every connected client" is one call regardless of transport. If
you come from Django this is Channels; from Laravel, Echo/Reverb; from Node,
`ws` + `EventSource` — same ideas, one bus behind them.

> **Source:** `rustango::sse` (`EventBus`) — behind the **`sse`** feature; and
> `rustango::ws` (`WsHub`, `WsConfig`, `ws_handler`) — behind the
> **`websocket`** feature (implies `admin` + `sse` + `axum/ws`). Both are in the
> default feature set. The MCP server's server→client channel
> (`rustango::mcp::transport`) is a production consumer of the SSE side.
>
> **Runnable reference:** the authoritative, up-to-date snippets are the module
> quick-starts in `src/sse.rs` and `src/ws.rs`; `EventBus`'s fan-out is covered
> by the unit tests in `src/sse.rs`.

> **New to a term here?** *SSE*, *WebSocket*, *broadcast*, *keep-alive/ping*,
> *back-pressure* — see the [glossary](glossary.md).

## Table of contents

- [SSE or WebSocket — which?](#sse-or-websocket--which)
- [The broadcast bus](#the-broadcast-bus)
- [Server-Sent Events](#server-sent-events)
- [WebSockets](#websockets)
- [Sending from elsewhere](#sending-from-elsewhere)
- [Auth & tenancy](#auth--tenancy)
- [Scaling notes](#scaling-notes)
- [Feature flags](#feature-flags)

---

## SSE or WebSocket — which?

| | **SSE** | **WebSocket** |
|---|---|---|
| Direction | server → client only | bidirectional |
| Transport | plain HTTP (`text/event-stream`) | `ws://` / `wss://` upgrade |
| Reconnect | automatic (built into `EventSource`) | you handle it (or a client lib does) |
| Proxies / infra | just HTTP — trivially proxied | needs upgrade-aware proxies |
| Client API | `new EventSource(url)` | `new WebSocket(url)` |
| Use it for | notifications, progress, live counters, feeds | chat, presence, collaborative editing, anything the client also *sends* |

**Rule of thumb:** if the client only *receives*, use SSE — it's less code, auto-reconnects, and rides ordinary HTTP. Reach for a WebSocket only when the client also *sends*.

## The broadcast bus

Both transports sit on [`EventBus<T>`] — a cheap-to-clone fan-out channel. One
`send` reaches every subscriber; slow subscribers lag rather than blocking the
others. `T` is your own `Clone` message type.

```rust
use rustango::sse::EventBus;
use serde::{Serialize, Deserialize};

#[derive(Clone, Serialize, Deserialize)]
struct Notification { kind: String, message: String }

// capacity = per-subscriber buffer; older messages drop for a lagging client.
let bus: EventBus<Notification> = EventBus::new(100);

// share `bus` via router state (it's Clone). Then from anywhere:
let delivered = bus.send(Notification { kind: "info".into(), message: "Saved".into() });
//  `delivered` = how many connected clients received it (0 = no-op).
```

`EventBus` API: `new(capacity)`, `send(event) -> usize`, `subscribe() ->
broadcast::Receiver<T>`, `receiver_count() -> usize`.

## Server-Sent Events

Wire the bus to an SSE endpoint with axum's `Sse` response + an
`async_stream::stream!` that forwards each broadcast message as an event. Add
`async-stream` and `futures` to your `Cargo.toml`.

```rust
use std::convert::Infallible;
use axum::{extract::State, response::sse::{Event, KeepAlive, Sse}, routing::get, Router};
use futures::Stream;
use rustango::sse::EventBus;

async fn events(
    State(bus): State<EventBus<Notification>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = bus.subscribe();
    let stream = async_stream::stream! {
        while let Ok(msg) = rx.recv().await {
            let json = serde_json::to_string(&msg).unwrap_or_default();
            yield Ok(Event::default().event("notification").data(json));
        }
    };
    // keep-alive comments stop idle connections being reaped by proxies.
    Sse::new(stream).keep_alive(KeepAlive::default())
}

let app = Router::new()
    .route("/events", get(events))
    .with_state(bus.clone());
```

Browser — the built-in `EventSource` connects and auto-reconnects:

```js
const es = new EventSource('/events');
es.addEventListener('notification', (e) => {
  const n = JSON.parse(e.data);
  console.log(n.kind, n.message);
});
es.onerror = () => {/* EventSource retries automatically */};
```

## WebSockets

For bidirectional traffic, use [`WsHub`] + [`ws_handler`]. The hub wraps an
`EventBus` and adds keep-alive pings, JSON (de)serialization, and slow-consumer
handling; `ws_handler` runs one connection until the client disconnects.

```rust
use std::time::Duration;
use axum::{extract::{State, WebSocketUpgrade}, response::Response, routing::get, Router};
use rustango::{sse::EventBus, ws::{ws_handler, WsHub}};
use serde::{Serialize, Deserialize};

#[derive(Clone, Serialize, Deserialize)]
struct Tick { value: i64 }

let bus: EventBus<Tick> = EventBus::new(100);
let hub = WsHub::new(bus).keepalive(Duration::from_secs(20));

async fn ws_route(ws: WebSocketUpgrade, State(hub): State<WsHub<Tick>>) -> Response {
    ws.on_upgrade(move |socket| ws_handler(socket, hub.clone()))
}

let app = Router::new()
    .route("/ws", get(ws_route))
    .with_state(hub.clone());

// From anywhere — one call fans out to every connected socket:
hub.broadcast(Tick { value: 42 });
```

```js
const ws = new WebSocket(`${location.origin.replace('http', 'ws')}/ws`);
ws.onmessage = (e) => { const t = JSON.parse(e.data); console.log('tick', t.value); };
```

**Tuning** via [`WsConfig`] (builder methods on `WsHub`):

| Knob | Default | Effect |
|---|---|---|
| `.keepalive(Duration)` | 30 s | idle `Ping` interval — lower detects dead connections sooner |
| `.on_message(fn(&str) -> Option<String>)` | none | handle client→server text; return `Some(reply)` to echo **to that client only** (call `hub.broadcast(..)` for fan-out) |
| `.max_message_bytes(n)` | 1 MiB | close the connection if a client message exceeds this |

**Slow consumers** don't get silently dropped: a lagging client is told it
missed `n` messages (`Lagged(n)`) so it can resync or carry on.

## Sending from elsewhere

The bus/hub is `Clone` and lives in router state, so any part of your app —
a request handler, a [background job](jobs.md), a [signal](models.md) receiver —
can push to every connected client by holding a clone and calling `bus.send(..)`
/ `hub.broadcast(..)`. A common pattern: a job finishes → it broadcasts a
"done" notification → the browser's open SSE stream updates the UI.

One `EventBus` can back **both** an SSE route and a WebSocket route at once —
`WsHub::bus()` hands you the underlying bus to share with an SSE handler, so
clients on either transport see the same stream.

## Auth & tenancy

The SSE/WS routes are ordinary axum routes — layer your usual auth on them
(session, [JWT](auth-jwt-api.md), [API key](auth-api-keys.md)) exactly as you
would any handler; add the extractor to the route and reject before subscribing.
In a multi-tenant app, resolve the tenant on the connection and use a
**per-tenant bus** (or filter messages by tenant) so one tenant's events never
reach another's clients. The [MCP server](mcp.md) does exactly this — its
authenticated, per-agent SSE channel is built on this same `sse` foundation.

## Scaling notes

`EventBus` is an **in-process** `tokio::sync::broadcast` — fan-out is instant
and lock-light, but it only reaches clients connected to *this* process. Running
multiple app instances behind a load balancer? A client connected to node A
won't see an event sent on node B. To fan out across nodes, bridge the bus to an
external pub/sub (e.g. Redis) — publish domain events there and have each node's
subscriber re-`send` them onto its local `EventBus`. Single-node (the common
case) needs none of this.

Also size `EventBus::new(capacity)` for your burst rate: it's the per-subscriber
buffer, and a client that falls further behind than `capacity` gets a
`Lagged` signal rather than unbounded memory growth.

## Feature flags

```toml
# Cargo.toml — both are in Rustango's default set; list them if you slim features.
rustango = { version = "*", features = ["sse", "websocket"] }
```

- **`sse`** — the `EventBus` broadcast bus (pulls `tokio`). Enough for SSE, since
  the SSE wire format comes from axum's own `response::sse`.
- **`websocket`** — the `WsHub` / `ws_handler` scaffold; implies `admin` (axum)
  + `sse` + `axum/ws`.
