# Tests

Des tests rapides et fiables doivent piloter votre application comme le ferait un client — sans
démarrer de serveur ni toucher au réseau. Le `TestClient` de **Rustango** exécute votre
routeur **in-process** : vous appelez `client.get("/path")`, il achemine la requête
à travers la vraie pile (extractors, middleware, handlers) et vous renvoie la
réponse sur laquelle faire des assertions. Ajoutez l'isolation par rollback de transaction pour les tests de base de données et
un ensemble d'assertions de réponse, et vous obtenez un environnement de tests
d'intégration complet sans quitter le processus.

[![Les tests dans Rustango : TestClient enveloppe votre Router et envoie des requêtes in-process à travers la vraie pile de handlers ; la TestResponse expose le statut, le texte et le JSON sur lesquels faire des assertions — pas de socket, pas de serveur](../img/testing.png)](../img/testing.png)

> **Un terme nouveau ici ?** *router*, *handler*, *fixture*, *rollback* — voir le
> [glossaire](glossary.md).

> **Source :** `rustango::test_client` (`TestClient`, `TestResponse`) — demande
> la feature `admin`, car il enveloppe un `axum::Router`. `rustango::test_db`
> (`with_rollback`) n'est derrière aucune feature. `rustango::test_assertions`
> est toujours compilé, mais ses helpers de réponse (`assert_status_2xx`,
> `assert_redirects`, `assert_cookie_set`, …) demandent une feature HTTP comme
> `admin` ou `manage`.
>
> **Version exécutable :** les snippets ci-dessous *sont* un test qui passe —
> [`testing_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/testing_doc.rs)
> (`cargo test -p rustango --test testing_doc`). Presque tous les autres `*_doc.rs`
> de ce dépôt utilisent `TestClient` de la même manière.

## Table des matières

- [Étape 1 — Pilotez votre application avec TestClient](#étape-1--pilotez-votre-application-avec-testclient)
- [Étape 2 — Faites des assertions sur la réponse](#étape-2--faites-des-assertions-sur-la-réponse)
- [Envoyer du JSON, des en-têtes et des corps](#envoyer-du-json-des-en-têtes-et-des-corps)
- [Tester une vraie API](#tester-une-vraie-api)
- [Tests de base de données avec rollback](#tests-de-base-de-données-avec-rollback)
- [Suites live, et pourquoi une exécution verte peut ne rien prouver](#suites-live-et-pourquoi-une-exécution-verte-peut-ne-rien-prouver)
- [Helpers d'assertion de réponse](#helpers-dassertion-de-réponse)
- [Voir aussi](#voir-aussi)

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

`TestClient` a `get` / `post` / `put` / `patch` / `delete` / `head` /
`query`, plus `request(method, path)`, chacun renvoyant un builder que vous
finissez avec `.send().await`.

Le client se comporte comme un navigateur sur `http://testserver` :

- **Un cookie jar.** `Set-Cookie` est conservé et renvoyé aux requêtes
  suivantes vers les chemins correspondants. Lisez-le avec `cookie(name)` ou
  `cookies()`, remplissez-le avec `set_cookie`, videz-le avec `clear_cookies`.
- **`Host: testserver`**, et un `Origin` de même origine sur les méthodes non
  sûres, sauf si la requête fixe les siens.
- **Adresse du pair `127.0.0.1`.** Changez-la par requête avec
  `.remote_addr(addr)`.
- **Le CSRF est appliqué.** Envoyez le jeton avec `.with_csrf()` (l'en-tête
  `X-CSRF-Token`) ou un champ de formulaire `_csrf` ; `csrf_token()` le lit.

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

Utilisez `.form(&[("k", "v")])` pour un corps de formulaire, `.body(...)` pour
des octets bruts, et une route manquante renvoie un vrai `404` — vérifié dans le
test sous-jacent.

`.send_following_redirects(max_hops)` suit jusqu'à ce nombre de redirections
comme un navigateur et renvoie la réponse finale avec la chaîne
`(status, location)`. `client.get_following_redirects(path, max_hops)` est le
raccourci `GET`.

### Se connecter

- `client.login(path, &[("username", "ada"), ("password", "…")])` se connecte
  par le formulaire : il charge `path` pour un jeton CSRF si le jar n'en a pas,
  puis envoie les champs. Le cookie de session reste dans le jar.
- `client.logout(Some(path))` poste vers la route de déconnexion ;
  `logout(None)` vide seulement le jar.
- Avec `tenancy`, `force_login_tenant_user(&secret, slug, &user, ttl_secs)` et
  `force_login_operator(&secret, &operator, ttl_secs)` mettent un cookie de
  session dans le jar sans le formulaire. La ligne doit être enregistrée.

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
    with_rollback(&pool, |tx| Box::pin(async move {
        // ... insert + assert against `&mut *tx.lock().await?` ...
        // everything here is rolled back when the closure returns
        Ok(())
    })).await.unwrap();
}
```

Le `Box::pin` est obligatoire, pas stylistique : la borne est
`for<'tx> FnOnce(&'tx AtomicTx) -> Pin<Box<dyn Future<…> + Send + 'tx>>`,
c'est ce qui permet à la closure d'emprunter `tx` à travers ses propres points
d'await. Un `atomic()` sur le même pool à l'intérieur de la closure est un
savepoint et est annulé lui aussi ; relâchez le guard avant lui ou avant un
`bulk_insert_pool`, sinon ils échouent avec `NestedAtomic`. La closure renvoie
`Result<T, ExecError>`, tout comme `with_rollback` — le rollback a lieu dans
tous les cas, donc l'`unwrap` concerne vos assertions, pas le nettoyage.

Pour SQLite, les tests `*_sqlite_live.rs` à travers ce dépôt utilisent à la place une base de données
en mémoire par test — également entièrement isolée, avec zéro configuration externe.

Le code qui doit vraiment valider (signaux, hooks `on_commit`) ne tourne pas
sous un rollback. Utilisez `with_truncate_after(&pool, &["posts"], || async move { … })` :
le corps valide, puis les tables listées sont vidées, même si le corps panique.
Lancez ces tests un par un derrière un `tokio::sync::Mutex` partagé.
`truncate_tables(&pool, &[…])` est le vidage seul.

### Autres helpers

| Module | Ce qu'il vous donne |
|---|---|
| `test_server::LiveServer` (`admin`) | Un vrai socket : `LiveServer::spawn(router)`, puis `url(path)` |
| `test_factory` | `Sequence` (un compteur qui construit des valeurs) et le trait `Factory` avec `build_batch(n)` |
| `test_data` | `setup_test_data!` / `setup_test_data_async!` : une fixture construite une fois par processus |
| `test_settings` (`config`) | `with_overridden(settings, future)` : `Settings::load` renvoie `settings` dans la future |
| `test_filter` | `tags!("slow")` en tête d'un test ; `RUSTANGO_TEST_TAGS` / `RUSTANGO_TEST_EXCLUDE_TAGS` choisissent ce qui tourne |

---

## Suites live, et pourquoi une exécution verte peut ne rien prouver

Les tests nommés `*_live.rs` parlent à une vraie base de données. La plupart
n'ont besoin de rien de votre part ; les autres ont besoin d'une variable
d'environnement, et **quand elle manque, ils n'échouent pas. Ils font un
`return`, et l'exécution signale un succès.**

C'est délibéré — cela garde `cargo test` fonctionnel sur un portable sans
serveur — mais cela veut dire qu'une exécution qui passe n'est pas la preuve que
la suite a tourné. Bon à savoir avant de lire un résultat vert comme de la
couverture.

### Quelle variable veut chaque suite

| Variable | Suites | Ce dont elles ont besoin |
|---|---:|---|
| *(aucune)* | 228 | Rien — une SQLite en mémoire ou en fichier temporaire. Tournent toujours. |
| `DATABASE_URL` | 181 | Un serveur PostgreSQL joignable. |
| `MYSQL_TEST_URL` | 113 | Un serveur MySQL 8+ joignable. **Pas** `DATABASE_URL`. |
| `REDIS_TEST_URL` | 2 | Un Redis joignable. |

Une suite qui lit deux variables est comptée sous les deux ; la colonne ne
totalise donc pas le nombre de fichiers.

Les suites `*_tri.rs` sont comptées sous les deux variables de serveur. Elles ne
lisent aucune variable elles-mêmes — c'est `Backend::pool()` qui s'en charge — et
leur bras SQLite tourne sans rien de configuré ; les compter comme « ne demande
rien » serait techniquement tenable et pratiquement faux : les deux bras qui
exigent un serveur sont la raison d'être de ces suites. Démarre les deux
serveurs, sinon une suite tri annoncera un joli total en n'ayant éprouvé qu'un
backend sur trois.

MySQL est celle qui piège les gens : elle lit sa propre variable, donc un shell
où seule `DATABASE_URL` est définie exécute les suites Postgres et saute
silencieusement toutes les suites MySQL.

### Les exécuter une à la fois

Les suites live partagent une seule base et la plupart suppriment ou vident les
tables du framework à l'initialisation : les exécuter en parallèle les fait se
battre pour le catalogue. Utilisez le profil `live`, qui définit
`test-threads = 1` :

```bash
cargo nextest run --profile live --workspace --all-features
```

Sans lui, les échecs sont nombreux et aucun n'est réel : 758 contre
PostgreSQL, 95 contre MySQL, 0 dans les deux cas avec le profil. Les gardes
`tokio::sync::Mutex` à l'intérieur de ces suites n'aident pas, car nextest
exécute chaque test dans son propre processus et un mutex dans l'un n'atteint
pas l'autre.

C'est un contournement, pas une correction : des bases par processus
rétabliraient le parallélisme.

### Distinguer un saut d'un succès

La plupart des sauts sont un simple `return` anticipé, sans aucune sortie. Une
minorité affiche d'abord une ligne sur stderr, que `cargo test` masque à moins
que vous ne la demandiez :

```bash
cargo test --test <name> -- --nocapture
```

Le signal fiable est le compte. Une suite live qui rapporte `0 passed` — ou bien
moins que ce que le fichier contient — a sauté.
`running 2 tests … 2 passed` sans serveur en marche signifie que ces deux tests
ont fait un `return` anticipé.

Si vous voulez qu'une suite échoue plutôt que de sauter quand son serveur manque,
définissez la variable sur une URL délibérément mauvaise : elle échouera alors à
la connexion, ce qui est un signal plus bruyant et plus honnête qu'un saut.

---

## Helpers d'assertion de réponse

Pour les valeurs `axum::Response` brutes (par exemple issues de `tower::oneshot`), `test_assertions`
fournit des vérifications lisibles en une ligne pour le statut, le contenu, les
redirections et les cookies :

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
