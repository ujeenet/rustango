# La méthode QUERY

`QUERY` ([RFC 10008](https://www.rfc-editor.org/rfc/rfc10008), une norme proposée
publiée en juin 2026) est le « GET sûr avec un corps ». Elle est **sûre** et
**idempotente** comme `GET`, mais les critères de recherche voyagent dans le corps
de la requête plutôt que dans l'URL — ainsi une recherche complexe n'a pas à être
comprimée dans une chaîne de requête, et il n'y a pas de plafond de longueur d'URL.
**Rustango** route `QUERY` aux côtés de `GET` dans tout le framework : le routage,
un extracteur adaptatif à la méthode, la politique CSRF/CORS/retentative, la mise
en cache par vue, les ViewSets, le client de test et OpenAPI.

> **Source :** `rustango::http_query` (`query`, `QueryRouterExt`, `QUERY`) et
> `rustango::params` (`Params`) — derrière la fonctionnalité `admin`. Surface
> connexe : `viewset::ViewSet`, `cache_page::CachePageLayer::cache_query`,
> `forms::csrf::CsrfConfig::require_csrf_on_query`, `cors::CorsLayer`,
> `test_client` / `http_client`.

## Quand y recourir

Utilisez `QUERY` au lieu de `GET` lorsque les critères de recherche sont volumineux
ou structurés :

- Des ensembles de filtres qui dépasseraient une chaîne de requête (longues listes
  `IN`, nombreuses facettes).
- Des critères imbriqués / structurés qui ne se mappent pas proprement en paires
  `?key=value` — envoyez-les sous forme de corps JSON.
- Tout ce que vous seriez tenté de modéliser comme un `POST /search` alors même
  qu'il ne lit aucun état et ne change rien — `QUERY` dit « ceci est une lecture
  sûre et cacheable » dans la méthode elle-même.

Utilisez un simple `GET` pour les requêtes simples, courtes et marque-pageables.
`QUERY` est additif : le même handler peut servir les deux.

## Routage

axum 0.8 ne peut pas router `QUERY` nativement (son `MethodFilter` est un ensemble
fermé — [tokio-rs/axum#3799](https://github.com/tokio-rs/axum/issues/3799)), donc
rustango fournit `query()` et une chaîne `.query()` qui reflètent les propres
`get()` / `post()` d'axum :

```rust
use rustango::http_query::{query, QueryRouterExt};
use axum::routing::get;

let app = axum::Router::new()
    // QUERY-only route.
    .route("/search", query(search))
    // GET + QUERY on one path — chain `.query()` last.
    .route("/products", get(list_products).query(search_products));
```

Un `405` sur une route mixte rapporte l'ensemble complet des méthodes, par ex.
`Allow: GET,HEAD,QUERY`.

## Un handler, deux transports

`Params<T>` lit `T` depuis la chaîne de requête sur `GET`/`HEAD` et depuis le corps
de la requête sur `QUERY`, de sorte qu'un seul handler sert les deux sans
branchement :

```rust
use rustango::params::Params;
use rustango::http_query::QueryRouterExt;
use axum::routing::get;
use serde::Deserialize;

#[derive(Deserialize)]
struct Search { q: String, page: Option<u32> }

async fn search(Params(s): Params<Search>) -> String {
    format!("q={} page={:?}", s.q, s.page)
}

let app = axum::Router::new().route("/search", get(search).query(search));
```

`GET /search?q=hi` et `QUERY /search` avec le corps `q=hi` atteignent `search` et
se désérialisent de manière identique. Sur `QUERY`, le corps est analysé selon le
`Content-Type` :

| `Content-Type` | Analysé comme | Notes |
|---|---|---|
| `application/x-www-form-urlencoded` (ou aucun) | `serde_urlencoded` | Même chemin de code que la chaîne de requête — plat, une seule valeur par clé. |
| `application/json` (ou un suffixe `…+json`) | `serde_json` | À utiliser pour les tableaux / critères imbriqués. |
| tout le reste | — | `415 Unsupported Media Type` |

Les codes de statut correspondent aux conventions d'axum : une erreur d'analyse de
la chaîne de requête est un `400`, une erreur d'analyse du corps est un `422`, et
une méthode autre que GET/HEAD/QUERY est un `405`.

```bash
# urlencoded body
curl -X QUERY http://localhost:8080/search \
     -H 'Content-Type: application/x-www-form-urlencoded' \
     --data 'q=hello&page=2'

# JSON body (arrays / nesting)
curl -X QUERY http://localhost:8080/search \
     -H 'Content-Type: application/json' \
     --data '{"q":"hello","tags":["rust","web"]}'
```

## ViewSets

Un `ViewSet` obtient gratuitement une action de collection `QUERY` — `QUERY /things`
renvoie la même liste filtrée / ordonnée / paginée que `GET /things?…`, mais avec
les critères dans le corps :

```bash
# identical results:
curl 'http://localhost:8080/posts?status=draft&ordering=-rating'
curl -X QUERY http://localhost:8080/posts \
     -H 'Content-Type: application/x-www-form-urlencoded' \
     --data 'status=draft&ordering=-rating'

# arrays via JSON (comma-joined internally for __in lookups):
curl -X QUERY http://localhost:8080/posts \
     -H 'Content-Type: application/json' \
     --data '{"status__in":["draft","published"],"rating__gte":2}'
```

Les permissions et les throttles réutilisent celles de l'action `list`, et les vues
d'administration personnalisées déclarées avec la méthode `QUERY` se routent
correctement elles aussi.

## Garanties du framework

- **CSRF.** `QUERY` est exempté par défaut de l'application des jetons CSRF, aux
  côtés de `GET`/`HEAD`/`OPTIONS`/`TRACE`. Les navigateurs ne peuvent pas soumettre
  `QUERY` via un formulaire, et un `fetch` cross-origin avec la méthode `QUERY`
  n'est jamais une « requête simple » figurant sur la liste blanche CORS — il
  déclenche toujours un preflight — il n'y a donc pas de vecteur CSRF par
  identifiants ambiants. Définissez `CsrfConfig::require_csrf_on_query = true` pour
  une défense en profondeur pure. Gardez les handlers `QUERY` sans effet de bord,
  et n'ajoutez jamais `QUERY` à la liste d'autorisation de la
  [surcharge de méthode](middleware.md).
- **CORS.** `QUERY` figure dans les listes de méthodes `permissive()` et dérivées
  des paramètres. Parce que chaque `QUERY` cross-origin fait un preflight, elle doit
  être annoncée dans `Access-Control-Allow-Methods` pour fonctionner en cross-origin.
- **Retentatives.** Le client HTTP (`http_client`) traite `QUERY` comme idempotente,
  elle est donc retentée en cas d'échecs transitoires comme `GET`.
- **Idempotence.** `QUERY` n'a besoin d'aucun `Idempotency-Key` — la méthode est
  idempotente par définition.
- **Mise en cache.** `cache_page` peut mettre en cache les réponses `QUERY` lorsque
  vous l'activez avec `CachePageLayer::cache_query(true)`. La clé de cache intègre un
  condensé du corps de la requête, et la réponse est marquée `private` afin que les
  caches partagés (qui ne peuvent pas se baser sur un corps) ne la servent jamais à
  tort. Voir [Mise en cache](caching.md).

## Tests

`TestClient` et `RequestFactory` disposent de constructeurs `.query()`, et le
`HttpClient` sortant peut envoyer `QUERY` :

```rust
let resp = client.query("/search").json(&criteria).send().await;
```

## OpenAPI

OpenAPI 3.1 n'a pas d'opération `QUERY` ; [OpenAPI 3.2](https://www.openapis.org/blog/2025/09/23/announcing-openapi-v3-2)
l'a ajoutée comme champ Path Item de première classe. Rustango émet une opération
`query` lorsque vous en attachez une, et fait passer la spécification à
`openapi: 3.2.0` uniquement pour les spécifications qui l'utilisent (les
spécifications sans `QUERY` restent en `3.1.0` pour une compatibilité maximale avec
l'outillage) :

```rust
use rustango::openapi::{OpenApiSpec, PathItem, Operation, RequestBody, Response, Schema};

let spec = OpenApiSpec::new("API", "1.0").add_path(
    "/posts",
    PathItem::new()
        .get(Operation::new().summary("List posts").response("200", Response::new("OK")))
        .query(
            Operation::new()
                .summary("Search posts")
                .request_body(RequestBody::json(Schema::ref_("SearchCriteria")))
                .response("200", Response::new("OK")),
        ),
);
```
