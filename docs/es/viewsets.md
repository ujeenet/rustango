# ViewSets — APIs REST CRUD

Un ViewSet convierte un modelo en un recurso REST completo — endpoints para
**listar, crear, leer, actualizar y eliminar** registros — a partir de una sola
declaración. (Es el equivalente en **Rustango** de un `ModelViewSet` de Django
REST Framework o de un controlador de recurso de API de Laravel, si has usado
alguno de esos.)

> **¿Nuevo en las APIs REST?** Esta guía asume que sabes qué es un *endpoint*, un
> *verbo HTTP* (GET / POST / …) y una *petición y respuesta JSON*. Si algo de eso
> te resulta confuso, el [glosario](glossary.md#web-api-basics) es una
> introducción de cinco minutos — léelo primero y luego vuelve aquí.

Empareja un ViewSet con un [serializador](serializers.md) — la pieza que da forma
a tu JSON — y protege **ambas direcciones** a la vez: el serializador formatea
cada **respuesta** (renombra, oculta, calcula o anida campos) *y* gobierna cada
**petición** (valida los datos entrantes e ignora silenciosamente los campos que
un cliente no debería poder establecer). La entrada rechazada regresa con la
forma familiar de DRF — un objeto JSON indexado por nombre de campo. Todo
funciona igual en PostgreSQL, MySQL y SQLite.

Esta guía es ante todo un tutorial: **construimos una API REST de blog completa**
de principio a fin — scaffolding, modelos, un serializador, el ViewSet, los seis
endpoints CRUD, validación de entrada, filtrado/búsqueda/paginación y pruebas —
y luego el resto de la página es una referencia de cada perilla.

[![Un ViewSet de Rustango conectado a un serializador: un solo bloque #[viewset(serializer = …)] da salida JSON tipada y entrada validada en las seis rutas CRUD](../img/viewsets.png)](../img/viewsets.png)

> **Fuente:** `rustango::viewset` (`ViewSet`, `#[derive(ViewSet)]`, las opciones
> `#[viewset(...)]` + el builder `for_model`) — siempre compilado.
>
> **Versión ejecutable:** el blog construido aquí refleja el ejemplo probado y
> compilable [`getting_started_blog`](https://github.com/ujeenet/rustango/tree/main/crates/rustango/examples/getting_started_blog)
> (sus `Post` / `PostSerializer` / `PostViewSet`), y cada comportamiento está
> fijado por las propias pruebas en vivo del framework — `crates/rustango/tests/viewset_*.rs`
> (en particular `viewset_serializer_render_sqlite_live` y
> `viewset_serializer_input_sqlite_live`).

---

## Table of contents
- [Vistas de API vs vistas HTML](#api-views-vs-html-views) — ¿JSON para clientes o páginas HTML?
- [Construir una API REST de blog](#build-a-rest-blog-api) — el recorrido completo
- [El matrimonio con el serializador: entrada + salida](#the-serializer-marriage-input--output)
- [Las dos formas de definir un ViewSet](#the-two-ways-to-define-a-viewset)
- [Los endpoints CRUD](#the-crud-endpoints) · [Elegir cuáles exponer](#choosing-which-operations-to-expose)
- [Referencia de `#[viewset(...)]`](#viewset-attribute-reference) · [Referencia del builder](#builder-reference)
- [Filtrado, búsqueda y ordenación](#filtering-search-and-ordering) · [Paginación](#pagination)
- [Validación](#validation) · [Permisos y throttling](#permissions-and-throttling) · [Acciones personalizadas](#custom-actions-beyond-crud)
- [Montaje](#mounting) · [Backends](#backend-support)

---

## API views vs HTML views

Antes del tutorial, una bifurcación en el camino. **Rustango** tiene dos formas
de convertir un modelo en endpoints, y un ViewSet es una de ellas:

- Un **ViewSet** (esta guía) es una **vista de API** — habla **JSON**, para
  frameworks de frontend, apps móviles y otros servicios.
- Una **vista de plantilla** ([vistas HTML](html-views.md)) es una **vista HTML** —
  renderiza **páginas del lado del servidor** a través de Tera, para navegadores
  y sitios renderizados en el servidor.

El mismo modelo por debajo; lo que difiere es qué sale y quién llama.

| | **Vista de API** — ViewSet (aquí) | **Vista HTML** — [vistas de plantilla](html-views.md) |
|---|---|---|
| Módulo | `rustango::viewset` | `rustango::template_views` |
| Devuelve | **datos JSON** | una **página HTML renderizada en el servidor** |
| Diseñado para | SPAs, móvil, otros servicios | navegadores, sitios renderizados en el servidor, CRUD estilo admin |
| Un "crear" | `POST` JSON → `201` + el objeto | `POST` de un formulario → redirección `303` (Post/Redirect/Get) |
| Ante entrada inválida | `400` + un mapa de errores JSON indexado por campo | re-renderiza el formulario con los errores mostrados |
| Un "listar" es | un sobre JSON paginado | un bucle sobre filas en tu plantilla |
| Normalmente autenticado por | tokens / JWT / claves de API | cookies de sesión |
| Análogo en Django | `ModelViewSet` de DRF | vistas genéricas basadas en clases |

Elige por recurso — y puedes montar **ambos sobre el mismo modelo** (una API JSON
pública *y* páginas CRUD internas). El resto de esta guía es el lado JSON/API;
para el lado HTML consulta [Vistas HTML — páginas renderizadas en el servidor](html-views.md).

---

## Build a REST blog API

Construiremos un blog con dos modelos — `Author` y `Post` — y expondremos `Post`
como un recurso REST en `/api/posts` cuya forma JSON y validación están
gobernadas por un serializador. Al final podrás hacer `curl` a cada verbo CRUD y
ver cómo el serializador da forma a la salida y rechaza la entrada inválida.

Este recorrido asume un proyecto creado con `cargo rustango new myblog`
(consulta [Primeros pasos](getting-started.md) para la configuración del proyecto
y la base de datos). Cada paso es un comando o archivo real.

### Step 1 — Create the blog app

Las apps son módulos de funcionalidad autocontenidos (el `startapp` de Django):

```bash
cargo run -- startapp blog
```

Eso escribe `src/blog/{mod,models,views,urls,tests}.rs` y conecta el módulo
dentro de `main.rs` + el agregador `urls::api()`.

### Step 2 — Define the models

`src/blog/models.rs` — un `Author` y un `Post` (una clave foránea los enlaza):

```rust
use rustango::{Auto, Model};
use chrono::{DateTime, Utc};

#[derive(Model, Clone, Debug)]
#[rustango(table = "authors", display = "name")]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 120)]
    pub name: String,
    #[rustango(max_length = 200)]
    pub email: String,
}

#[derive(Model, Clone, Debug)]
#[rustango(table = "posts", display = "title", index("status, published_at"))]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(max_length = 200)]
    pub title: String,

    pub body: String,

    #[rustango(max_length = 20, default = "'draft'")]
    pub status: String,                       // draft | published | archived

    #[rustango(fk = "authors", on = "id")]
    pub author_id: i64,

    #[rustango(auto_now_add)]
    pub published_at: Auto<DateTime<Utc>>,
}
```

### Step 3 — Migrate

Genera y aplica la migración (igual que `makemigrations` + `migrate`):

```bash
cargo run -- makemigrations
cargo run -- migrate
```

### Step 4 — Scaffold the serializer

El serializador es lo que hace de esto una API *DRF* — define el contrato de
petición/respuesta. Genera el esqueleto:

```bash
cargo run -- make:serializer PostSerializer --model Post
```

Luego complétalo. Este ejercita toda la superficie de entrada+salida — un
renombrado, un campo calculado de solo lectura, un campo de servidor de solo
lectura y un validador de campo:

```rust
// src/blog/post_serializer.rs
use rustango::{Auto, Serializer};
use chrono::{DateTime, Utc};
use crate::blog::models::Post;

#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
pub struct PostSerializer {
    pub id: Auto<i64>,

    #[serializer(validate = "title_min_3")]   // input: reject titles < 3 chars
    pub title: String,

    #[serializer(source = "body")]            // JSON key `content`, column `body`
    pub content: String,

    pub status: String,
    pub author_id: i64,

    #[serializer(method = "summary")]         // output: computed, never written
    pub summary: String,

    #[serializer(read_only)]                  // output: shown, ignored on write
    pub published_at: Auto<DateTime<Utc>>,
}

impl PostSerializer {
    fn title_min_3(t: &String) -> Result<(), String> {
        if t.chars().count() < 3 {
            Err("title must be at least 3 characters".into())
        } else {
            Ok(())
        }
    }
    fn summary(p: &Post) -> String {
        p.body.chars().take(80).collect::<String>()
    }
}
```

Registra el módulo — añade `pub mod post_serializer;` a `src/blog/mod.rs`.

Nota que solo escribimos un validador (`title_min_3`); los campos también
**heredan las restricciones del modelo** automáticamente — a `title` se le
verifica la longitud contra el `max_length = 200` del modelo, y una columna con
`choices`/`min`/`max` también se verificaría, todo devolviendo `400`s amigables
en la escritura. Añade los atributos de serializador `max_length` / `min_length` /
`min` / `max` para sobrescribir el límite de un campo. (Consulta la
[guía de serializadores](serializers.md#validation) para la historia completa de
validación.)

### Step 5 — Scaffold the ViewSet and wire the serializer

```bash
cargo run -- make:viewset PostViewSet --model Post
```

Edítalo para declarar el recurso y **conectar el serializador con el atributo
`serializer`** — esa única línea activa la salida *y* la entrada gobernadas por
el serializador:

```rust
// src/blog/post_view_set.rs
use rustango::ViewSet;

#[derive(ViewSet)]
#[viewset(
    model         = Post,
    serializer    = crate::blog::post_serializer::PostSerializer,
    filter_fields = "author_id, status",
    search_fields = "title, body",
    ordering      = "-published_at",
    page_size     = 20,
)]
pub struct PostViewSet;
```

Añade `pub mod post_view_set;` a `src/blog/mod.rs`.

> Con un serializador conectado no necesitas `fields = "..."` — el serializador
> es la proyección. Usa `fields` solo cuando quieras la proyección de campos por
> defecto (sin serializador) en su lugar.

### Step 6 — Mount the routes

En un proyecto de un solo inquilino, anida el router del ViewSet bajo una ruta,
pasando el pool:

```rust
// src/blog/urls.rs (or your urls::api aggregator)
use axum::Router;
use rustango::sql::sqlx::PgPool;
use crate::blog::post_view_set::PostViewSet;

pub fn api(pool: PgPool) -> Router {
    Router::new()
        .merge(PostViewSet::router("/api/posts", pool))
}
```

`make:api_routes blog` genera exactamente este agregador si prefieres
generarlo. Conecta `blog::urls::api(pool)` en tu `urls.rs` de nivel superior.

### Step 7 — Run it and exercise every endpoint

```bash
cargo run            # listening on http://0.0.0.0:8080
```

**Crear** (`POST`). El serializador valida primero, luego escribe solo los campos
que acepta:

```bash
# happy path — note `content` (the renamed `body`) on the way in
curl -X POST localhost:8080/api/posts \
  -H 'content-type: application/json' \
  -d '{"title":"Hello Rustango","content":"First post body.","status":"published","author_id":1}'
```
```json
{
  "id": 1,
  "title": "Hello Rustango",
  "content": "First post body.",
  "status": "published",
  "author_id": 1,
  "summary": "First post body.",
  "published_at": "2026-01-02T12:00:00Z"
}
```
La respuesta tiene la forma del **serializador**: `body` regresó como `content`,
apareció el `summary` calculado, y `published_at` (de solo lectura, establecido
por el servidor) está presente.

**La validación rechaza la entrada inválida** con un `400` con forma de DRF —
arreglos de mensajes indexados por campo:

```bash
curl -i -X POST localhost:8080/api/posts \
  -H 'content-type: application/json' \
  -d '{"title":"hi","content":"x","author_id":1}'
# HTTP/1.1 400 Bad Request
# {"title":["title must be at least 3 characters"]}
```

**Los campos de solo lectura / calculados que un cliente envía se ignoran** — no
pueden inyectar `published_at` ni `summary`:

```bash
curl -X POST localhost:8080/api/posts \
  -H 'content-type: application/json' \
  -d '{"title":"Sneaky","content":"x","author_id":1,"published_at":"1999-01-01T00:00:00Z","summary":"hax"}'
# → published_at is the server value, not 1999; summary is recomputed from body.
```

**Listar** (`GET`) — paginado, cada fila con la forma del serializador:

```bash
curl localhost:8080/api/posts
```
```json
{ "count": 1, "page": 1, "page_size": 20, "last_page": 1, "results": [ { "id": 1, "title": "Hello Rustango", … } ] }
```

**Recuperar / actualizar / actualización parcial / eliminar:**

```bash
curl localhost:8080/api/posts/1                       # retrieve  → 200
curl -X PUT   localhost:8080/api/posts/1 -H 'content-type: application/json' \
     -d '{"title":"Edited","content":"new body","status":"published","author_id":1}'   # full update → 200
curl -X PATCH localhost:8080/api/posts/1 -H 'content-type: application/json' \
     -d '{"title":"Just the title"}'                   # partial update → 200 (other fields untouched)
curl -X DELETE localhost:8080/api/posts/1              # destroy → 204
```

La validación de `PATCH` corre sobre lo que envías; los campos de solo lectura se
mantienen en su valor de servidor incluso si se envían.

### Step 8 — Filter, search, order, paginate

Todo en el endpoint de listado, sin código adicional (declaraste los campos en el
Paso 5):

```bash
curl 'localhost:8080/api/posts?status=published&author_id=1'      # filter
curl 'localhost:8080/api/posts?status__in=published,archived'     # lookup
curl 'localhost:8080/api/posts?search=rustango'                   # search title+body
curl 'localhost:8080/api/posts?ordering=title'                    # sort (asc)
curl 'localhost:8080/api/posts?page=2&page_size=10'               # paginate
```

### Step 9 — Test it

El framework incluye un cliente de pruebas en proceso — haz aserciones sobre
respuestas HTTP reales sin arrancar un servidor:

```rust
// tests/post_api.rs
use rustango::test_client::TestClient;
use myblog::blog::post_view_set::PostViewSet;
use rustango::sql::sqlx::PgPool;
use serde_json::json;

async fn app() -> axum::Router {
    let pool = PgPool::connect(&std::env::var("DATABASE_URL").unwrap()).await.unwrap();
    PostViewSet::router("/api/posts", pool)
}

#[tokio::test]
async fn rejects_short_title() {
    let client = TestClient::new(app().await);
    let res = client.post("/api/posts")
        .json(&json!({"title":"hi","content":"x","author_id":1}))
        .send().await;
    assert_eq!(res.status, 400);
    assert!(res.json_value()["title"].is_array());   // DRF field-error shape
}

#[tokio::test]
async fn create_then_list() {
    let client = TestClient::new(app().await);
    let created = client.post("/api/posts")
        .json(&json!({"title":"Hello","content":"b","status":"published","author_id":1}))
        .send().await;
    assert_eq!(created.status, 201);
    let list = client.get("/api/posts").send().await;
    assert!(list.json_value()["results"].is_array());
}
```

```bash
cargo test --test post_api
```

Ese es un recurso REST completo y validado. El resto de esta página es la
referencia detrás de cada paso.

---

## The serializer marriage: input + output

Conectar un serializador (vía `serializer = …` en el derive, o `.serializer::<S>()`
en el builder) cambia **ambas** direcciones. Funciona igual en PostgreSQL, MySQL
y SQLite.

### Output — responses render through the serializer

Las respuestas de `list`, `retrieve`, `create` y `update` se producen mediante
`S::from_model(&row)`, de modo que las sobrescrituras del serializador dan forma
al JSON:

| Campo del serializador | Efecto en la respuesta |
|---|---|
| `#[serializer(source = "body")]` | la columna `body` se emite bajo el nombre del campo (p. ej. `content`) |
| `#[serializer(method = "fn")]` | aparece un campo calculado (a partir de `Self::fn(&model)`) |
| `#[serializer(read_only)]` | incluido en la salida |
| `#[serializer(write_only)]` | **omitido** de la salida |

> **Advertencia sobre `nested` / `many`.** Los campos de serializador anidados y
> de colección se renderizan solo cuando las filas relacionadas fueron cargadas
> (vía `select_related` / una carga anticipada); de lo contrario recurren a su
> valor por defecto. La consulta de listado del ViewSet automático carga la fila
> base — conecta las relaciones explícitamente si un campo anidado debe
> poblarse.

### Input — requests are validated and filtered

En `create` y `update`, cuando hay un serializador registrado:

1. **Se ejecuta la validación.** El `validate()` del serializador — cada
   `#[serializer(validate = "fn")]` por campo más el `validate` cruzado a nivel
   de contenedor — corre contra el cuerpo JSON. Ante un fallo, la petición se
   rechaza con `400 Bad Request` con la forma de error de DRF: un objeto JSON
   indexado por nombre de campo con arreglos de mensajes, p. ej.
   `{"title":["title must be at least 3 characters"]}`.
2. **Filtrado de campos escribibles.** Solo se persisten los campos escribibles
   del serializador; los campos `read_only` y `method`/calculados que un cliente
   envía se **ignoran** (no se escriben), y los renombrados de `source` se
   resuelven a la columna del modelo. Así, un cliente no puede establecer un
   campo controlado por el servidor incluyéndolo en el cuerpo.

> **Los cuerpos form-urlencoded** (frente a JSON) omiten `validate()` — no hay un
> valor tipado que validar — pero aún reciben el filtrado de campos escribibles.

Por debajo, esto son los métodos `validate()`, `writable_source_fields()` y
`from_writable_json()` del trait `ModelSerializer`, todos generados por
`#[derive(Serializer)]`. Consulta la [guía de serializadores](serializers.md) para
saber cómo escribir los validadores.

---

## The two ways to define a ViewSet

Ambas producen un `axum::Router` con las mismas rutas CRUD.

**1. La macro derive** — declarativa, de un solo inquilino; conecta un
serializador con `serializer = …`:

```rust
#[derive(ViewSet)]
#[viewset(
    model         = Post,
    serializer    = crate::blog::post_serializer::PostSerializer,
    filter_fields = "author_id, status",
    search_fields = "title, body",
    ordering      = "-published_at",
    page_size     = 20,
)]
pub struct PostViewSet;

let router = PostViewSet::router("/api/posts", pool);
```

**2. El builder** — `ViewSet::for_model(...)`, programático, tri-dialecto
(PostgreSQL / SQLite / MySQL) y consciente de la tenencia; conecta un serializador
con `.serializer::<S>()`:

```rust
use rustango::viewset::ViewSet;
use rustango::core::Model as _;

let router = ViewSet::for_model(Post::SCHEMA)
    .serializer::<PostSerializer>()
    .filter_fields(&["author_id", "status"])
    .search_fields(&["title", "body"])
    .ordering(&[("published_at", true)])    // true = DESC
    .page_size(20)
    .router_pool("/api/posts", pool);       // tri-dialect Pool
```

Recurre al builder cuando necesites SQLite/MySQL, multi-tenencia, una
configuración construida en tiempo de ejecución, o los extras (throttling,
backends de filtro personalizados, paginación por cursor).

---

## The CRUD endpoints

Montar en `/api/posts` conecta las seis operaciones REST:

| Verbo | Ruta | Acción | Éxito | Cuerpo |
|---|---|---|---|---|
| `GET` | `/api/posts` | **list** | 200 | sobre paginado (ver [Paginación](#pagination)) |
| `POST` | `/api/posts` | **create** | 201 | el objeto creado — *o un arreglo, para creación masiva* |
| `GET` | `/api/posts/{pk}` | **retrieve** | 200 | el objeto |
| `PUT` | `/api/posts/{pk}` | **update** (completa) | 200 | el objeto actualizado |
| `PATCH` | `/api/posts/{pk}` | **partial update** | 200 | el objeto actualizado (solo cambian los campos suministrados) |
| `DELETE` | `/api/posts/{pk}` | **destroy** | 204 | vacío |

Una barra final en el prefijo de montaje es opcional. Solo se conectan estos seis
verbos — sin `HEAD`/`OPTIONS` automáticos. La **creación masiva** viene gratis:
haz `POST` de un *arreglo* JSON y cada elemento se inserta en orden, validado
atómicamente (un elemento inválido rechaza todo el lote).

---

## Choosing which operations to expose

Para un recurso de **solo lectura** (solo list + retrieve), añade `read_only`:

```rust
#[viewset(model = Post, read_only)]            // macro
ViewSet::for_model(Post::SCHEMA).read_only()   // builder
```

No hay un interruptor por verbo más allá de read-only. Para "todo excepto
eliminar", monta el ViewSet y sobrescribe la única ruta con tu propio handler
(ver [Acciones personalizadas](#custom-actions-beyond-crud)).

---

## `#[viewset(...)]` attribute reference

| Clave | Ejemplo | Por defecto | Qué hace |
|---|---|---|---|
| `model` | `model = Post` | **requerido** | El modelo sobre el que se construye el recurso. |
| `serializer` | `serializer = path::To::S` | ninguno | Conecta un serializador para **salida + entrada** tipadas (ver [arriba](#the-serializer-marriage-input--output)). |
| `fields` | `"id, title, body"` | todos los campos escalares | Lista blanca para la proyección por defecto (sin serializador) + campos escribibles. |
| `filter_fields` | `"author_id, status"` | ninguno | Campos filtrables vía `?field=value` (+ lookups). |
| `search_fields` | `"title, body"` | ninguno | Campos con los que coincide la caja `?search=` (OR sin distinción de mayúsculas). |
| `ordering` | `"-published_at, id"` | ninguno | Orden por defecto (`-` = DESC). |
| `page_size` | `20` | 20 | Filas por página (el `?page_size=` del cliente se limita a 1000). |
| `read_only` | *(flag)* | apagado | Expone solo GET (list + retrieve). |
| `permissions(...)` | `permissions(create = "post.add")` | ninguno | Codenames de permiso por acción. |

---

## Builder reference

Cada método de `ViewSet::for_model(SCHEMA)` (cada uno devuelve `Self`):

| Método | Propósito |
|---|---|
| `serializer::<S>()` | Conecta un serializador para salida + entrada tipadas (tri-dialecto). |
| `fields(&["…"])` | Lista blanca de proyección por defecto + campos escribibles (cuando no hay serializador). |
| `filter_fields(&["…"])` | Habilita el filtrado `?field=value`. |
| `search_fields(&["…"])` | Habilita `?search=`. |
| `ordering(&[("field", desc)])` | Orden de clasificación por defecto. |
| `ordering_fields(&["…"])` | Lista blanca de qué campos puede usar `?ordering=`. |
| `page_size(n)` | Tamaño de página por defecto (≤ 1000). |
| `read_only()` | Solo GET. |
| `permissions(ViewSetPerms{…})` / `permissions_for_model::<T>()` | Compuertas de codename por acción (la última sobre tenencia). |
| `cursor_pagination("id")` / `cursor_pagination_desc("id")` | Paginación por keyset (omite `COUNT(*)`). |
| `limit_offset_pagination()` | Ventaneo `?limit=&offset=`. |
| `pagination(PaginationStyle::…)` | Establece el estilo explícitamente. |
| `filter_backend(closure)` | Añade predicados `WHERE` personalizados más allá de `filter_fields`. |
| `throttle(…)` / `throttle_all(max, secs)` | Límites de tasa de ventana fija por acción. |
| `router(prefix, pgpool)` | Monta (Postgres, pool estático). |
| `router_pool(prefix, pool)` | Monta tri-dialecto (PG / SQLite / MySQL). |
| `tenant_router(prefix)` | *(tenencia)* monta con resolución de inquilino por petición. |

---

## Filtering, search and ordering

Todo gobernado por parámetros de consulta en el endpoint de **listado**.

**Filtrado** — cada entrada de `filter_fields` acepta `?field=value` (exacto) más
lookups estilo Django vía un sufijo `__`:

```
?status=published
?author_id__in=1,2,3
?published_at__gte=2026-01-01
?title__icontains=rust
?body__isnull=false
```

Lookups soportados: `ne`, `gt`, `gte`, `lt`, `lte`, `in`, `not_in`, `contains`,
`icontains`, `startswith`, `istartswith`, `endswith`, `iendswith`, `isnull`
(sin sufijo = exacto). Los campos que no están en `filter_fields` se ignoran.

**Búsqueda** — `?search=term` coincide con `search_fields` mediante un OR sin
distinción de mayúsculas.

**Ordenación** — `?ordering=field,-other` (`-` = DESC). Cualquier campo es
ordenable a menos que establezcas `.ordering_fields([...])` para restringirlo. Sin
un parámetro, se aplica el `ordering` por defecto. Todos se componen.

---

## Pagination

> **Trampa — pagina sobre un orden determinista.** La paginación por número de
> página y por limit/offset asume una clasificación estable; ordenar por una
> columna no única (o por ninguna) permite que las filas se desplacen entre
> páginas — duplicadas u omitidas. Añade siempre un desempate único, p. ej.
> `ordering = "-published_at, id"`. (Ambas también ejecutan `COUNT(*)` por
> llamada; la paginación por cursor lo omite para tablas grandes.)

Tres estilos; el de número de página es el predeterminado. El sobre de listado
difiere según el estilo:

**Número de página** (por defecto) — `?page=2&page_size=20`:

```json
{ "count": 137, "page": 2, "page_size": 20, "last_page": 7, "results": [ … ] }
```

**Cursor** — `.cursor_pagination("id")` (o `_desc`); omite `COUNT(*)`, ideal para
tablas muy grandes. `?cursor=<token>&page_size=20`:

```json
{ "page_size": 20, "next": "<opaque-cursor-or-null>", "results": [ … ] }
```

**Limit/offset** — `.limit_offset_pagination()`. `?limit=20&offset=40`:

```json
{ "count": 137, "limit": 20, "offset": 40, "results": [ … ] }
```

`page_size` / `limit` se limitan a 1000.

---

## Validation

Con un **serializador conectado**, la ruta de create/update ejecuta los
validadores del serializador y devuelve `400`s con forma de DRF — la forma
recomendada de validar (ver [el matrimonio](#the-serializer-marriage-input--output)
y la [guía de serializadores](serializers.md#validation)). Se ejecutan tres capas:

- **Restricciones declarativas** — `max_length` / `min_length` / `min` / `max`, y
  por defecto el campo **hereda del modelo** `max_length` / `min` / `max` /
  `choices`. Así, una columna `#[rustango(max_length = 200)]` se verifica en
  longitud en la API sin configuración extra (comportamiento del `ModelSerializer`
  de DRF), convirtiendo los `500`s que serían por restricción de BD en `400`s
  amigables como `{"title":["Ensure this value has at most 200 characters."]}`.
- **Por campo** `validate = "fn"` y un hook `validate` **cruzado** — tus reglas
  personalizadas (formatos, entre campos, lógica de negocio).

Independientemente de un serializador, la ruta de escritura siempre aplica el
**esquema**:

- **Los tipos se coaccionan y verifican** — un valor `i64` / `DateTime` / `Uuid` /
  `bool` inválido es un `400` que nombra el campo.
- **Requerido / NOT NULL** — un campo no anulable faltante (o cadena vacía para un
  `String` no anulable) es un `400`; los campos anulables aceptan vacío → `NULL`.
- **Restricciones de base de datos** — únicos, claves foráneas y restricciones
  check afloran como un `400` en INSERT/UPDATE.

Así que incluso sin un serializador obtienes validación de tipo + requerido +
restricción de BD; conecta un serializador para obtener verificaciones
declarativas de longitud/rango/choice (heredadas automáticamente) más tus propias
reglas por campo y entre campos.

---

## Permissions and throttling

> **Un ViewSet es público por defecto.** Montar uno expone los seis verbos CRUD a
> cualquiera — no hay autenticación integrada. Protégelo con `permissions(...)`
> (abajo), ponlo detrás del [middleware de autenticación](auth-backends.md)
> (`require_auth`), o ambos, antes de exponer escrituras.

**Los permisos** protegen cada acción con codenames (OR dentro de una acción):

```rust
use rustango::viewset::{ViewSet, ViewSetPerms};

ViewSet::for_model(Post::SCHEMA)
    .permissions(ViewSetPerms {
        list:     vec!["post.view".into()],
        retrieve: vec!["post.view".into()],
        create:   vec!["post.add".into()],
        update:   vec!["post.change".into()],
        destroy:  vec!["post.delete".into()],
    })
    .router_pool("/api/posts", pool);
```

Una lista de acción vacía = sin verificación. La aplicación lee un usuario
autenticado de la petición (la integración de autenticación de `tenancy`); los
superusuarios la eluden, un usuario ausente se deniega. `.permissions_for_model::<Post>()`
autocompleta los codenames estándar `post.view`/`add`/`change`/`delete`.

**El throttling** aplica límites de ventana fija por cliente, por acción:

```rust
ViewSet::for_model(Post::SCHEMA)
    .throttle_all(60, 60)              // 60 requests / 60s per client, every action
    .router_pool("/api/posts", pool);
```

Sobre el límite → `429 Too Many Requests` + `Retry-After`. Los contadores son por
proceso; la clave del cliente es la IP de conexión (o `X-Forwarded-For` /
`X-Real-IP`).

---

## Custom actions beyond CRUD

No hay un decorador `@action` de DRF — el ViewSet es estrictamente las seis rutas
CRUD. Para endpoints adicionales, monta tus propios handlers junto al ViewSet:

```rust
use axum::{Router, routing::{get, post}};

let api = Router::new()
    .merge(ViewSet::for_model(Post::SCHEMA).router_pool("/api/posts", pool.clone()))
    .route("/api/posts/stats", get(post_stats))
    .route("/api/posts/bulk_archive", post(bulk_archive));
```

Para lógica `WHERE` adicional, `.filter_backend(…)` aporta predicados sin una ruta
separada.

### Scoping rows to the authenticated principal

Un backend se ejecuta en **cada** acción — `list`, `retrieve`, `update`,
`destroy` — de modo que se comporta como el `get_queryset()` de DRF. Una fila que
el backend excluye es un **404** en las rutas de ítem, no un 403: un 403
confirmaría que el id existe.

La identidad debe provenir de la credencial, nunca de la cadena de consulta. Un
filtro `?owner_id=` no es un ámbito — es un parámetro que el llamador elige.

#### `OwnedBy` — the shipped backend

La mayoría de los recursos con dueño necesitan exactamente una regla: *filas cuya
columna de propiedad es el llamador*. Nombra la columna y móntala.

```rust
use rustango::viewset::{OwnedBy, ViewSet};

ViewSet::for_model(Note::SCHEMA)
    .filter_backend(OwnedBy::column("member_id"))
    .tenant_router("/api/notes")
    .layer(axum::middleware::from_fn(
        rustango::tenancy::auth_routes::require_bearer,
    ))
```

Cualquier columna funciona — `owner_id`, `member_id`, `author_id` — porque el
backend toma el nombre en lugar de asumir una convención. Falla cerrado en las dos
formas en que puede estar mal: una petición no autenticada y una columna que el
modelo no tiene coinciden ambas con **nada**, de modo que un error tipográfico al
montar no puede convertirse en "sin predicados, devuelve la tabla".

Los superusuarios no son especiales por defecto; `.superuser_sees_all()` lo
habilita, porque "los admins ven todo" es una decisión de producto, no del
framework.

#### Where the identity comes from

[`Principal`] es el único tipo de identidad, resuelto a partir de lo que sea que
verificó la petición — un `Principal` explícito, un `AuthenticatedUser` dejado por
un middleware de sesión o Bearer, o un token de agente MCP (que actúa como el
usuario que lo acuñó). No autentica nada por sí mismo; solo lee lo que un
middleware verificador ya demostró, de modo que nada puede insertar uno sin
verificar primero una credencial.

`require_bearer` es ese middleware para una API JSON: verifica el token de acceso
contra el inquilino resuelto, vuelve a leer la fila del usuario (una cuenta
desactivada deja de funcionar en la siguiente petición, no cuando el token
expira), e inserta tanto `AuthenticatedUser` como `Principal`. Úsalo como
extractor en cualquier lugar:

```rust
use rustango::tenancy::{OptionalPrincipal, Principal};

async fn mine(principal: Principal) -> String {          // 401 when absent
    format!("user {}", principal.user_id)
}

async fn home(OptionalPrincipal(who): OptionalPrincipal) -> String {
    who.map_or("anonymous".into(), |p| format!("user {}", p.user_id))
}
```

#### Writing your own backend

Cuando la propiedad no es una sola columna — un equipo compartido, una fila con
borrado lógico, una ventana de fechas — implementa el trait y sobrescribe
`filter_with`, que recibe los `Parts` de la petición:

```rust
use axum::http::request::Parts;
use rustango::tenancy::Principal;
use rustango::viewset::ViewSetFilter;

struct OwnerFilter;

impl ViewSetFilter for OwnerFilter {
    // No principal in hand — fail closed. Returning no predicates here would
    // widen the query to every row in the table.
    fn filter(&self, _p: &HashMap<String, String>, schema: &'static ModelSchema) -> Vec<WhereExpr> {
        deny_all(schema)
    }

    fn filter_with(
        &self,
        parts: &Parts,
        _p: &HashMap<String, String>,
        schema: &'static ModelSchema,
    ) -> Vec<WhereExpr> {
        let Some(principal) = Principal::from_parts(parts) else {
            return deny_all(schema);
        };
        vec![WhereExpr::Predicate(Filter {
            column: schema.field("owner_id").expect("owner_id").column,
            op: Op::Eq,
            value: SqlValue::from(principal.user_id),
        })]
    }
}

ViewSet::for_model(Note::SCHEMA)
    .filter_backend(OwnerFilter)
    .tenant_router("/api/notes")
```

`filter_with` recae por defecto en `filter`, de modo que un backend que no
necesita la petición — incluida la forma de closure simple — implementa solo
`filter` como antes.

---

## Mounting

Compón el router del ViewSet en tu app. Un solo inquilino, pool estático:

```rust
let api = urls::api()
    .merge(PostViewSet::router("/api/posts", pool.clone()))                          // macro
    .merge(ViewSet::for_model(Author::SCHEMA).router_pool("/api/authors", pool.clone())); // builder
```

Multi-inquilino (sin pool capturado — cada petición resuelve su conexión de
inquilino):

```rust
let api = urls::api()
    .merge(ViewSet::for_model(Post::SCHEMA).tenant_router("/api/posts"));
```

`make:api_routes <app>` genera una `api()` por app que reúne estas líneas
`.merge(...)`; conéctala en tu `urls.rs` de nivel superior.

---

## Backend support

- **El builder + `router_pool` / `tenant_router`** es **tri-dialecto** —
  PostgreSQL, SQLite y MySQL — y es la ruta recomendada.
- **El `router(prefix, PgPool)` de la macro derive** captura un `PgPool`
  (PostgreSQL).
- **La entrada + salida del serializador** ahora funciona en **los tres backends**
  (el renderizado por fila es tri-dialecto; la antigua compuerta solo-PG
  desapareció).
- El filtrado, la búsqueda, la ordenación, los tres modos de paginación, los
  permisos, el throttling y la creación masiva funcionan todos en los backends
  soportados en la ruta del builder.

---

## Try it

El flujo de principio a fin de arriba refleja el ejemplo compilable
`getting_started_blog` (Pasos 12–13 de la [guía de primeros pasos](getting-started.md)).
Las propias pruebas en vivo del framework bajo `crates/rustango/tests/viewset_*.rs`
son la referencia ejecutable más completa — incluidas las pruebas de
entrada/salida del serializador. Corren sobre SQLite en memoria pero necesitan los
feature flags correspondientes, p. ej.:

```bash
cd crates/rustango
cargo test --features sqlite,tenancy --test viewset_serializer_render_sqlite_live
cargo test --features sqlite,tenancy --test viewset_serializer_input_sqlite_live
cargo test --features sqlite,tenancy --test viewset_sqlite_live
```

---

## See also

- [Serializadores](serializers.md) — da forma al JSON que un ViewSet envía y valida.
- [Vistas HTML](html-views.md) — la contraparte renderizada en el servidor de esta API JSON.
- [OpenAPI](openapi.md) — genera una especificación + Swagger UI a partir de tus ViewSets.
- [URLs y enrutamiento](urls.md) — compón routers de ViewSet en tu app.
