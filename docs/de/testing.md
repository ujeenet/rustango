# Testen

Schnelle, zuverlässige Tests müssen deine App so ansteuern, wie es ein Client tut — ohne
einen Server zu booten oder das Netzwerk zu berühren. **Rustango**s `TestClient` führt deinen
Router **in-process** aus: du rufst `client.get("/path")` auf, es routet die Anfrage
durch den echten Stack (Extractors, Middleware, Handler) und gibt dir die
Antwort zum Assertieren zurück. Füge Transaction-Rollback-Isolation für Datenbanktests und
ein Set von Response-Assertions hinzu, und ein ganzer Request-Ablauf ist ohne
Socket testbar.

[![Testen in Rustango: TestClient umschließt deinen Router und sendet In-process-Anfragen durch den echten Handler-Stack; die TestResponse macht Status, Text und JSON zum Assertieren verfügbar — kein Socket, kein Server](../img/testing.png)](../img/testing.png)

> **Neu bei einem Begriff hier?** *router*, *handler*, *fixture*, *rollback* — siehe das
> [Glossar](glossary.md).

> **Quelle:** `rustango::test_client` (`TestClient`, `TestResponse`) — braucht
> das Feature `admin`, weil es einen `axum::Router` umhüllt. `rustango::test_db`
> (`with_rollback`) ist ungegated. `rustango::test_assertions` wird immer
> kompiliert, aber seine Response-Helfer (`assert_status_2xx`, `assert_redirects`,
> `assert_cookie_set`, …) brauchen ein HTTP-Feature wie `admin` oder `manage`.
>
> **Lauffähige Version:** die untenstehenden Snippets *sind* ein bestehender Test —
> [`testing_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/testing_doc.rs)
> (`cargo test -p rustango --test testing_doc`). Nahezu jede andere `*_doc.rs`
> in diesem Repo verwendet `TestClient` auf dieselbe Weise.

## Inhaltsverzeichnis

- [Schritt 1 — Steuere deine App mit TestClient an](#schritt-1--steuere-deine-app-mit-testclient-an)
- [Schritt 2 — Assertiere auf die Response](#schritt-2--assertiere-auf-die-response)
- [JSON, Header und Bodies senden](#json-header-und-bodies-senden)
- [Eine echte API testen](#eine-echte-api-testen)
- [Datenbanktests mit Rollback](#datenbanktests-mit-rollback)
- [Live-Suites, und warum ein grüner Lauf nichts beweisen muss](#live-suites-und-warum-ein-grüner-lauf-nichts-beweisen-muss)
- [Response-Assertion-Helper](#response-assertion-helper)
- [Siehe auch](#siehe-auch)

---

## Schritt 1 — Steuere deine App mit TestClient an

Umschließe einen beliebigen `axum::Router` in einem `TestClient` und sende Anfragen — kein Socket wird gebunden,
kein Server-Task gestartet. Die Anfrage fließt durch deine echte Middleware und
Handler:

```rust
use rustango::test_client::TestClient;

let client = TestClient::new(app());          // app() returns your Router

let res = client.get("/ping").send().await;   // routed in-process
assert_eq!(res.status, 200);
```

`TestClient` hat `get` / `post` / `put` / `patch` / `delete` / `head` /
`query`, dazu `request(method, path)`, jeweils einen Builder zurückgebend, den
du mit `.send().await` abschließt.

Der Client verhält sich wie ein Browser auf `http://testserver`:

- **Ein Cookie-Jar.** `Set-Cookie` wird gespeichert und bei späteren Requests
  an passende Pfade zurückgeschickt. Lies ihn mit `cookie(name)` oder
  `cookies()`, befülle ihn mit `set_cookie`, leere ihn mit `clear_cookies`.
- **`Host: testserver`** und ein same-origin `Origin` bei unsicheren Methoden,
  sofern der Request keine eigenen setzt.
- **Peer-Adresse `127.0.0.1`.** Pro Request änderbar mit `.remote_addr(addr)`.
- **CSRF wird durchgesetzt.** Sende das Token mit `.with_csrf()` (Header
  `X-CSRF-Token`) oder einem `_csrf`-Formularfeld; `csrf_token()` liest es.

---

## Schritt 2 — Assertiere auf die Response

`TestResponse` macht den Status und Body in jeder Form verfügbar, die du brauchst:

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

## JSON, Header und Bodies senden

Der Request-Builder verkettet alles vor `.send()`:

```rust
let res = client
    .post("/api/posts")
    .header("authorization", "Bearer <token>")   // auth, content negotiation, …
    .json(&json!({ "title": "Hello", "body": "..." }))
    .send()
    .await;
assert_eq!(res.status, 201);
```

Nutze `.form(&[("k", "v")])` für einen Formular-Body, `.body(...)` für rohe
Bytes, und eine fehlende Route gibt ein echtes `404` zurück — verifiziert im
zugrunde liegenden Test.

`.send_following_redirects(max_hops)` folgt bis zu so vielen Redirects wie ein
Browser und gibt die letzte Antwort mit der `(status, location)`-Kette zurück.
`client.get_following_redirects(path, max_hops)` ist die `GET`-Abkürzung.

### Anmelden

- `client.login(path, &[("username", "ada"), ("password", "…")])` meldet über
  das Formular an: Es holt `path` für ein CSRF-Token, wenn der Jar keines hat,
  und sendet dann die Felder. Das Session-Cookie bleibt im Jar.
- `client.logout(Some(path))` postet an die Logout-Route; `logout(None)` leert
  nur den Jar.
- Mit `tenancy` legen `force_login_tenant_user(&secret, slug, &user, ttl_secs)`
  und `force_login_operator(&secret, &operator, ttl_secs)` ein Session-Cookie
  ohne Formular in den Jar. Die Zeile muss gespeichert sein.

---

## Eine echte API testen

`app()` in deinen Tests ist einfach dein Router. Für eine DB-gestützte API baue ihn genau
so, wie es `main.rs` tut, aber mit einem Test-Pool — das Muster, das die meisten `*_doc.rs`-Tests verwenden:

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

Dies ist der [ViewSets](viewsets.md)-Test aus jenem Leitfaden — derselbe `TestClient`.

---

## Datenbanktests mit Rollback

Tests, die in eine Datenbank schreiben, dürfen keinen Zustand ineinander lecken lassen.
`test_db::with_rollback` führt deinen Test innerhalb einer Transaktion aus und **rollt sie zurück**
am Ende, sodass jeder Test vom selben sauberen Zustand startet und nichts persistiert wird:

```rust
use rustango::test_db::with_rollback;

#[tokio::test]
async fn creating_a_post_persists_it() {
    with_rollback(&pool, |tx| Box::pin(async move {
        // ... insert + assert against `&mut *tx.lock().await?` ...
        // everything here is rolled back when the closure returns
        Ok(())
    })).await.unwrap();
}
```

Das `Box::pin` ist Pflicht, keine Stilfrage: Die Schranke ist
`for<'tx> FnOnce(&'tx AtomicTx) -> Pin<Box<dyn Future<…> + Send + 'tx>>`,
so darf die Closure `tx` über ihre eigenen Await-Punkte hinweg ausleihen. Ein
`atomic()` auf demselben Pool innerhalb der Closure ist ein Savepoint und wird
ebenfalls zurückgerollt; gib den Guard davor oder vor einem `bulk_insert_pool`
frei, sonst schlagen sie mit `NestedAtomic` fehl. Die Closure liefert
`Result<T, ExecError>`, ebenso `with_rollback` — das Rollback passiert so oder
so, das `unwrap` betrifft also deine Assertions, nicht das Aufräumen.

Für SQLite verwenden die `*_sqlite_live.rs`-Tests überall in diesem Repo stattdessen eine In-Memory-
Datenbank pro Test — ebenfalls vollständig isoliert, mit null externem Setup.

Code, der wirklich committen muss (Signale, `on_commit`-Hooks), läuft nicht
unter einem Rollback. Nutze `with_truncate_after(&pool, &["posts"], || async move { … })`:
Der Body committet, danach werden die genannten Tabellen geleert, auch wenn der
Body panict. Führe solche Tests nacheinander hinter einem gemeinsamen
`tokio::sync::Mutex` aus. `truncate_tables(&pool, &[…])` ist das Leeren allein.

### Weitere Helfer

| Modul | Was es dir gibt |
|---|---|
| `test_server::LiveServer` (`admin`) | Ein echter Socket: `LiveServer::spawn(router)`, dann `url(path)` |
| `test_factory` | `Sequence` (ein Zähler, der Werte baut) und der Trait `Factory` mit `build_batch(n)` |
| `test_data` | `setup_test_data!` / `setup_test_data_async!`: ein Fixture, einmal pro Prozess gebaut |
| `test_settings` (`config`) | `with_overridden(settings, future)`: `Settings::load` gibt in der Future `settings` zurück |
| `test_filter` | `tags!("slow")` am Anfang eines Tests; `RUSTANGO_TEST_TAGS` / `RUSTANGO_TEST_EXCLUDE_TAGS` wählen aus, was läuft |

---

## Live-Suites, und warum ein grüner Lauf nichts beweisen muss

Tests mit dem Namen `*_live.rs` sprechen mit einer echten Datenbank. Die meisten
brauchen nichts von dir; der Rest braucht eine Umgebungsvariable, und **wenn sie
fehlt, schlagen sie nicht fehl. Sie machen ein `return`, und der Lauf meldet
Erfolg.**

Das ist Absicht — es hält `cargo test` auf einem Laptop ohne Server lauffähig —
aber es bedeutet, dass ein bestandener Lauf kein Beleg dafür ist, dass die Suite
gelaufen ist. Gut zu wissen, bevor du ein grünes Ergebnis als Abdeckung liest.

### Welche Variable jede Suite will

| Variable | Suites | Was sie brauchen |
|---|---:|---|
| *(keine)* | 229 | Nichts — eine In-Memory- oder temporäre Datei-SQLite. Laufen immer. |
| `DATABASE_URL` | 184 | Ein erreichbarer PostgreSQL-Server. |
| `MYSQL_TEST_URL` | 116 | Ein erreichbarer MySQL-8+-Server. **Nicht** `DATABASE_URL`. |
| `REDIS_TEST_URL` | 2 | Ein erreichbares Redis. |

Eine Suite, die zwei Variablen liest, wird unter beiden gezählt, die Spalte
summiert sich also nicht auf die Anzahl der Dateien.

Die `*_tri.rs`-Suiten werden unter beiden Server-Variablen gezählt. Sie lesen
selbst keine Variable — `Backend::pool()` übernimmt das — und ihr SQLite-Arm
läuft auch ohne gesetzte Variablen. Sie als „braucht nichts“ zu zählen wäre
deshalb formal haltbar und praktisch falsch: Die beiden Arme, die einen Server
brauchen, sind der Grund für diese Suiten. Starte beide Server, sonst meldet
eine Tri-Suite eine gesunde Trefferzahl und hat ein Backend von dreien geprüft.

MySQL ist das, worüber Leute stolpern: es liest seine eigene Variable, also führt
eine Shell, in der nur `DATABASE_URL` gesetzt ist, die Postgres-Suites aus und
überspringt stillschweigend jede MySQL-Suite.

### Nacheinander ausführen

Die Live-Suiten teilen sich eine Datenbank, und die meisten löschen oder leeren
die Framework-Tabellen im Setup, parallel ausgeführt kämpfen sie also um den
Katalog. Nutze das Profil `live`, das `test-threads = 1` setzt:

```bash
cargo nextest run --profile live --workspace --all-features
```

Ohne es gibt es viele Fehlschläge, und keiner davon ist echt: 758 gegen
PostgreSQL, 95 gegen MySQL, mit dem Profil in beiden Fällen 0. Die
`tokio::sync::Mutex`-Guards in diesen Suiten helfen nicht, weil nextest jeden
Test in seinem eigenen Prozess ausführt und ein Mutex in einem Prozess keinen
anderen erreicht.

Das ist ein Workaround, keine Lösung — Datenbanken pro Prozess würden die
Parallelität zurückbringen.

### Einen Skip von einem Pass unterscheiden

Die meisten Skips sind ein nacktes frühes `return` ganz ohne Ausgabe. Eine
Minderheit gibt zuvor eine Zeile auf stderr aus, die `cargo test` versteckt,
solange du nicht danach fragst:

```bash
cargo test --test <name> -- --nocapture
```

Das verlässliche Signal ist die Anzahl. Eine Live-Suite, die `0 passed` meldet —
oder weit weniger, als die Datei enthält —, hat übersprungen.
`running 2 tests … 2 passed` ohne laufenden Server heißt, dass diese beiden Tests
früh zurückgekehrt sind.

Wenn du willst, dass eine Suite fehlschlägt statt zu überspringen, wenn ihr
Server fehlt, setze die Variable auf eine absichtlich falsche URL: sie schlägt
dann beim Verbinden fehl, was ein lauteres und ehrlicheres Signal ist als ein
Skip.

---

## Response-Assertion-Helper

Für rohe `axum::Response`-Werte (z. B. aus `tower::oneshot`) liefert `test_assertions`
fertige Prüfungen für Status, Redirects und Cookies:

```rust
use rustango::test_assertions::{assert_status_2xx, assert_redirects, assert_cookie_set};

assert_status_2xx(&res);
assert_redirects(&res, "/login?next=/dashboard");
assert_cookie_set(&res, "rustango_session", None);
```

Ebenfalls verfügbar: `assert_status` / `assert_status_in` / `assert_status_4xx` /
`assert_status_5xx`, `assert_header`, `assert_content_type`,
`assert_redirect_chain`, `assert_cookie_not_set` und `assert_messages`.

---

## Siehe auch

- [ViewSets](viewsets.md) · [HTML-Views](html-views.md) — worauf du den
  `TestClient` richtest.
- [Middleware](middleware.md) — `TestClient` übt auch die Layer aus (DB-frei,
  via `tower::oneshot` in `middleware.rs`).
- [Erste Schritte](getting-started.md) — Schritt 16 schreibt den ersten Test.
- [`manage` CLI](manage.md) — `make:test` scaffoldet ein Test-Modul.
