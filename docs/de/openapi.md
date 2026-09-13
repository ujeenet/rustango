# OpenAPI

**Rustango** generiert eine **OpenAPI-3.1**-Spec für deine API und liefert sie
mit Swagger UI / Redoc aus — keine von Hand zu pflegenden Annotationen. Richte
den Generator auf die Serializer und ViewSets, die du bereits geschrieben hast:
die Feldschemata kommen aus `#[derive(Serializer)]`, die CRUD-Pfade kommen aus
deinem ViewSet, und ein einzeiliger Router hängt `/openapi.json` + eine
interaktive Docs-Seite ein. Wenn du volle Kontrolle brauchst, lassen dich
dieselben Typen jede beliebige Spec von Hand bauen.

[![Rustango OpenAPI: Serializer-Felder werden zu einem Component-Schema, das ViewSet wird zu CRUD-Pfaden, und openapi_router liefert /openapi.json + Swagger UI aus](../img/openapi.png)](../img/openapi.png)

> **Quelle:** `rustango::openapi` (`OpenApiSpec`, `Schema`, `OpenApiSchema`,
> `PathItem`, `Operation`, `SecurityScheme`, `router::openapi_router`) und
> `ViewSet::openapi_paths` — hinter dem `openapi`-Feature (standardmäßig aktiv;
> der Viewer-Router benötigt zusätzlich `admin`).
>
> **Lauffähige Version:** jedes Snippet unten ist aus dem getesteten
> [`getting_started_blog`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/examples/getting_started_blog/tests/openapi.rs)-Beispiel
> kopiert — `cargo test -p getting_started_blog --test openapi`. Die Builder-,
> Schema- und Router-Typen werden zusätzlich durch die eigenen Unit-Tests des
> Frameworks abgedeckt (`crates/rustango/src/openapi/`).

> **Neu bei einem Begriff hier?** *OpenAPI*, *schema*, *serializer*, *ViewSet* —
> siehe das [Glossar](glossary.md).

---

## Inhaltsverzeichnis
- [Schnellstart](#quick-start) — generieren + ausliefern auf einem Bildschirm
- [Schemata aus Serializern](#schemas-from-serializers) · [Pfade aus ViewSets](#paths-from-viewsets)
- [Ausliefern](#serving-the-spec-swagger-ui--redoc) · [Eine Spec von Hand bauen](#hand-building-a-spec)
- [Security-Schemata](#security-schemes) · [Der `Schema`-Builder](#the-schema-builder)
- [Anmerkungen & Grenzen](#notes-and-limits)

---

## Schnellstart

Generiere ein Component-Schema aus einem Serializer, CRUD-Pfade aus einem
ViewSet und hänge dann den Viewer ein:

```rust
use rustango::core::Model;                  // brings `Post::SCHEMA` into scope
use rustango::openapi::{OpenApiSpec, OpenApiSchema, SecurityScheme};
use rustango::openapi::router::openapi_router;
use rustango::viewset::ViewSet;

let mut spec = OpenApiSpec::new("Blog API", "1.0.0")
    .description("Demo of rustango's OpenAPI generation")
    .server("https://api.example.com", "Production")
    .add_security_scheme("bearerAuth", SecurityScheme::bearer("JWT"))
    .require_security("bearerAuth", [])                  // global default
    .add_schema("Post", PostSerializer::openapi_schema()); // from #[derive(Serializer)]

// CRUD path items straight off the ViewSet:
for (path, item) in ViewSet::for_model(Post::SCHEMA).openapi_paths("/api/posts", "Post") {
    spec = spec.add_path(path, item);
}

// Mount /openapi.json + /docs (Swagger UI) + /redoc:
let app = axum::Router::new().merge(openapi_router(spec));
```

Rufe `/docs` für Swagger UI auf, `/redoc` für Redoc, oder hole das rohe
`/openapi.json` für die Codegenerierung (`openapi-generator`, `oapi-codegen`
usw.).

---

## Schemata aus Serializern

Mit aktivem `openapi`-Feature emittiert `#[derive(Serializer)]` zusätzlich eine
[`OpenApiSchema`]-Impl, sodass jeder Serializer zu einem Component-Schema wird:

```rust
use rustango::openapi::OpenApiSchema;

let schema = PostSerializer::openapi_schema();   // -> rustango::openapi::Schema
spec = spec.add_schema("Post", schema);
```

Das Schema spiegelt die **API-Form** wider, nicht die rohe Tabelle: umbenannte
Felder (`#[serializer(source = "body")]` → `content`), `read_only`- /
`write_only`-Sichtbarkeit, berechnete Methodenfelder und verschachtelte
Serializer tragen sich alle durch. Feldtypen bilden automatisch ab:

| Rust-Feld | OpenAPI |
|---|---|
| `i32` / `i64` | `integer` (`int32` / `int64`) |
| `f32` / `f64` | `number` (`float` / `double`) |
| `String`, `&str` | `string` |
| `bool` | `boolean` |
| `Vec<T>` / `[T; N]` | `array` von `T` |
| `Option<T>` | `T`, als `nullable` markiert |
| `chrono::DateTime<Utc>` | `string` / `date-time` |
| `chrono::NaiveDate` | `string` / `date` |
| `uuid::Uuid` | `string` / `uuid` |
| `Auto<T>` | wie `T` (serverseitig zugewiesen) |
| `HashMap<String, V>` | `object` mit `additionalProperties` |
| `serde_json::Value` | frei geformt (beliebig) |

Für einen **benutzerdefinierten Feldtyp** implementiere `OpenApiSchema` einmal,
und es funktioniert überall dort, wo dieser Typ auftaucht:

```rust
struct Money { cents: i64 }

impl rustango::openapi::OpenApiSchema for Money {
    fn openapi_schema() -> rustango::openapi::Schema {
        rustango::openapi::Schema::integer().description("amount in cents")
    }
}
```

> Siehe den [Serializer-Leitfaden](serializers.md#openapi-schemas) für die
> Feldattribute, die die Ausgabe formen.

---

## Pfade aus ViewSets

`ViewSet::openapi_paths(prefix, schema_ref)` gibt die `(path, PathItem)`-Paare
für die fünf standardmäßigen CRUD-Routen zurück — list, create, retrieve,
update, partial update, delete — verdrahtet, um auf ein registriertes
Component-Schema zu verweisen:

```rust
for (path, item) in ViewSet::for_model(Post::SCHEMA)
    .filter_fields(&["author_id", "status"])
    .search_fields(&["title", "body"])
    .openapi_paths("/api/posts", "Post")
{
    spec = spec.add_path(path, item);
}
```

Es erzeugt `/api/posts` (GET list, POST create) und `/api/posts/{pk}` (GET, PUT,
PATCH, DELETE), und es bleibt mit der Konfiguration des ViewSet synchron:

- **Pagination** → die passenden Query-Parameter + der Umschlag der
  Listenantwort für den konfigurierten Stil (`page`/`page_size`, `cursor` oder
  `limit`/`offset`).
- **Filtering / Suche / Sortierung** → ein `?field=`-Query-Parameter pro
  `filter_fields` (mit den verfügbaren `__gt`/`__in`/…-Lookups beschrieben) plus
  `?search=` und `?ordering=`, wenn konfiguriert.
- **`read_only()`** → die Schreiboperationen (POST/PUT/PATCH/DELETE) werden
  weggelassen.
- **Pfadparameter** → `{pk}`, typisiert aus dem Primärschlüssel des Modells.
- **`operationId`** → `list_post`, `create_post`, … (pro Aktion in snake_case).

> Das ViewSet selbst wird im [ViewSets-Leitfaden](viewsets.md) dokumentiert.

---

## Die Spec ausliefern (Swagger UI / Redoc)

`openapi_router(spec)` hängt drei Routen ein:

| Route | Liefert |
|---|---|
| `GET /openapi.json` | die Spec als JSON (`application/json`) |
| `GET /docs` | Swagger UI (interaktives „try-it-out“) |
| `GET /redoc` | Redoc (saubere Drei-Panel-Referenz) |

```rust
let app = axum::Router::new()
    .merge(PostViewSet::router("/api/posts", pool.clone()))
    .merge(openapi_router(spec));        // adds /openapi.json, /docs, /redoc
```

Die Viewer-Seiten sind winzige HTML-Hüllen, die Swagger UI / Redoc von einem CDN
laden (`unpkg` / `jsdelivr`) — es wird kein JS in **Rustango** gebündelt. Für
ein Air-Gapped-Deployment schreibe `spec.to_json()` beim Start in eine Datei und
hoste die Viewer-Assets aus deinem eigenen Static-Verzeichnis selbst.

---

## Eine Spec von Hand bauen

Wenn deine API nicht ViewSet-förmig ist, baue die Spec direkt — dieselben Typen,
volle OpenAPI-3.1-Treue:

```rust
use rustango::openapi::{OpenApiSpec, PathItem, Operation, Response, Parameter, Schema};

let spec = OpenApiSpec::new("My API", "1.0.0")
    .description("Customer-facing public API")
    .add_schema("Post", Schema::object()
        .property("id", Schema::integer())
        .property("title", Schema::string())
        .required(["id", "title"]))
    .add_path("/posts/{id}", PathItem::new()
        .parameter(Parameter::path("id", Schema::integer()))
        .get(Operation::new()
            .summary("Get a post")
            .operation_id("get_post")
            .tag("posts")
            .response("200", Response::new("OK")
                .json_content(Schema::ref_("Post")))
            .response("404", Response::new("not found"))));

let json = spec.to_json();   // pretty-printed OpenAPI 3.1
```

`OpenApiSpec` nimmt außerdem `.contact(...)`, `.license(...)`, `.add_tag(...)`
und mehrere `.server(...)`-Einträge entgegen.

---

## Security-Schemata

Deklariere, wie sich die API authentifiziert, und fordere dann ein Schema global
(Overrides pro Operation gewinnen):

```rust
use rustango::openapi::SecurityScheme;

let spec = OpenApiSpec::new("API", "1.0")
    .add_security_scheme("bearerAuth", SecurityScheme::bearer("JWT"))
    .add_security_scheme("apiKey", SecurityScheme::api_key_header("X-API-Key"))
    .require_security("bearerAuth", []);     // default for every operation
```

Helfer: `SecurityScheme::bearer(fmt)`, `basic()`, `api_key_header(name)`,
`api_key_query(name)`, `oauth2_authorization_code(auth_url, token_url, scopes)`.
An einer `Operation` markiert `.no_security()` einen öffentlichen Endpunkt und
`.require_security(scheme, scopes)` überschreibt den globalen Default. Diese
passen zu **Rustango**s eigener Auth (siehe den
[Security-Leitfaden](security.md#authenticating-users)): JWT → `bearer`,
API-Keys → `apiKey`, OAuth2 → `oauth2`.

---

## Der `Schema`-Builder

`Schema` ist ein flüssiger JSON-Schema-Builder (OpenAPI-3.1-Teilmenge):

```rust
Schema::object()
    .property("id", Schema::integer())                       // int64
    .property("email", Schema::email())                      // string/email
    .property("status", Schema::string().enum_(["draft", "published"]))
    .property("tags", Schema::array_of(Schema::string()))
    .property("created", Schema::datetime().nullable())
    .required(["id", "email"]);
```

Typkonstruktoren: `string` · `integer` / `int32` · `number` · `boolean` ·
`object` · `array_of(..)` · `any_object` · `datetime` / `date` / `time` · `uuid`
· `decimal` · `binary` · `email` · `uri` · `ref_("Name")`. Modifikatoren:
`.property`, `.required`, `.nullable`, `.enum_`, `.format`, `.description`,
`.example`, `.default_value`, `.min_length` / `.max_length`, `.minimum` /
`.maximum`.

---

## Anmerkungen und Grenzen

- **Es ist OpenAPI 3.1** (`"openapi": "3.1.0"`), was sich an JSON Schema 2020-12
  ausrichtet — die meisten modernen Werkzeuge konsumieren es direkt.
- **Der Viewer-Router benötigt das `admin`-Feature** (für axum); `openapi`
  allein reicht, um eine Spec zu *bauen* und zu serialisieren
  (`spec.to_json()`).
- **Viewer laden von einem CDN** — in Ordnung für interne/Dev-Docs; hoste die
  Assets selbst für Offline- oder CSP-gesperrte Deployments.
- **`openapi_paths` deckt die Standard-CRUD-Form ab.** Benutzerdefinierte
  Aktionen oder von Hand gebaute Endpunkte fügst du mit `add_path` hinzu (Abschnitt
  „Von Hand bauen“ oben).
- Regeneriere Client-SDKs aus `/openapi.json` in der CI, damit sie nie vom Server
  abdriften.
