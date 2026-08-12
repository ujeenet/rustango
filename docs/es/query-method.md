# El método QUERY

`QUERY` ([RFC 10008](https://www.rfc-editor.org/rfc/rfc10008), un Proposed Standard
publicado en junio de 2026) es el «GET seguro con cuerpo». Es **seguro** e
**idempotente** como `GET`, pero los criterios de búsqueda viajan en el cuerpo de
la petición en lugar de en la URL — así una búsqueda compleja no tiene que
comprimirse en una cadena de consulta, y no hay un límite de longitud de URL.
**Rustango** enruta `QUERY` junto a `GET` en todo el framework: el enrutamiento, un
extractor adaptativo al método, la política de CSRF/CORS/reintentos, el caché por
vista, los ViewSets, el cliente de pruebas y OpenAPI.

> **Fuente:** `rustango::http_query` (`query`, `QueryRouterExt`, `QUERY`) y
> `rustango::params` (`Params`) — detrás de la característica `admin`. Superficie
> relacionada: `viewset::ViewSet`, `cache_page::CachePageLayer::cache_query`,
> `forms::csrf::CsrfConfig::require_csrf_on_query`, `cors::CorsLayer`,
> `test_client` / `http_client`.

## Cuándo recurrir a él

Usa `QUERY` en lugar de `GET` cuando los criterios de búsqueda sean grandes o
estructurados:

- Conjuntos de filtros que rebasarían una cadena de consulta (listas `IN` largas,
  muchas facetas).
- Criterios anidados / estructurados que no se mapean limpiamente a pares
  `?key=value` — envíalos como un cuerpo JSON.
- Cualquier cosa que estarías tentado a modelar como un `POST /search` aunque no
  lea ningún estado ni cambie nada — `QUERY` dice «esta es una lectura segura y
  cacheable» en el propio método.

Usa un `GET` simple para peticiones sencillas, cortas y guardables en marcadores.
`QUERY` es aditivo: el mismo handler puede servir ambos.

## Enrutamiento

axum 0.8 no puede enrutar `QUERY` de forma nativa (su `MethodFilter` es un conjunto
cerrado — [tokio-rs/axum#3799](https://github.com/tokio-rs/axum/issues/3799)), así
que rustango proporciona `query()` y una cadena `.query()` que reflejan los propios
`get()` / `post()` de axum:

```rust
use rustango::http_query::{query, QueryRouterExt};
use axum::routing::get;

let app = axum::Router::new()
    // QUERY-only route.
    .route("/search", query(search))
    // GET + QUERY on one path — chain `.query()` last.
    .route("/products", get(list_products).query(search_products));
```

Un `405` en una ruta mixta reporta el conjunto completo de métodos, p. ej.
`Allow: GET,HEAD,QUERY`.

## Un handler, ambos transportes

`Params<T>` lee `T` desde la cadena de consulta en `GET`/`HEAD` y desde el cuerpo de
la petición en `QUERY`, de modo que un único handler sirve ambos sin ramificación:

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

`GET /search?q=hi` y `QUERY /search` con el cuerpo `q=hi` alcanzan `search` y se
deserializan de forma idéntica. En `QUERY` el cuerpo se analiza según el
`Content-Type`:

| `Content-Type` | Analizado como | Notas |
|---|---|---|
| `application/x-www-form-urlencoded` (o ninguno) | `serde_urlencoded` | Mismo codepath que la cadena de consulta — plano, un único valor por clave. |
| `application/json` (o un sufijo `…+json`) | `serde_json` | Úsalo para arrays / criterios anidados. |
| cualquier otra cosa | — | `415 Unsupported Media Type` |

Los códigos de estado coinciden con las convenciones de axum: un error de análisis
de la cadena de consulta es un `400`, un error de análisis del cuerpo es un `422`, y
un método distinto de GET/HEAD/QUERY es un `405`.

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

Un `ViewSet` obtiene gratis una acción de colección `QUERY` — `QUERY /things` devuelve
la misma lista filtrada / ordenada / paginada que `GET /things?…`, pero con los
criterios en el cuerpo:

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

Los permisos y throttles reutilizan los de la acción `list`, y las vistas de
administración personalizadas declaradas con el método `QUERY` también se enrutan
correctamente.

## Garantías del framework

- **CSRF.** `QUERY` está exento por defecto de la aplicación de tokens CSRF, junto a
  `GET`/`HEAD`/`OPTIONS`/`TRACE`. Los navegadores no pueden enviar `QUERY` mediante
  formulario, y un `fetch` de origen cruzado con el método `QUERY` nunca es una
  «petición simple» de la lista blanca de CORS — siempre desencadena un preflight —
  por lo que no hay un vector CSRF por credenciales ambientales. Establece
  `CsrfConfig::require_csrf_on_query = true` para pura defensa en profundidad. Mantén
  los handlers `QUERY` sin efectos secundarios, y nunca añadas `QUERY` a la lista de
  permitidos de la [sobrescritura de método](middleware.md).
- **CORS.** `QUERY` está en las listas de métodos `permissive()` y derivadas de la
  configuración. Como cada `QUERY` de origen cruzado hace un preflight, debe
  anunciarse en `Access-Control-Allow-Methods` para funcionar en origen cruzado.
- **Reintentos.** El cliente HTTP (`http_client`) trata `QUERY` como idempotente,
  así que se reintenta ante fallos transitorios como `GET`.
- **Idempotencia.** `QUERY` no necesita ningún `Idempotency-Key` — el método es
  idempotente por definición.
- **Caché.** `cache_page` puede cachear las respuestas `QUERY` cuando lo habilitas
  con `CachePageLayer::cache_query(true)`. La clave de caché incorpora un resumen del
  cuerpo de la petición, y la respuesta se marca como `private` para que los cachés
  compartidos (que no pueden basarse en un cuerpo) nunca la sirvan erróneamente. Ver
  [Caché](caching.md).

## Pruebas

`TestClient` y `RequestFactory` tienen constructores `.query()`, y el `HttpClient`
saliente puede enviar `QUERY`:

```rust
let resp = client.query("/search").json(&criteria).send().await;
```

## OpenAPI

OpenAPI 3.1 no tiene una operación `QUERY`; [OpenAPI 3.2](https://www.openapis.org/blog/2025/09/23/announcing-openapi-v3-2)
la añadió como un campo Path Item de primera clase. Rustango emite una operación
`query` cuando adjuntas una, y sube la especificación a `openapi: 3.2.0` solo para
las especificaciones que la usan (las especificaciones sin `QUERY` permanecen en
`3.1.0` para máxima compatibilidad con las herramientas):

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
