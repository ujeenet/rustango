# WebSockets & SSE

La plupart des requêtes sont ponctuelles : on demande, on répond, terminé. Les fonctionnalités
**temps réel** sont l'exception — une notification en direct, une barre de progression, un tableau
de bord qui se met à jour tout seul, un chat. Le serveur doit *pousser* vers le navigateur sans
qu'on le lui demande. **Rustango** vous offre les deux transports de push sur une seule fondation :

- **SSE (Server-Sent Events)** — un flux unidirectionnel, serveur → navigateur, sur du HTTP simple.
  L'`EventSource` intégré au navigateur se reconnecte automatiquement. La chose la plus simple qui
  fonctionne ; utilisez-le pour les notifications, la progression, les compteurs en direct, les
  flux.
- **WebSockets** — un canal full-duplex, dans les deux sens. Utilisez-le quand le
  client envoie aussi (chat, édition collaborative, présence).

Les deux se diffusent à travers le même **bus de diffusion** en processus ([`EventBus`]), si bien
que « envoyer ceci à chaque client connecté » est un seul appel quel que soit le transport. Si vous
venez de Django, c'est Channels ; de Laravel, Echo/Reverb ; de Node,
`ws` + `EventSource` — mêmes idées, un seul bus derrière elles.

> **Source :** `rustango::sse` (`EventBus`) — derrière la fonctionnalité **`sse`** ; et
> `rustango::ws` (`WsHub`, `WsConfig`, `ws_handler`) — derrière la
> fonctionnalité **`websocket`** (implique `admin` + `sse` + `axum/ws`). Les deux sont dans
> l'ensemble de fonctionnalités par défaut. Le canal serveur→client du serveur MCP
> (`rustango::mcp::transport`) est un consommateur de production du côté SSE.
>
> **Référence exécutable :** les snippets faisant autorité et à jour sont les
> guides de démarrage rapide des modules dans `src/sse.rs` et `src/ws.rs` ; la diffusion de
> l'`EventBus` est couverte par les tests unitaires dans `src/sse.rs`.

> **Un terme vous est inconnu ?** *SSE*, *WebSocket*, *broadcast*, *keep-alive/ping*,
> *back-pressure* — voir le [glossaire](glossary.md).

## Table des matières

- [SSE ou WebSocket — lequel ?](#sse-or-websocket--which)
- [Le bus de diffusion](#the-broadcast-bus)
- [Server-Sent Events](#server-sent-events)
- [WebSockets](#websockets)
- [Envoyer depuis ailleurs](#sending-from-elsewhere)
- [Auth & multi-locataire](#auth--tenancy)
- [Notes de mise à l'échelle](#scaling-notes)
- [Drapeaux de fonctionnalités](#feature-flags)

---

## SSE ou WebSocket — lequel ?

| | **SSE** | **WebSocket** |
|---|---|---|
| Direction | serveur → client uniquement | bidirectionnel |
| Transport | HTTP simple (`text/event-stream`) | mise à niveau `ws://` / `wss://` |
| Reconnexion | automatique (intégrée à `EventSource`) | à votre charge (ou une bibliothèque cliente le fait) |
| Proxys / infra | juste du HTTP — proxifié trivialement | nécessite des proxys conscients de la mise à niveau |
| API cliente | `new EventSource(url)` | `new WebSocket(url)` |
| À utiliser pour | notifications, progression, compteurs en direct, flux | chat, présence, édition collaborative, tout ce que le client *envoie* aussi |

**Règle empirique :** si le client ne fait que *recevoir*, utilisez SSE — c'est moins de code, ça se reconnecte automatiquement, et ça roule sur du HTTP ordinaire. Tournez-vous vers un WebSocket seulement quand le client *envoie* aussi.

## Le bus de diffusion

Les deux transports reposent sur [`EventBus<T>`] — un canal de diffusion peu coûteux à cloner. Un
seul `send` atteint chaque abonné ; les abonnés lents prennent du retard plutôt que de bloquer les
autres. `T` est votre propre type de message `Clone`.

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

API d'`EventBus` : `new(capacity)`, `send(event) -> usize`, `subscribe() ->
broadcast::Receiver<T>`, `receiver_count() -> usize`.

## Server-Sent Events

Câblez le bus à un point d'accès SSE avec la réponse `Sse` d'axum + un
`async_stream::stream!` qui transmet chaque message de diffusion en tant qu'événement. Ajoutez
`async-stream` et `futures` à votre `Cargo.toml`.

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

Navigateur — l'`EventSource` intégré se connecte et se reconnecte automatiquement :

```js
const es = new EventSource('/events');
es.addEventListener('notification', (e) => {
  const n = JSON.parse(e.data);
  console.log(n.kind, n.message);
});
es.onerror = () => {/* EventSource retries automatically */};
```

## WebSockets

Pour du trafic bidirectionnel, utilisez [`WsHub`] + [`ws_handler`]. Le hub enveloppe un
`EventBus` et ajoute des pings keep-alive, la (dé)sérialisation JSON, et la gestion des
consommateurs lents ; `ws_handler` fait tourner une connexion jusqu'à ce que le client se
déconnecte.

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

**Réglage** via [`WsConfig`] (méthodes de builder sur `WsHub`) :

| Réglage | Défaut | Effet |
|---|---|---|
| `.keepalive(Duration)` | 30 s | intervalle de `Ping` en veille — plus bas détecte les connexions mortes plus tôt |
| `.on_message(fn(&str) -> Option<String>)` | aucun | gère le texte client→serveur ; renvoyez `Some(reply)` pour renvoyer un écho **à ce client uniquement** (appelez `hub.broadcast(..)` pour la diffusion) |
| `.max_message_bytes(n)` | 1 MiB | ferme la connexion si un message client dépasse cette limite |

**Les consommateurs lents** ne sont pas abandonnés en silence : un client à la traîne se voit dire
qu'il a manqué `n` messages (`Lagged(n)`) afin qu'il puisse se resynchroniser ou continuer.

## Envoyer depuis ailleurs

Le bus/hub est `Clone` et vit dans le state du routeur, si bien que n'importe quelle partie de votre
application — un handler de requête, une [tâche d'arrière-plan](jobs.md), un récepteur de
[signal](models.md) — peut pousser vers chaque client connecté en tenant un clone et en appelant
`bus.send(..)` / `hub.broadcast(..)`. Un motif courant : une tâche se termine → elle diffuse une
notification « terminé » → le flux SSE ouvert du navigateur met l'UI à jour.

Un seul `EventBus` peut soutenir **à la fois** une route SSE et une route WebSocket en même temps —
`WsHub::bus()` vous remet le bus sous-jacent à partager avec un handler SSE, si bien que les
clients sur l'un ou l'autre transport voient le même flux.

## Auth & multi-locataire

Les routes SSE/WS sont des routes axum ordinaires — superposez-y votre auth habituelle
(session, [JWT](auth-jwt-api.md), [clé d'API](auth-api-keys.md)) exactement comme vous le
feriez pour n'importe quel handler ; ajoutez l'extracteur à la route et rejetez avant de
s'abonner. Dans une application multi-locataire, résolvez le locataire sur la connexion et utilisez
un **bus par locataire** (ou filtrez les messages par locataire) afin que les événements d'un
locataire n'atteignent jamais les clients d'un autre. Le [serveur MCP](mcp.md) fait exactement
cela — son canal SSE authentifié et par agent est construit sur cette même fondation `sse`.

## Notes de mise à l'échelle

`EventBus` est un `tokio::sync::broadcast` **en processus** — la diffusion est instantanée
et légère en verrous, mais elle n'atteint que les clients connectés à *ce* processus. Vous exécutez
plusieurs instances d'application derrière un équilibreur de charge ? Un client connecté au nœud A
ne verra pas un événement envoyé sur le nœud B. Pour diffuser à travers les nœuds, faites le pont
entre le bus et un pub/sub externe (par ex. Redis) — publiez-y les événements métier et faites en
sorte que le souscripteur de chaque nœud les re-`send` vers son `EventBus` local. Le cas
mono-nœud (le cas courant) n'a besoin de rien de tout cela.

Dimensionnez aussi `EventBus::new(capacity)` pour votre débit en pointe : c'est le tampon par
abonné, et un client qui prend plus de retard que `capacity` reçoit un
signal `Lagged` plutôt qu'une croissance mémoire illimitée.

## Drapeaux de fonctionnalités

```toml
# Cargo.toml — both are in Rustango's default set; list them if you slim features.
rustango = { version = "*", features = ["sse", "websocket"] }
```

- **`sse`** — le bus de diffusion `EventBus` (tire `tokio`). Suffisant pour SSE, puisque
  le format de fil SSE provient du `response::sse` propre à axum.
- **`websocket`** — l'échafaudage `WsHub` / `ws_handler` ; implique `admin` (axum)
  + `sse` + `axum/ws`.
