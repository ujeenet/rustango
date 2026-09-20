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

> **Source:** `rustango::test_client` (`TestClient`, `TestResponse`) — needs
> the `admin` feature, because it wraps an `axum::Router`. `rustango::test_assertions`
> (`assert_status_2xx`, `assert_redirects`, `assert_cookie_set`, …) and
> `rustango::test_db` (`with_rollback`) are ungated.
>
> **Runnable version:** the snippets below *are* a passing test —
> [`testing_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/testing_doc.rs)
> (`cargo test -p rustango --test testing_doc`). Nearly every other `*_doc.rs`
> in this repo uses `TestClient` the same way.

## Table of contents

- [Step 1 — Drive your app with TestClient](#step-1--drive-your-app-with-testclient)
- [Step 2 — Assert on the response](#step-2--assert-on-the-response)
- [Sending JSON, headers, and bodies](#sending-json-headers-and-bodies)
- [Testing a real API](#testing-a-real-api)
- [Database tests with rollback](#database-tests-with-rollback)
- [Live suites, and why a green run may prove nothing](#live-suites-and-why-a-green-run-may-prove-nothing)
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
    with_rollback(&pool, |tx| Box::pin(async move {
        // ... insert + assert against `tx` ...
        // everything here is rolled back when the closure returns
        Ok(())
    })).await.unwrap();
}
```

The `Box::pin` is required, not stylistic: the bound is
`for<'tx> FnOnce(&'tx mut PoolTx<'_>) -> Pin<Box<dyn Future<…> + Send + 'tx>>`,
which is how the closure gets to borrow `tx` across its own await points. The
closure returns `Result<T, ExecError>`, and so does `with_rollback` — the
rollback happens either way, so the `unwrap` is about your assertions, not
about cleanup.

For SQLite, the `*_sqlite_live.rs` tests throughout this repo use an in-memory
database per test instead — also fully isolated, with zero external setup.

---

## Live suites, and why a green run may prove nothing

Tests named `*_live.rs` talk to a real database. Most need nothing from you; the
rest need an environment variable, and **when it is missing they do not fail.
They return, and the run reports success.**

That is deliberate — it keeps `cargo test` working on a laptop with no server —
but it means a passing run is not evidence the suite ran. Worth knowing before
you read a green result as coverage.

### Which variable each suite wants

| Variable | Suites | What they need |
|---|---:|---|
| *(none)* | 211 | Nothing — an in-memory or temp-file SQLite. Always run. |
| `DATABASE_URL` | 96 | A reachable PostgreSQL server. |
| `MYSQL_TEST_URL` | 28 | A reachable MySQL 8+ server. **Not** `DATABASE_URL`. |
| `REDIS_TEST_URL` | 2 | A reachable Redis. |

A suite reading two variables is counted under both, so the column does not sum
to the number of files.

The `*_tri.rs` suites are counted under both server variables. They read no
variable themselves — `Backend::pool()` does the lookup — and they run their
SQLite arm with nothing set, so counting them as needing nothing would be
technically survivable and practically wrong: the two arms that need a server
are the reason those suites exist. Start both servers, or a tri suite reports
a healthy pass count having exercised one backend of three.

MySQL is the one that catches people: it reads its own variable, so a shell with
only `DATABASE_URL` set runs the Postgres suites and silently skips every MySQL
one.

### Telling a skip from a pass

Most skips are a bare early `return` with no output at all. A minority print a
line to stderr first, which `cargo test` hides unless you ask:

```bash
cargo test --test <name> -- --nocapture
```

The reliable signal is the count. A live suite that reports `0 passed` — or far
fewer than the file contains — skipped. `running 2 tests … 2 passed` with no
server running means those two tests returned early.

If you want a suite to fail rather than skip when its server is missing, set the
variable to a deliberately bad URL: it will then fail at connect, which is a
louder and more honest signal than a skip.

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
