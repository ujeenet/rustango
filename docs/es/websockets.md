# WebSockets y SSE

La mayoría de las peticiones son de un solo disparo: pregunta, respuesta, listo. Las
características **en tiempo real** son la excepción — una notificación en vivo, una barra de
progreso, un panel que se actualiza solo, un chat. El servidor necesita *empujar* al navegador sin
que se lo pidan. **Rustango** te ofrece ambos transportes de push sobre una sola base:

- **SSE (Server-Sent Events)** — un flujo unidireccional, servidor → navegador, sobre HTTP
  simple. El `EventSource` integrado del navegador se reconecta automáticamente. Lo más simple que
  funciona; úsalo para notificaciones, progreso, contadores en vivo, feeds.
- **WebSockets** — un canal full-duplex, en ambas direcciones. Úsalo cuando el
  cliente también envía (chat, edición colaborativa, presencia).

Ambos se difunden a través del mismo **bus de difusión** en proceso ([`EventBus`]), de modo que
«envía esto a cada cliente conectado» es una sola llamada independientemente del transporte. Si
vienes de Django, esto es Channels; de Laravel, Echo/Reverb; de Node,
`ws` + `EventSource` — las mismas ideas, un solo bus detrás de ellas.

> **Fuente:** `rustango::sse` (`EventBus`) — tras la característica **`sse`**; y
> `rustango::ws` (`WsHub`, `WsConfig`, `ws_handler`) — tras la
> característica **`websocket`** (implica `admin` + `sse` + `axum/ws`). Ambas están en
> el conjunto de características por defecto. El canal servidor→cliente del servidor MCP
> (`rustango::mcp::transport`) es un consumidor de producción del lado SSE.
>
> **Referencia ejecutable:** los snippets autorizados y actualizados son las
> guías de inicio rápido de los módulos en `src/sse.rs` y `src/ws.rs`; la difusión del
> `EventBus` está cubierta por los tests unitarios en `src/sse.rs`.

> **¿Nuevo con algún término aquí?** *SSE*, *WebSocket*, *broadcast*, *keep-alive/ping*,
> *back-pressure* — ver el [glosario](glossary.md).

## Tabla de contenidos

- [¿SSE o WebSocket — cuál?](#sse-or-websocket--which)
- [El bus de difusión](#the-broadcast-bus)
- [Server-Sent Events](#server-sent-events)
- [WebSockets](#websockets)
- [Enviar desde otro sitio](#sending-from-elsewhere)
- [Auth y multi-inquilino](#auth--tenancy)
- [Notas de escalado](#scaling-notes)
- [Flags de características](#feature-flags)

---

## ¿SSE o WebSocket — cuál?

| | **SSE** | **WebSocket** |
|---|---|---|
| Dirección | solo servidor → cliente | bidireccional |
| Transporte | HTTP simple (`text/event-stream`) | upgrade `ws://` / `wss://` |
| Reconexión | automática (integrada en `EventSource`) | la gestionas tú (o lo hace una biblioteca cliente) |
| Proxies / infra | solo HTTP — trivialmente proxeable | necesita proxies conscientes del upgrade |
| API del cliente | `new EventSource(url)` | `new WebSocket(url)` |
| Úsalo para | notificaciones, progreso, contadores en vivo, feeds | chat, presencia, edición colaborativa, cualquier cosa que el cliente también *envíe* |

**Regla general:** si el cliente solo *recibe*, usa SSE — es menos código, se reconecta automáticamente y viaja sobre HTTP ordinario. Recurre a un WebSocket solo cuando el cliente también *envía*.

## El bus de difusión

Ambos transportes se apoyan en [`EventBus<T>`] — un canal de difusión barato de clonar. Un
solo `send` alcanza a cada suscriptor; los suscriptores lentos se rezagan en lugar de bloquear a
los demás. `T` es tu propio tipo de mensaje `Clone`.

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

API de `EventBus`: `new(capacity)`, `send(event) -> usize`, `subscribe() ->
broadcast::Receiver<T>`, `receiver_count() -> usize`.

## Server-Sent Events

Cablea el bus a un endpoint SSE con la respuesta `Sse` de axum + un
`async_stream::stream!` que reenvía cada mensaje de difusión como un evento. Añade
`async-stream` y `futures` a tu `Cargo.toml`.

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

Navegador — el `EventSource` integrado se conecta y se reconecta automáticamente:

```js
const es = new EventSource('/events');
es.addEventListener('notification', (e) => {
  const n = JSON.parse(e.data);
  console.log(n.kind, n.message);
});
es.onerror = () => {/* EventSource retries automatically */};
```

## WebSockets

Para tráfico bidireccional, usa [`WsHub`] + [`ws_handler`]. El hub envuelve un
`EventBus` y añade pings keep-alive, (de)serialización JSON y el manejo de consumidores
lentos; `ws_handler` ejecuta una conexión hasta que el cliente se desconecta.

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

**Ajuste** mediante [`WsConfig`] (métodos de builder en `WsHub`):

| Perilla | Por defecto | Efecto |
|---|---|---|
| `.keepalive(Duration)` | 30 s | intervalo de `Ping` en reposo — más bajo detecta conexiones muertas antes |
| `.on_message(fn(&str) -> Option<String>)` | ninguno | maneja texto cliente→servidor; devuelve `Some(reply)` para responder con eco **solo a ese cliente** (llama a `hub.broadcast(..)` para la difusión) |
| `.max_message_bytes(n)` | 1 MiB | cierra la conexión si un mensaje del cliente excede esto |

**Los consumidores lentos** no se descartan en silencio: a un cliente rezagado se le indica que
se perdió `n` mensajes (`Lagged(n)`) para que pueda resincronizarse o continuar.

## Enviar desde otro sitio

El bus/hub es `Clone` y vive en el state del router, de modo que cualquier parte de tu
aplicación — un handler de petición, un [trabajo en segundo plano](jobs.md), un receptor de
[señal](models.md) — puede empujar a cada cliente conectado sosteniendo un clon y llamando a
`bus.send(..)` / `hub.broadcast(..)`. Un patrón común: un trabajo termina → difunde una
notificación de «hecho» → el flujo SSE abierto del navegador actualiza la UI.

Un solo `EventBus` puede respaldar **tanto** una ruta SSE como una ruta WebSocket a la vez —
`WsHub::bus()` te entrega el bus subyacente para compartirlo con un handler SSE, de modo que los
clientes de cualquiera de los dos transportes ven el mismo flujo.

## Auth y multi-inquilino

Las rutas SSE/WS son rutas axum ordinarias — superpón tu auth habitual sobre ellas
(sesión, [JWT](auth-jwt-api.md), [clave de API](auth-api-keys.md)) exactamente como lo
harías con cualquier handler; añade el extractor a la ruta y rechaza antes de
suscribir. En una aplicación multi-inquilino, resuelve el inquilino en la conexión y usa un
**bus por inquilino** (o filtra los mensajes por inquilino) para que los eventos de un
inquilino nunca alcancen a los clientes de otro. El [servidor MCP](mcp.md) hace exactamente
esto — su canal SSE autenticado y por agente se construye sobre esta misma base `sse`.

## Notas de escalado

`EventBus` es un `tokio::sync::broadcast` **en proceso** — la difusión es instantánea
y ligera en bloqueos, pero solo alcanza a los clientes conectados a *este* proceso. ¿Ejecutas
varias instancias de la aplicación detrás de un balanceador de carga? Un cliente conectado al nodo A
no verá un evento enviado en el nodo B. Para difundir entre nodos, tiende un puente entre el bus y
un pub/sub externo (p. ej. Redis) — publica ahí los eventos de dominio y haz que el
suscriptor de cada nodo los vuelva a `send` a su `EventBus` local. El caso de un solo nodo
(el caso común) no necesita nada de esto.

Dimensiona también `EventBus::new(capacity)` para tu tasa de ráfaga: es el búfer por
suscriptor, y un cliente que se rezaga más allá de `capacity` recibe una
señal `Lagged` en lugar de un crecimiento de memoria ilimitado.

## Flags de características

```toml
# Cargo.toml — both are in Rustango's default set; list them if you slim features.
rustango = { version = "*", features = ["sse", "websocket"] }
```

- **`sse`** — el bus de difusión `EventBus` (arrastra `tokio`). Suficiente para SSE, ya que
  el formato de cable SSE proviene del propio `response::sse` de axum.
- **`websocket`** — el andamiaje `WsHub` / `ws_handler`; implica `admin` (axum)
  + `sse` + `axum/ws`.
