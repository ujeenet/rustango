# Die QUERY-Methode

`QUERY` ([RFC 10008](https://www.rfc-editor.org/rfc/rfc10008), ein im Juni 2026
veröffentlichter Proposed Standard) ist das „sichere GET mit Body". Sie ist
**sicher** und **idempotent** wie `GET`, doch die Suchkriterien reisen im
Anfrage-Body statt in der URL — so muss eine komplexe Suche nicht in einen
Querystring gepresst werden, und es gibt keine URL-Längenobergrenze. **Rustango**
routet `QUERY` neben `GET` im gesamten Framework: Routing, einen
methodenadaptiven Extraktor, CSRF-/CORS-/Retry-Policy, Caching pro View, ViewSets,
den Testclient und OpenAPI.

> **Quelle:** `rustango::http_query` (`query`, `QueryRouterExt`, `QUERY`) und
> `rustango::params` (`Params`) — hinter dem `admin`-Feature. Verwandte Oberfläche:
> `viewset::ViewSet`, `cache_page::CachePageLayer::cache_query`,
> `forms::csrf::CsrfConfig::require_csrf_on_query`, `cors::CorsLayer`,
> `test_client` / `http_client`.

## Wann man dazu greift

Verwenden Sie `QUERY` anstelle von `GET`, wenn die Suchkriterien groß oder
strukturiert sind:

- Filtermengen, die einen Querystring sprengen würden (lange `IN`-Listen, viele
  Facetten).
- Verschachtelte / strukturierte Kriterien, die sich nicht sauber auf
  `?key=value`-Paare abbilden lassen — senden Sie sie als JSON-Body.
- Alles, was Sie versucht wären als `POST /search` zu modellieren, obwohl es keinen
  Zustand liest und nichts ändert — `QUERY` sagt „dies ist ein sicherer,
  cachebarer Lesevorgang" bereits in der Methode selbst.

Verwenden Sie ein einfaches `GET` für einfache, kurze, mit Lesezeichen versehbare
Anfragen. `QUERY` ist additiv: Derselbe Handler kann beide bedienen.

## Routing

axum 0.8 kann `QUERY` nicht nativ routen (sein `MethodFilter` ist eine geschlossene
Menge — [tokio-rs/axum#3799](https://github.com/tokio-rs/axum/issues/3799)), daher
stellt rustango `query()` und eine `.query()`-Kette bereit, die axums eigene
`get()` / `post()` widerspiegeln:

```rust
use rustango::http_query::{query, QueryRouterExt};
use axum::routing::get;

let app = axum::Router::new()
    // QUERY-only route.
    .route("/search", query(search))
    // GET + QUERY on one path — chain `.query()` last.
    .route("/products", get(list_products).query(search_products));
```

Ein `405` auf einer gemischten Route meldet die vollständige Methodenmenge, z. B.
`Allow: GET,HEAD,QUERY`.

## Ein Handler, beide Transporte

`Params<T>` liest `T` bei `GET`/`HEAD` aus dem Querystring und bei `QUERY` aus dem
Anfrage-Body, sodass ein einziger Handler beide ohne Verzweigung bedient:

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

`GET /search?q=hi` und `QUERY /search` mit dem Body `q=hi` erreichen `search` und
deserialisieren identisch. Bei `QUERY` wird der Body nach `Content-Type` geparst:

| `Content-Type` | Geparst als | Anmerkungen |
|---|---|---|
| `application/x-www-form-urlencoded` (oder keiner) | `serde_urlencoded` | Derselbe Codepfad wie der Querystring — flach, ein einzelner Wert pro Schlüssel. |
| `application/json` (oder ein `…+json`-Suffix) | `serde_json` | Für Arrays / verschachtelte Kriterien verwenden. |
| alles andere | — | `415 Unsupported Media Type` |

Die Statuscodes entsprechen axums Konventionen: Ein Parsefehler des Querystrings
ist ein `400`, ein Parsefehler des Bodys ein `422`, und eine andere Methode als
GET/HEAD/QUERY ein `405`.

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

Ein `ViewSet` erhält kostenlos eine `QUERY`-Sammlungsaktion — `QUERY /things` liefert
dieselbe gefilterte / geordnete / paginierte Liste wie `GET /things?…`, aber mit den
Kriterien im Body:

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

Berechtigungen und Throttles nutzen die der `list`-Aktion wieder, und
benutzerdefinierte Admin-Views, die mit der Methode `QUERY` deklariert sind, routen
ebenfalls korrekt.

## Framework-Garantien

- **CSRF.** `QUERY` ist standardmäßig von der CSRF-Token-Durchsetzung befreit,
  neben `GET`/`HEAD`/`OPTIONS`/`TRACE`. Browser können `QUERY` nicht per Formular
  absenden, und ein cross-origin `fetch` mit der Methode `QUERY` ist niemals eine
  CORS-safelisted „einfache Anfrage" — sie löst immer einen Preflight aus — es gibt
  also keinen CSRF-Vektor über Ambient-Credentials. Setzen Sie
  `CsrfConfig::require_csrf_on_query = true` für reine Defense-in-Depth. Halten Sie
  `QUERY`-Handler nebenwirkungsfrei, und fügen Sie `QUERY` niemals zur Allow-List
  des [Method-Overrides](middleware.md) hinzu.
- **CORS.** `QUERY` ist in den `permissive()`- und aus den Einstellungen
  abgeleiteten Methodenlisten enthalten. Da jedes cross-origin `QUERY` einen
  Preflight durchläuft, muss es in `Access-Control-Allow-Methods` beworben werden,
  um cross-origin zu funktionieren.
- **Retries.** Der HTTP-Client (`http_client`) behandelt `QUERY` als idempotent, es
  wird also bei transienten Fehlern wie `GET` erneut versucht.
- **Idempotenz.** `QUERY` benötigt keinen `Idempotency-Key` — die Methode ist
  per Definition idempotent.
- **Caching.** `cache_page` kann `QUERY`-Antworten cachen, wenn Sie dies mit
  `CachePageLayer::cache_query(true)` aktivieren. Der Cache-Schlüssel bezieht einen
  Digest des Anfrage-Bodys mit ein, und die Antwort wird als `private` markiert,
  damit geteilte Caches (die nicht auf einem Body basieren können) sie niemals
  fehlerhaft ausliefern. Siehe [Caching](caching.md).

## Testing

`TestClient` und `RequestFactory` verfügen über `.query()`-Builder, und der
ausgehende `HttpClient` kann `QUERY` senden:

```rust
let resp = client.query("/search").json(&criteria).send().await;
```

## OpenAPI

OpenAPI 3.1 hat keine `QUERY`-Operation; [OpenAPI 3.2](https://www.openapis.org/blog/2025/09/23/announcing-openapi-v3-2)
hat sie als erstklassiges Path-Item-Feld hinzugefügt. Rustango gibt eine
`query`-Operation aus, wenn Sie eine anhängen, und hebt die Spezifikation nur für
Spezifikationen, die sie verwenden, auf `openapi: 3.2.0` an (Spezifikationen ohne
`QUERY` bleiben auf `3.1.0` für maximale Tooling-Kompatibilität):

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
