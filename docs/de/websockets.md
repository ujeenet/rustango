# WebSockets & SSE

Die meisten Requests sind einmalig: fragen, antworten, fertig. **Echtzeit**-Features sind die
Ausnahme — eine Live-Benachrichtigung, ein Fortschrittsbalken, ein Dashboard, das sich
selbst aktualisiert, ein Chat. Der Server muss zum Browser *pushen*, ohne gefragt zu werden.
**Rustango** gibt dir beide Push-Transporte auf einer Grundlage:

- **SSE (Server-Sent Events)** — ein Einweg-Stream, Server → Browser, über schlichtes
  HTTP. Die eingebaute `EventSource` des Browsers verbindet sich automatisch neu. Das Einfachste,
  was funktioniert; verwende es für Benachrichtigungen, Fortschritt, Live-Zähler, Feeds.
- **WebSockets** — ein Vollduplex-Kanal, beide Richtungen. Verwende ihn, wenn der
  Client auch sendet (Chat, kollaboratives Editieren, Präsenz).

Beide fächern sich über denselben In-Process-**Broadcast-Bus** ([`EventBus`]) auf, sodass
„sende dies an jeden verbundenen Client" ein einziger Aufruf ist, unabhängig vom Transport. Wenn du
von Django kommst, ist das Channels; von Laravel, Echo/Reverb; von Node,
`ws` + `EventSource` — dieselben Ideen, ein Bus dahinter.

> **Quelle:** `rustango::sse` (`EventBus`) — hinter dem **`sse`**-Feature; und
> `rustango::ws` (`WsHub`, `WsConfig`, `ws_handler`) — hinter dem
> **`websocket`**-Feature (impliziert `admin` + `sse` + `axum/ws`). Beide sind im
> Standard-Feature-Set. Der Server→Client-Kanal des MCP-Servers
> (`rustango::mcp::transport`) ist ein Produktionskonsument der SSE-Seite.
>
> **Lauffähige Referenz:** Die maßgeblichen, aktuellen Snippets sind die
> Modul-Schnellstarts in `src/sse.rs` und `src/ws.rs`; das Auffächern des
> `EventBus` wird von den Unit-Tests in `src/sse.rs` abgedeckt.

> **Neu bei einem Begriff hier?** *SSE*, *WebSocket*, *broadcast*, *keep-alive/ping*,
> *back-pressure* — siehe das [Glossar](glossary.md).

## Inhaltsverzeichnis

- [SSE oder WebSocket — welches?](#sse-or-websocket--which)
- [Der Broadcast-Bus](#the-broadcast-bus)
- [Server-Sent Events](#server-sent-events)
- [WebSockets](#websockets)
- [Von anderswo senden](#sending-from-elsewhere)
- [Auth & Mandantenfähigkeit](#auth--tenancy)
- [Skalierungshinweise](#scaling-notes)
- [Feature-Flags](#feature-flags)

---

## SSE oder WebSocket — welches?

| | **SSE** | **WebSocket** |
|---|---|---|
| Richtung | nur Server → Client | bidirektional |
| Transport | schlichtes HTTP (`text/event-stream`) | `ws://` / `wss://` Upgrade |
| Reconnect | automatisch (in `EventSource` eingebaut) | du übernimmst es (oder eine Client-Bibliothek tut es) |
| Proxys / Infra | nur HTTP — trivial proxybar | benötigt upgrade-fähige Proxys |
| Client-API | `new EventSource(url)` | `new WebSocket(url)` |
| Verwende es für | Benachrichtigungen, Fortschritt, Live-Zähler, Feeds | Chat, Präsenz, kollaboratives Editieren, alles, was der Client auch *sendet* |

**Faustregel:** Wenn der Client nur *empfängt*, verwende SSE — es ist weniger Code, verbindet sich automatisch neu und läuft über gewöhnliches HTTP. Greife nur dann zu einem WebSocket, wenn der Client auch *sendet*.

## Der Broadcast-Bus

Beide Transporte sitzen auf [`EventBus<T>`] — einem günstig zu klonenden Auffächer-Kanal. Ein
`send` erreicht jeden Abonnenten; langsame Abonnenten hinken hinterher, statt die
anderen zu blockieren. `T` ist dein eigener `Clone`-Nachrichtentyp.

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

`EventBus`-API: `new(capacity)`, `send(event) -> usize`, `subscribe() ->
broadcast::Receiver<T>`, `receiver_count() -> usize`.

## Server-Sent Events

Verdrahte den Bus mit einem SSE-Endpunkt über axums `Sse`-Response + einen
`async_stream::stream!`, der jede Broadcast-Nachricht als Event weiterleitet. Füge
`async-stream` und `futures` zu deiner `Cargo.toml` hinzu.

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

Browser — die eingebaute `EventSource` verbindet sich und reconnectet automatisch:

```js
const es = new EventSource('/events');
es.addEventListener('notification', (e) => {
  const n = JSON.parse(e.data);
  console.log(n.kind, n.message);
});
es.onerror = () => {/* EventSource retries automatically */};
```

## WebSockets

Für bidirektionalen Verkehr verwende [`WsHub`] + [`ws_handler`]. Der Hub umhüllt einen
`EventBus` und ergänzt Keep-Alive-Pings, JSON-(De-)Serialisierung und die Behandlung langsamer
Konsumenten; `ws_handler` betreibt eine Verbindung, bis der Client sich trennt.

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

**Tuning** über [`WsConfig`] (Builder-Methoden auf `WsHub`):

| Stellschraube | Standard | Wirkung |
|---|---|---|
| `.keepalive(Duration)` | 30 s | Intervall des Leerlauf-`Ping` — niedriger erkennt tote Verbindungen früher |
| `.on_message(fn(&str) -> Option<String>)` | keine | verarbeitet Client→Server-Text; gib `Some(reply)` zurück, um ein Echo **nur an diesen Client** zu senden (rufe `hub.broadcast(..)` für das Auffächern auf) |
| `.max_message_bytes(n)` | 1 MiB | schließt die Verbindung, wenn eine Client-Nachricht dies überschreitet |

**Langsame Konsumenten** werden nicht stillschweigend verworfen: einem hinterherhinkenden Client
wird mitgeteilt, dass er `n` Nachrichten verpasst hat (`Lagged(n)`), sodass er neu synchronisieren
oder weitermachen kann.

## Von anderswo senden

Der Bus/Hub ist `Clone` und lebt im Router-State, sodass jeder Teil deiner
App — ein Request-Handler, ein [Hintergrundjob](jobs.md), ein [Signal](models.md)-Empfänger —
zu jedem verbundenen Client pushen kann, indem er einen Klon hält und
`bus.send(..)` / `hub.broadcast(..)` aufruft. Ein häufiges Muster: Ein Job endet → er broadcastet eine
„erledigt"-Benachrichtigung → der offene SSE-Stream des Browsers aktualisiert die UI.

Ein `EventBus` kann **sowohl** eine SSE-Route als auch eine WebSocket-Route zugleich unterlegen —
`WsHub::bus()` reicht dir den zugrunde liegenden Bus, um ihn mit einem SSE-Handler zu teilen, sodass
Clients auf beiden Transporten denselben Stream sehen.

## Auth & Mandantenfähigkeit

Die SSE/WS-Routen sind gewöhnliche axum-Routen — lege deine übliche Auth darüber
(Session, [JWT](auth-jwt-api.md), [API-Key](auth-api-keys.md)) genauso, wie du es
bei jedem Handler tätest; füge den Extractor zur Route hinzu und lehne ab, bevor
abonniert wird. In einer mandantenfähigen App löse den Mandanten auf der Verbindung auf und verwende
einen **Bus pro Mandant** (oder filtere Nachrichten nach Mandant), sodass die Events eines
Mandanten nie die Clients eines anderen erreichen. Der [MCP-Server](mcp.md) tut genau
dies — sein authentifizierter, agentenbezogener SSE-Kanal baut auf dieser selben `sse`-Grundlage auf.

## Skalierungshinweise

`EventBus` ist ein **In-Process**-`tokio::sync::broadcast` — das Auffächern ist sofortig
und sperrleicht, aber es erreicht nur Clients, die mit *diesem* Prozess verbunden sind. Du betreibst
mehrere App-Instanzen hinter einem Load Balancer? Ein mit Knoten A verbundener Client
sieht kein Event, das auf Knoten B gesendet wurde. Um über Knoten hinweg aufzufächern, überbrücke
den Bus zu einem externen Pub/Sub (z. B. Redis) — veröffentliche Domänen-Events dort und lass den
Abonnenten jedes Knotens sie erneut auf seinen lokalen `EventBus` `send`en. Der Ein-Knoten-Fall
(der häufige Fall) braucht nichts davon.

Bemesse außerdem `EventBus::new(capacity)` für deine Spitzenrate: es ist der Puffer pro
Abonnent, und ein Client, der weiter als `capacity` zurückfällt, erhält ein
`Lagged`-Signal statt unbeschränkten Speicherwachstums.

## Feature-Flags

```toml
# Cargo.toml — both are in Rustango's default set; list them if you slim features.
rustango = { version = "*", features = ["sse", "websocket"] }
```

- **`sse`** — der `EventBus`-Broadcast-Bus (zieht `tokio` nach). Ausreichend für SSE, da
  das SSE-Drahtformat aus axums eigenem `response::sse` stammt.
- **`websocket`** — das `WsHub` / `ws_handler`-Gerüst; impliziert `admin` (axum)
  + `sse` + `axum/ws`.
