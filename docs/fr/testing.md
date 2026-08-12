# Tests

Des tests rapides et fiables doivent piloter votre application comme le ferait un client — sans
démarrer de serveur ni toucher au réseau. Le `TestClient` de **Rustango** exécute votre
routeur **in-process** : vous appelez `client.get("/path")`, il achemine la requête
à travers la vraie pile (extractors, middleware, handlers) et vous renvoie la
réponse sur laquelle faire des assertions. Ajoutez l'isolation par rollback de transaction pour les tests de base de données et
un ensemble d'assertions de réponse, et vous obtenez le client de test de Django + `TestCase`, en
Rust.

[![Les tests dans Rustango : TestClient enveloppe votre Router et envoie des requêtes in-process à travers la vraie pile de handlers ; la TestResponse expose le statut, le texte et le JSON sur lesquels faire des assertions — pas de socket, pas de serveur](img/testing.png)](img/testing.png)

> **Un terme nouveau ici ?** *router*, *handler*, *fixture*, *rollback* — voir le
> [glossaire](glossary.md).

> **Source :** `rustango::test_client` (`TestClient`, `TestResponse`),
> `rustango::test_assertions` (`assert_status_2xx`, `assert_redirects`,
> `assert_cookie_set`, …), et `rustango::test_db` (`with_rollback`) — toujours
> compilés.
>
> **Version exécutable :** les snippets ci-dessous *sont* un test qui passe —
> [`testing_doc.rs`](../crates/rustango/tests/testing_doc.rs)
> (`cargo test -p rustango --test testing_doc`). Presque tous les autres `*_doc.rs`
> de ce dépôt utilisent `TestClient` de la même manière.

## Table des matières

- [Étape 1 — Pilotez votre application avec TestClient](#step-1--drive-your-app-with-testclient)
- [Étape 2 — Faites des assertions sur la réponse](#step-2--assert-on-the-response)
- [Envoyer du JSON, des en-têtes et des corps](#sending-json-headers-and-bodies)
- [Tester une vraie API](#testing-a-real-api)
- [Tests de base de données avec rollback](#database-tests-with-rollback)
- [Helpers d'assertion de réponse](#response-assertion-helpers)
- [Voir aussi](#see-also)

---

## Étape 1 — Pilotez votre application avec TestClient

Enveloppez n'importe quel `axum::Router` dans un `TestClient` et envoyez des requêtes — aucun socket n'est lié,
aucune tâche serveur n'est lancée. La requête traverse votre vrai middleware et vos
handlers :

```rust
use rustango::test_client::TestClient;

let client = TestClient::new(app());          // app() returns your Router

let res = client.get("/ping").send().await;   // routed in-process
assert_eq!(res.status, 200);
```

`TestClient` a `get` / `post` / `put` / `patch` / `delete` / `head`, chacun
renvoyant un builder que vous finissez avec `.send().await`.

---

## Étape 2 — Faites des assertions sur la réponse

`TestResponse` expose le statut et le corps dans la forme dont vous avez besoin :

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

## Envoyer du JSON, des en-têtes et des corps

Le builder de requête enchaîne tout avant `.send()` :

```rust
let res = client
    .post("/api/posts")
    .header("authorization", "Bearer <token>")   // auth, content negotiation, …
    .json(&json!({ "title": "Hello", "body": "..." }))
    .send()
    .await;
assert_eq!(res.status, 201);
```

Utilisez `.body(...)` pour les corps bruts (non-JSON), et une route manquante renvoie un vrai
`404` — vérifié dans le test sous-jacent.

---

## Tester une vraie API

`app()` dans vos tests n'est que votre routeur. Pour une API adossée à une DB, construisez-la exactement
comme le fait `main.rs` mais avec un pool de test — le motif que la plupart des tests `*_doc.rs` utilisent :

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

C'est le test [ViewSets](viewsets.md) de ce guide — le même `TestClient`.

---

## Tests de base de données avec rollback

Les tests qui écrivent dans une base de données ne doivent pas laisser fuir leur état les uns dans les autres.
`test_db::with_rollback` exécute votre test à l'intérieur d'une transaction et **la rollback**
à la fin, de sorte que chaque test démarre depuis le même état propre et que rien ne persiste :

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

Pour SQLite, les tests `*_sqlite_live.rs` à travers ce dépôt utilisent à la place une base de données
en mémoire par test — également entièrement isolée, avec zéro configuration externe.

---

## Helpers d'assertion de réponse

Pour les valeurs `axum::Response` brutes (par exemple issues de `tower::oneshot`), `test_assertions`
se lit comme les `assertContains` / `assertRedirects` de Django :

```rust
use rustango::test_assertions::{assert_status_2xx, assert_redirects, assert_cookie_set};

assert_status_2xx(&res);
assert_redirects(&res, "/login?next=/dashboard");
assert_cookie_set(&res, "rustango_session", None);
```

Également disponibles : `assert_status` / `assert_status_in` / `assert_status_4xx` /
`assert_status_5xx`, `assert_header`, `assert_content_type`,
`assert_redirect_chain`, `assert_cookie_not_set`, et `assert_messages`.

---

## Voir aussi

- [ViewSets](viewsets.md) · [Vues HTML](html-views.md) — ce sur quoi vous pointez le
  `TestClient`.
- [Middleware](middleware.md) — `TestClient` exerce aussi les couches (sans DB,
  via `tower::oneshot` dans `middleware.rs`).
- [Prise en main](getting-started.md) — l'étape 16 écrit le premier test.
- [CLI `manage`](manage.md) — `make:test` génère un module de test.
