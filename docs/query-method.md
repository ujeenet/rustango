# The QUERY method

`QUERY` ([RFC 10008](https://www.rfc-editor.org/rfc/rfc10008), a Proposed Standard
published in June 2026) is the "safe GET with a body." It's **safe** and
**idempotent** like `GET`, but the query criteria travel in the request body
instead of the URL — so a complex search doesn't have to be squeezed into a
querystring, and there's no URL-length ceiling. **Rustango** routes `QUERY`
alongside `GET` across the whole framework: routing, a method-adaptive
extractor, CSRF/CORS/retry policy, per-view caching, ViewSets, the test client,
and OpenAPI.

> **Source:** `rustango::http_query` (`query`, `QueryRouterExt`, `QUERY`) and
> `rustango::params` (`Params`) — behind the `admin` feature. Related surface:
> `viewset::ViewSet`, `cache_page::CachePageLayer::cache_query`,
> `forms::csrf::CsrfConfig::require_csrf_on_query`, `cors::CorsLayer`,
> `test_client` / `http_client`.

## When to reach for it

Use `QUERY` instead of `GET` when the search criteria are big or structured:

- Filter sets that would blow past a querystring (long `IN` lists, many facets).
- Nested / structured criteria that don't map cleanly to `?key=value` pairs —
  send them as a JSON body.
- Anything you'd be tempted to model as a `POST /search` even though it reads no
  state and changes nothing — `QUERY` says "this is a safe, cacheable read" in
  the method itself.

Use plain `GET` for simple, short, bookmarkable requests. `QUERY` is additive:
the same handler can serve both.

## Routing

axum 0.8 can't route `QUERY` natively (its `MethodFilter` is a closed set —
[tokio-rs/axum#3799](https://github.com/tokio-rs/axum/issues/3799)), so rustango
provides `query()` and a `.query()` chain that mirror axum's own `get()` /
`post()`:

```rust
use rustango::http_query::{query, QueryRouterExt};
use axum::routing::get;

let app = axum::Router::new()
    // QUERY-only route.
    .route("/search", query(search))
    // GET + QUERY on one path — chain `.query()` last.
    .route("/products", get(list_products).query(search_products));
```

A `405` on a mixed route reports the full method set, e.g.
`Allow: GET,HEAD,QUERY`.

## One handler, both transports

`Params<T>` reads `T` from the querystring on `GET`/`HEAD` and from the request
body on `QUERY`, so a single handler serves both with no branching:

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

`GET /search?q=hi` and `QUERY /search` with body `q=hi` reach `search` and
deserialize identically. On `QUERY` the body is parsed by `Content-Type`:

| `Content-Type` | Parsed as | Notes |
|---|---|---|
| `application/x-www-form-urlencoded` (or none) | `serde_urlencoded` | Same codepath as the querystring — flat, single-value per key. |
| `application/json` (or a `…+json` suffix) | `serde_json` | Use this for arrays / nested criteria. |
| anything else | — | `415 Unsupported Media Type` |

Status codes match axum's conventions: a querystring parse error is `400`, a body
parse error is `422`, and a method other than GET/HEAD/QUERY is `405`.

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

A `ViewSet` gets a `QUERY` collection action for free — `QUERY /things` returns
the same filtered / ordered / paginated list as `GET /things?…`, but with the
criteria in the body:

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

Permissions and throttles reuse the `list` action's, and admin custom views
declared with method `QUERY` route correctly too.

## Framework guarantees

- **CSRF.** `QUERY` is exempt from CSRF token enforcement by default, alongside
  `GET`/`HEAD`/`OPTIONS`/`TRACE`. Browsers can't form-submit `QUERY`, and a
  cross-origin `fetch` with method `QUERY` is never a CORS-safelisted "simple
  request" — it always triggers a preflight — so there's no ambient-credential
  CSRF vector. Set `CsrfConfig::require_csrf_on_query = true` for pure
  defense-in-depth. Keep `QUERY` handlers side-effect-free, and never add
  `QUERY` to the [method-override](middleware.md) allow-list.
- **CORS.** `QUERY` is in the `permissive()` and settings-derived method lists.
  Because every cross-origin `QUERY` preflights, it must be advertised in
  `Access-Control-Allow-Methods` to work cross-origin.
- **Retries.** The HTTP client (`http_client`) treats `QUERY` as idempotent, so
  it's retried on transient failures like `GET`.
- **Idempotency.** `QUERY` needs no `Idempotency-Key` — the method is
  idempotent by definition.
- **Caching.** `cache_page` can cache `QUERY` responses when you opt in with
  `CachePageLayer::cache_query(true)`. The cache key folds in a digest of the
  request body, and the response is marked `private` so shared caches (which
  can't key on a body) never mis-serve it. See [Caching](caching.md).

## Testing

`TestClient` and `RequestFactory` have `.query()` builders, and the outbound
`HttpClient` can send `QUERY`:

```rust
let resp = client.query("/search").json(&criteria).send().await;
```

## OpenAPI

OpenAPI 3.1 has no `QUERY` operation; [OpenAPI 3.2](https://www.openapis.org/blog/2025/09/23/announcing-openapi-v3-2)
added it as a first-class Path Item field. Rustango emits a `query` operation
when you attach one, and bumps the spec to `openapi: 3.2.0` only for specs that
use it (specs without `QUERY` stay `3.1.0` for maximum tooling compatibility):

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
