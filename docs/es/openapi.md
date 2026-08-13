# OpenAPI

**Rustango** genera una especificación **OpenAPI 3.1** para tu API y la sirve con
Swagger UI / Redoc — sin anotaciones que mantener a mano. Apunta el generador a los
serializadores y ViewSets que ya escribiste: los esquemas de campo vienen de
`#[derive(Serializer)]`, las rutas CRUD vienen de tu ViewSet, y un router de una línea
monta `/openapi.json` + una página de documentación interactiva. Cuando necesitas
control total, los mismos tipos te permiten construir a mano cualquier especificación.

[![OpenAPI de Rustango: los campos del serializador se convierten en un esquema de componente, el ViewSet se convierte en rutas CRUD, y openapi_router sirve /openapi.json + Swagger UI](img/openapi.png)](img/openapi.png)

> **Fuente:** `rustango::openapi` (`OpenApiSpec`, `Schema`, `OpenApiSchema`,
> `PathItem`, `Operation`, `SecurityScheme`, `router::openapi_router`) y
> `ViewSet::openapi_paths` — tras la característica `openapi` (activada por defecto; el
> router del visor también necesita `admin`).
>
> **Versión ejecutable:** cada fragmento de abajo está copiado del ejemplo probado
> [`getting_started_blog`](../crates/rustango/examples/getting_started_blog/tests/openapi.rs)
> — `cargo test -p getting_started_blog --test openapi`. Los tipos del builder, del
> esquema y del router están cubiertos además por las propias pruebas unitarias del
> framework (`crates/rustango/src/openapi/`).

> **¿Algún término nuevo aquí?** *OpenAPI*, *schema*, *serializer*, *ViewSet* — consulta
> el [glosario](glossary.md).

---

## Tabla de contenidos
- [Inicio rápido](#quick-start) — genera + sirve en una sola pantalla
- [Esquemas desde serializadores](#schemas-from-serializers) · [Rutas desde ViewSets](#paths-from-viewsets)
- [Servirlo](#serving-the-spec-swagger-ui--redoc) · [Construir una especificación a mano](#hand-building-a-spec)
- [Esquemas de seguridad](#security-schemes) · [El builder `Schema`](#the-schema-builder)
- [Notas y límites](#notes-and-limits)

---

## Inicio rápido

Genera un esquema de componente desde un serializador, rutas CRUD desde un ViewSet, y
luego monta el visor:

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

Visita `/docs` para Swagger UI, `/redoc` para Redoc, u obtén el `/openapi.json` crudo
para codegen (`openapi-generator`, `oapi-codegen`, etc.).

---

## Esquemas desde serializadores

Con la característica `openapi` activada, `#[derive(Serializer)]` también emite una
impl de [`OpenApiSchema`], de modo que cualquier serializador se convierte en un
esquema de componente:

```rust
use rustango::openapi::OpenApiSchema;

let schema = PostSerializer::openapi_schema();   // -> rustango::openapi::Schema
spec = spec.add_schema("Post", schema);
```

El esquema refleja la **forma de la API**, no la tabla cruda: los campos renombrados
(`#[serializer(source = "body")]` → `content`), la visibilidad `read_only` /
`write_only`, los campos de método calculados y los serializadores anidados se
trasladan todos. Los tipos de campo se mapean automáticamente:

| Campo Rust | OpenAPI |
|---|---|
| `i32` / `i64` | `integer` (`int32` / `int64`) |
| `f32` / `f64` | `number` (`float` / `double`) |
| `String`, `&str` | `string` |
| `bool` | `boolean` |
| `Vec<T>` / `[T; N]` | `array` of `T` |
| `Option<T>` | `T`, marcado `nullable` |
| `chrono::DateTime<Utc>` | `string` / `date-time` |
| `chrono::NaiveDate` | `string` / `date` |
| `uuid::Uuid` | `string` / `uuid` |
| `Auto<T>` | igual que `T` (asignado por el servidor) |
| `HashMap<String, V>` | `object` con `additionalProperties` |
| `serde_json::Value` | libre (cualquiera) |

Para un **tipo de campo personalizado**, implementa `OpenApiSchema` una vez y funciona
en todas partes donde aparezca ese tipo:

```rust
struct Money { cents: i64 }

impl rustango::openapi::OpenApiSchema for Money {
    fn openapi_schema() -> rustango::openapi::Schema {
        rustango::openapi::Schema::integer().description("amount in cents")
    }
}
```

> Consulta la [guía de Serializadores](serializers.md#openapi-schemas) para los
> atributos de campo que dan forma a la salida.

---

## Rutas desde ViewSets

`ViewSet::openapi_paths(prefix, schema_ref)` devuelve los pares `(path, PathItem)` para
las cinco rutas CRUD estándar — list, create, retrieve, update, partial update,
delete — conectadas para referenciar un esquema de componente registrado:

```rust
for (path, item) in ViewSet::for_model(Post::SCHEMA)
    .filter_fields(&["author_id", "status"])
    .search_fields(&["title", "body"])
    .openapi_paths("/api/posts", "Post")
{
    spec = spec.add_path(path, item);
}
```

Produce `/api/posts` (GET list, POST create) y `/api/posts/{pk}` (GET, PUT, PATCH,
DELETE), y se mantiene sincronizado con la configuración del ViewSet:

- **Paginación** → los parámetros de consulta correctos + el envoltorio de respuesta
  de lista para el estilo configurado (`page`/`page_size`, `cursor`, o
  `limit`/`offset`).
- **Filtrado / búsqueda / ordenación** → un parámetro de consulta `?field=` por cada
  `filter_fields` (con los lookups disponibles `__gt`/`__in`/… descritos), más
  `?search=` y `?ordering=` cuando están configurados.
- **`read_only()`** → las operaciones de escritura (POST/PUT/PATCH/DELETE) se omiten.
- **Parámetro de ruta** → `{pk}` tipado a partir de la clave primaria del modelo.
- **`operationId`** → `list_post`, `create_post`, … (en snake-case por acción).

> El ViewSet en sí está documentado en la [guía de ViewSets](viewsets.md).

---

## Servir la especificación (Swagger UI / Redoc)

`openapi_router(spec)` monta tres rutas:

| Ruta | Sirve |
|---|---|
| `GET /openapi.json` | la especificación como JSON (`application/json`) |
| `GET /docs` | Swagger UI (probar interactivamente) |
| `GET /redoc` | Redoc (referencia limpia de tres paneles) |

```rust
let app = axum::Router::new()
    .merge(PostViewSet::router("/api/posts", pool.clone()))
    .merge(openapi_router(spec));        // adds /openapi.json, /docs, /redoc
```

Las páginas del visor son diminutas cáscaras HTML que cargan Swagger UI / Redoc desde
un CDN (`unpkg` / `jsdelivr`) — no se empaqueta ningún JS en **Rustango**. Para un
despliegue aislado (air-gapped), escribe `spec.to_json()` en un archivo al arrancar y
auto-hospeda los assets del visor desde tu propio directorio estático.

---

## Construir una especificación a mano

Cuando tu API no tiene forma de ViewSet, construye la especificación directamente — los
mismos tipos, fidelidad completa a OpenAPI 3.1:

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

`OpenApiSpec` también acepta `.contact(...)`, `.license(...)`, `.add_tag(...)` y
múltiples entradas `.server(...)`.

---

## Esquemas de seguridad

Declara cómo se autentica la API, luego exige un esquema globalmente (los overrides
por operación ganan):

```rust
use rustango::openapi::SecurityScheme;

let spec = OpenApiSpec::new("API", "1.0")
    .add_security_scheme("bearerAuth", SecurityScheme::bearer("JWT"))
    .add_security_scheme("apiKey", SecurityScheme::api_key_header("X-API-Key"))
    .require_security("bearerAuth", []);     // default for every operation
```

Helpers: `SecurityScheme::bearer(fmt)`, `basic()`, `api_key_header(name)`,
`api_key_query(name)`, `oauth2_authorization_code(auth_url, token_url, scopes)`. En una
`Operation`, `.no_security()` marca un endpoint público y
`.require_security(scheme, scopes)` sobrescribe el valor global por defecto. Estos se
alinean con la propia autenticación de **Rustango** (consulta la
[guía de Seguridad](security.md#authenticating-users)): JWT → `bearer`, API keys →
`apiKey`, OAuth2 → `oauth2`.

---

## El builder `Schema`

`Schema` es un builder fluido de JSON-Schema (subconjunto de OpenAPI 3.1):

```rust
Schema::object()
    .property("id", Schema::integer())                       // int64
    .property("email", Schema::email())                      // string/email
    .property("status", Schema::string().enum_(["draft", "published"]))
    .property("tags", Schema::array_of(Schema::string()))
    .property("created", Schema::datetime().nullable())
    .required(["id", "email"]);
```

Constructores de tipos: `string` · `integer` / `int32` · `number` · `boolean` ·
`object` · `array_of(..)` · `any_object` · `datetime` / `date` / `time` · `uuid` ·
`decimal` · `binary` · `email` · `uri` · `ref_("Name")`. Modificadores:
`.property`, `.required`, `.nullable`, `.enum_`, `.format`, `.description`,
`.example`, `.default_value`, `.min_length` / `.max_length`, `.minimum` /
`.maximum`.

---

## Notas y límites

- **Es OpenAPI 3.1** (`"openapi": "3.1.0"`), que se alinea con JSON Schema 2020-12 — la
  mayoría del tooling moderno lo consume directamente.
- **El router del visor necesita la característica `admin`** (para axum); `openapi` por
  sí solo basta para *construir* y serializar una especificación (`spec.to_json()`).
- **Los visores cargan desde un CDN** — está bien para documentación interna/de
  desarrollo; auto-hospeda los assets para despliegues offline o bloqueados por CSP.
- **`openapi_paths` cubre la forma CRUD estándar.** Las acciones personalizadas, o los
  endpoints hechos a mano, los añades con `add_path` (sección de construir a mano de
  arriba).
- Regenera los SDKs de cliente desde `/openapi.json` en CI para que nunca se
  desincronicen del servidor.
