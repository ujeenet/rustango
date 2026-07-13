# Testing

Fast, reliable tests need to drive your app the way a client does — without
booting a server or touching the network. **Rustango**'s `TestClient` runs your
router **in-process**: you call `client.get("/path")`, it routes the request
through the real stack (extractors, middleware, handlers) and hands back the
response to assert on. Add transaction-rollback isolation for database tests and
a set of response assertions, and you have Django's test client + `TestCase`, in
Rust.

[![Testing in Rustango: TestClient wraps your Router and sends in-process requests through the real handler stack; the TestResponse exposes status, text, and JSON to assert on — no socket, no server](img/testing.png)](img/testing.png)

> **New to a term here?** *router*, *handler*, *fixture*, *rollback* — see the
> [glossary](glossary.md).

> **Source:** `rustango::test_client` (`TestClient`, `TestResponse`),
> `rustango::test_assertions` (`assert_status_2xx`, `assert_redirects`,
> `assert_cookie_set`, …), and `rustango::test_db` (`with_rollback`) — always
> compiled.
>
> **Runnable version:** the snippets below *are* a passing test —
> [`testing_doc.rs`](../crates/rustango/tests/testing_doc.rs)
> (`cargo test -p rustango --test testing_doc`). Nearly every other `*_doc.rs`
> in this repo uses `TestClient` the same way.

## Table of contents

- [Step 1 — Drive your app with TestClient](#step-1--drive-your-app-with-testclient)
- [Step 2 — Assert on the response](#step-2--assert-on-the-response)
- [Sending JSON, headers, and bodies](#sending-json-headers-and-bodies)
- [Testing a real API](#testing-a-real-api)
- [Database tests with rollback](#database-tests-with-rollback)
- [Response assertion helpers](#response-assertion-helpers)
- [See also](#see-also)

---

## Step 1 — Drive your app with TestClient

Wrap any `axum::Router` in a `TestClient` and send requests — no socket is bound,
no server task spawned. The request flows through your real middleware and
handlers:

```rust
use rustango::test_client::TestClient;

let client = TestClient::new(app());          // app() returns your Router

let res = client.get("/ping").send().await;   // routed in-process
assert_eq!(res.status, 200);
```

`TestClient` has `get` / `post` / `put` / `patch` / `delete` / `head`, each
returning a builder you finish with `.send().await`.

---

## Step 2 — Assert on the response

`TestResponse` exposes the status and body in whatever shape you need:

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

## Sending JSON, headers, and bodies

The request builder chains everything before `.send()`:

```rust
let res = client
    .post("/api/posts")
    .header("authorization", "Bearer <token>")   // auth, content negotiation, …
    .json(&json!({ "title": "Hello", "body": "..." }))
    .send()
    .await;
assert_eq!(res.status, 201);
```

Use `.body(...)` for raw (non-JSON) bodies, and a missing route returns a real
`404` — verified in the backing test.

---

## Testing a real API

`app()` in your tests is just your router. For a DB-backed API, build it exactly
as `main.rs` does but with a test pool — the pattern most `*_doc.rs` tests use:

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

This is the [ViewSets](viewsets.md) test from that guide — the same `TestClient`.

---

## Database tests with rollback

Tests that write to a database must not leak state into each other.
`test_db::with_rollback` runs your test inside a transaction and **rolls it back**
at the end, so every test starts from the same clean state and nothing persists:

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

For SQLite, the `*_sqlite_live.rs` tests throughout this repo use an in-memory
database per test instead — also fully isolated, with zero external setup.

---

## Response assertion helpers

For raw `axum::Response` values (e.g. from `tower::oneshot`), `test_assertions`
reads like Django's `assertContains` / `assertRedirects`:

```rust
use rustango::test_assertions::{assert_status_2xx, assert_redirects, assert_cookie_set};

assert_status_2xx(&res);
assert_redirects(&res, "/login?next=/dashboard");
assert_cookie_set(&res, "rustango_session", None);
```

Also available: `assert_status` / `assert_status_in` / `assert_status_4xx` /
`assert_status_5xx`, `assert_header`, `assert_content_type`,
`assert_redirect_chain`, `assert_cookie_not_set`, and `assert_messages`.

---

## See also

- [ViewSets](viewsets.md) · [HTML views](html-views.md) — what you point the
  `TestClient` at.
- [Middleware](middleware.md) — `TestClient` exercises the layers too (DB-free,
  via `tower::oneshot` in `middleware.rs`).
- [Getting started](getting-started.md) — Step 16 writes the first test.
- [`manage` CLI](manage.md) — `make:test` scaffolds a test module.
