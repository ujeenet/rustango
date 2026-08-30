# Pruebas

Las pruebas rápidas y fiables necesitan controlar tu aplicación igual que lo hace
un cliente: sin arrancar un servidor ni tocar la red. El `TestClient` de
**Rustango** ejecuta tu router **en proceso**: llamas a `client.get("/path")`, la
petición se enruta a través de la pila real (extractores, middleware, manejadores)
y te devuelve la respuesta para hacer aserciones. Añade el aislamiento por
reversión de transacción para las pruebas de base de datos y un conjunto de
aserciones de respuesta, y tienes el cliente de pruebas de Django + `TestCase`, en
Rust.

[![Pruebas en Rustango: TestClient envuelve tu Router y envía peticiones en proceso a través de la pila real de manejadores; el TestResponse expone el estado, el texto y el JSON para hacer aserciones — sin socket, sin servidor](../img/testing.png)](../img/testing.png)

> **¿Hay algún término nuevo para ti aquí?** *router*, *handler*, *fixture*, *rollback* — consulta el
> [glosario](glossary.md).

> **Fuente:** `rustango::test_client` (`TestClient`, `TestResponse`),
> `rustango::test_assertions` (`assert_status_2xx`, `assert_redirects`,
> `assert_cookie_set`, …), y `rustango::test_db` (`with_rollback`) — siempre
> compilados.
>
> **Versión ejecutable:** los fragmentos de abajo *son* una prueba que pasa —
> [`testing_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/testing_doc.rs)
> (`cargo test -p rustango --test testing_doc`). Casi cualquier otro `*_doc.rs`
> de este repositorio usa `TestClient` de la misma manera.

## Tabla de contenidos

- [Paso 1 — Controla tu aplicación con TestClient](#step-1--drive-your-app-with-testclient)
- [Paso 2 — Haz aserciones sobre la respuesta](#step-2--assert-on-the-response)
- [Enviar JSON, cabeceras y cuerpos](#sending-json-headers-and-bodies)
- [Probar una API real](#testing-a-real-api)
- [Pruebas de base de datos con reversión](#database-tests-with-rollback)
- [Ayudantes de aserción de respuesta](#response-assertion-helpers)
- [Véase también](#see-also)

---

## Paso 1 — Controla tu aplicación con TestClient

Envuelve cualquier `axum::Router` en un `TestClient` y envía peticiones — no se
enlaza ningún socket, no se genera ninguna tarea de servidor. La petición fluye a
través de tu middleware y tus manejadores reales:

```rust
use rustango::test_client::TestClient;

let client = TestClient::new(app());          // app() returns your Router

let res = client.get("/ping").send().await;   // routed in-process
assert_eq!(res.status, 200);
```

`TestClient` tiene `get` / `post` / `put` / `patch` / `delete` / `head`, cada uno
devolviendo un constructor que finalizas con `.send().await`.

---

## Paso 2 — Haz aserciones sobre la respuesta

`TestResponse` expone el estado y el cuerpo en la forma que necesites:

```rust
let res = client.get("/ping").send().await;

res.status;                 // u16 — e.g. 200
res.text();                 // body as a String
res.header("content-type"); // Option<&str>
```

```rust
// JSON, two ways:
let res = client.post("/echo").json(&json!({ "name": "Ada" })).send().await;
assert_eq!(res.json_value()["name"], "Ada");   // untyped

#[derive(serde::Deserialize)]
struct Out { name: String }
let out: Out = res.json();                       // typed
assert_eq!(out.name, "Ada");
```

---

## Enviar JSON, cabeceras y cuerpos

El constructor de peticiones encadena todo antes de `.send()`:

```rust
let res = client
    .post("/api/posts")
    .header("authorization", "Bearer <token>")   // auth, content negotiation, …
    .json(&json!({ "title": "Hello", "body": "..." }))
    .send()
    .await;
assert_eq!(res.status, 201);
```

Usa `.body(...)` para cuerpos sin procesar (que no sean JSON), y una ruta
inexistente devuelve un `404` real — verificado en la prueba de respaldo.

---

## Probar una API real

`app()` en tus pruebas no es más que tu router. Para una API respaldada por base de
datos, constrúyela exactamente como lo hace `main.rs` pero con un pool de pruebas —
el patrón que usa la mayoría de las pruebas `*_doc.rs`:

```rust
async fn app() -> axum::Router {
    let pool = test_pool().await;                 // a sqlite::memory: or test DB pool
    PostViewSet::router("/api/posts", pool)
}

#[tokio::test]
async fn create_then_list() {
    let client = TestClient::new(app().await);
    let created = client.post("/api/posts")
        .json(&json!({ "title": "Hi", "body": "b" }))
        .send().await;
    assert_eq!(created.status, 201);

    let list = client.get("/api/posts").send().await;
    assert!(list.json_value()["results"].is_array());
}
```

Esta es la prueba de [ViewSets](viewsets.md) de aquella guía — el mismo `TestClient`.

---

## Pruebas de base de datos con reversión

Las pruebas que escriben en una base de datos no deben filtrar estado entre sí.
`test_db::with_rollback` ejecuta tu prueba dentro de una transacción y **la revierte**
al final, de modo que cada prueba parte del mismo estado limpio y nada persiste:

```rust
use rustango::test_db::with_rollback;

#[tokio::test]
async fn creating_a_post_persists_it() {
    with_rollback(&pool, |tx| async move {
        // ... insert + assert against `tx` ...
        // everything here is rolled back when the closure returns
    }).await;
}
```

Para SQLite, las pruebas `*_sqlite_live.rs` repartidas por este repositorio usan en
su lugar una base de datos en memoria por prueba — también totalmente aislada, con
cero configuración externa.

---

## Ayudantes de aserción de respuesta

Para valores `axum::Response` sin procesar (por ejemplo, de `tower::oneshot`),
`test_assertions` se lee como los `assertContains` / `assertRedirects` de Django:

```rust
use rustango::test_assertions::{assert_status_2xx, assert_redirects, assert_cookie_set};

assert_status_2xx(&res);
assert_redirects(&res, "/login?next=/dashboard");
assert_cookie_set(&res, "rustango_session", None);
```

También disponibles: `assert_status` / `assert_status_in` / `assert_status_4xx` /
`assert_status_5xx`, `assert_header`, `assert_content_type`,
`assert_redirect_chain`, `assert_cookie_not_set`, y `assert_messages`.

---

## Véase también

- [ViewSets](viewsets.md) · [Vistas HTML](html-views.md) — a qué apuntas el
  `TestClient`.
- [Middleware](middleware.md) — `TestClient` también ejercita las capas (sin base
  de datos, mediante `tower::oneshot` en `middleware.rs`).
- [Primeros pasos](getting-started.md) — el Paso 16 escribe la primera prueba.
- [CLI `manage`](manage.md) — `make:test` genera un módulo de pruebas.
