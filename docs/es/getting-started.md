# Primeros pasos: construir un blog con Rustango

Este recorrido te lleva desde un directorio vacío hasta un blog desplegado: publicaciones, una interfaz de administración, una API JSON, autenticación JWT y pruebas. De principio a fin. Si has usado Django, Laravel o Rails, la mayoría de los pasos te resultarán familiares; señalamos los paralelismos sobre la marcha.

> **Tiempo:** ~45 minutos para el recorrido completo, ~10 minutos si solo quieres verlo funcionar.
>
> **Versión ejecutable:** cada paso a continuación está replicado en un ejemplo probado y compilable en [`crates/rustango/examples/getting_started_blog`](../crates/rustango/examples/getting_started_blog). Si algún paso parece estar mal, compáralo con ese ejemplo.

[![Construir un blog con Rustango: generar la migración, aplicarla, arrancar el servidor y consultar la API JSON — todo desde un único binario](img/getting-started.png)](img/getting-started.png)

---

## Qué necesitas primero

| Herramienta | Para qué | Instalación |
|---|---|---|
| Rust 1.88+ | Compilador | <https://rustup.rs> |
| Docker | Postgres local | <https://docker.com> |
| `psql` (opcional) | Inspeccionar la BD | `brew install libpq` / `apt install postgresql-client` |

Comprueba las versiones que tienes instaladas:

```bash
rustc --version    # should print 1.88+
docker --version   # any recent version
```

---

## Paso 1: Instalar el generador de andamiaje

El generador de andamiaje crea esqueletos de proyectos y apps por ti, como `django-admin` o `rails new`.

```bash
cargo install cargo-rustango
```

Esto añade el subcomando `cargo rustango ...` de forma global. Confirma que está ahí:

```bash
cargo rustango --help
```

---

## Paso 2: Crear el proyecto

Esto genera el andamiaje de un proyecto nuevo, el equivalente en **Rustango** de `rails new` o `composer create-project`.

```bash
cd ~/projects                                 # wherever you keep code
cargo rustango new myblog                     # default = fullstack template
cd myblog
```

Esto es lo que se generó:

```
myblog/
├── Cargo.toml                  # rustango + axum + sqlx + tokio
├── .env.example                # template for DATABASE_URL etc.
├── .gitignore
├── docker-compose.yml          # Postgres in a container
├── README.md                   # project-specific
├── config/                     # tiered settings (default + dev/staging/prod)
├── migrations/                 # empty — `cargo run -- makemigrations` populates
└── src/
    ├── main.rs                 # entry point: `Cli::new().api(urls::api()).run()`
    ├── models.rs               # every #[derive(Model)] lives here
    ├── views.rs                # axum request handlers
    └── urls.rs                 # `pub fn api()` route aggregator + `admin_router(pool)`
```

Hay un único binario: `cargo run` arranca el servidor HTTP, y cada verbo al estilo de Django (`migrate`, `makemigrations`, `startapp`, `check`, …) pasa por el mismo binario mediante `cargo run -- <verb>`. No hay un binario `manage` aparte.

`Cargo.toml` es el manifiesto de dependencias (como `composer.json` o un `Gemfile`). Ábrelo y confirma que `rustango` aparece bajo `[dependencies]`.

> **Confirma el bloque `[features]` — elige un backend de base de datos.** `#[derive(Model)]`
> aplica cfg-gates a sus impls `FromRow` / `LoadRelated` generadas según las features
> de **tu** crate (un `cfg` dentro de una macro derive se resuelve contra la crate de
> destino, no contra **Rustango**), así que aquí debe estar habilitada una feature de
> backend o el primer modelo no compilará. Un andamiaje actual incluye:
>
> ```toml
> [features]
> default  = ["postgres"]            # the backend `cargo run` uses
> postgres = ["rustango/postgres"]
> sqlite   = ["rustango/sqlite"]
> mysql    = ["rustango/mysql"]
> ```
>
> Si tu `Cargo.toml` generado **no** tiene un bloque `[features]` (un `cargo-rustango`
> más antiguo), añade el de arriba a mano — eso siempre lo arregla. Sin él, la
> compilación falla con *"the trait bound `…: MaybePgFromRow` is not satisfied"*
> más un revelador `warning: unexpected cfg condition value: postgres`.

---

## Paso 3: Configurar tu entorno

La configuración vive en un archivo `.env`, igual que en Django o Laravel. Copia la plantilla:

```bash
cp .env.example .env
```

El `.env` generado es compatible con Docker de fábrica. Como vamos a ejecutar `cargo` en el host (no dentro del contenedor de desarrollo), cambia el host de la base de datos de `postgres` a `localhost`:

```bash
DATABASE_URL=postgres://rustango:rustango@localhost:5432/myblog_dev
RUSTANGO_BIND=0.0.0.0:8080
RUSTANGO_APEX_DOMAIN=localhost
RUSTANGO_SESSION_SECRET=change-me-base64-encoded-32-bytes-or-more
```

Las credenciales, el puerto y el nombre de la base de datos (`myblog_dev`) ya coinciden con el servicio Postgres del `docker-compose.yml`, así que no necesitas tocarlos.

`RUSTANGO_SESSION_SECRET` firma sesiones y tokens, así que no despliegues el marcador de posición. Genera uno real y pégalo:

```bash
openssl rand -base64 32     # paste output as RUSTANGO_SESSION_SECRET value
```

---

## Paso 4: Arrancar Postgres

El proyecto incluye un `docker-compose.yml` que ejecuta Postgres en un contenedor, para que no instales una base de datos a mano. La app en sí la ejecutaremos con `cargo` en el host, así que arranca solo el servicio `postgres` en segundo plano (el archivo de compose también define un contenedor de desarrollo `rust` opcional que, de lo contrario, ocuparía el puerto 8080):

```bash
docker compose up -d postgres
```

Confirma que está en marcha:

```bash
docker compose ps
psql "$DATABASE_URL" -c "SELECT version();"   # should print Postgres version
```

---

## Paso 5: Ejecutar las migraciones integradas

Las migraciones crean las tablas de tu base de datos, la misma idea que `php artisan migrate` o `rails db:migrate`. Ejecútalas una vez para preparar las propias tablas del framework:

```bash
cargo run -- migrate
```

La primera compilación tarda ~2 minutos (Rust compila todo desde el código fuente). Un proyecto nuevo aún no incluye archivos de migración, así que verás `nothing to migrate (already up to date)` — `migrate` igualmente prepara la tabla de registro de auditoría del framework para que los modelos auditados funcionen en cuanto los añadas. Generarás tu primera migración real en el Paso 9.

Comprueba el estado de las migraciones:

```bash
cargo run -- showmigrations
```

En un proyecto nuevo esto imprime `(no migrations in ./migrations)`. Una vez que crees un modelo y ejecutes `makemigrations` (Paso 9), cada migración aplicada muestra aquí una `[X]`.

---

## Paso 6: Primer arranque

Arranca el servidor para asegurarte de que todo está conectado.

```bash
cargo run
```

Verás:

```
listening on http://0.0.0.0:8080
```

Abre <http://localhost:8080> en tu navegador. El andamiaje incluye un handler raíz sencillo (`views::index`) que te saluda con **Hello from Rustango!** y un enlace al admin — eso confirma que **Rustango** está funcionando. (Los proyectos que no definen su propia ruta `/` obtienen en su lugar una página de bienvenida integrada, mediante `Cli::with_welcome()`.)

Pulsa Ctrl-C para detenerlo.

---

## Paso 7: Crear una app

Una "app" es un módulo de funcionalidad autocontenido, exactamente como una app de Django. Tu app de blog contendrá el modelo Post, sus rutas y sus plantillas.

```bash
cargo run -- startapp blog
```

Esto escribe:

```
src/blog/
├── mod.rs
├── models.rs              # a starter model named after the app (you'll replace it)
├── views.rs               # axum handlers
├── urls.rs                # blog-specific routes (pub fn api())
└── tests.rs               # in-process router + inventory smoke tests
```

`startapp` conecta el nuevo módulo por ti (similar a añadirlo a `INSTALLED_APPS` de Django): declara `mod blog;` en `src/main.rs` e inserta una línea `.merge(crate::blog::urls::api())` en el agregador `api()` de `src/urls.rs`, de modo que las rutas del blog se componen en la app automáticamente. No hace falta registrar el módulo manualmente.

---

## Paso 8: Definir un modelo

Un modelo es una tabla de base de datos descrita como un struct de Rust, como un modelo de Django o una clase de Eloquent/Active Record. Abre `src/blog/models.rs` y define tu `Post`. (Para la referencia completa — cada tipo de campo, claves primarias personalizadas y todos los atributos — consulta la [guía de Modelos](models.md).)

```rust
use rustango::{Auto, Model};
use chrono::{DateTime, Utc};

#[derive(Model, Clone, Debug)]
#[rustango(
    table = "posts",
    display = "title",
    admin(
        list_display  = "id, title, status, published_at",
        search_fields = "title, body",
        list_filter   = "status, author_id",
        ordering      = "-published_at",
    ),
    audit(track = "title, body, status"),
    index("status, published_at"),
)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(max_length = 200)]
    pub title: String,

    pub body: String,

    #[rustango(max_length = 20, default = "'draft'")]
    pub status: String,                  // draft | published

    pub author_id: i64,

    #[rustango(auto_now_add)]
    pub published_at: Auto<DateTime<Utc>>,

    #[rustango(soft_delete)]
    pub deleted_at: Option<DateTime<Utc>>,
}
```

Algunas cosas de Rust a tener en cuenta:

- `#[derive(Model, ...)]` es una **macro derive**: genera código automáticamente para el struct, del mismo modo que lo haría un decorador de clase o una clase base en otros frameworks. Derivar `Model` es lo que le da al struct sus métodos de consulta.
- `Auto<i64>` marca un campo que la base de datos rellena por ti (un entero `i64` autoincremental), como una clave primaria automática.
- `Option<...>` significa "este valor puede estar ausente". `Option<DateTime<Utc>>` es una marca de tiempo que puede ser nula, así que `deleted_at` está vacío hasta que la fila se elimina de forma lógica (soft-delete).
- Los atributos `#[rustango(...)]` configuran cada campo (longitud máxima, valores por defecto, índices) y el bloque `admin(...)` prepara las columnas y filtros de la interfaz de administración.

---

## Paso 9: Crear y aplicar la migración

Ahora convierte ese modelo en una tabla real. Primero, genera la migración a partir de tu modelo (como `makemigrations` en Django):

```bash
cargo run -- makemigrations
```

Verás algo como:

```
wrote ./migrations/0001_create_item_and_posts_and_rustango_admin_users_etc.json
    + CreateTable("item")
    + CreateTable("posts")
    + CreateTable("rustango_admin_users")
    + CreateTable("rustango_content_types")
    + CreateIndex { table: "posts", columns: ["status", "published_at"], ... }
```

Esta primera migración crea tus modelos — `posts`, además del modelo inicial `item` que el andamiaje incluyó en `src/models.rs` — junto con las propias tablas de admin y de tipos de contenido del framework. Abre el JSON si quieres: contiene las operaciones más una instantánea completa del esquema.

Aplícala a la base de datos:

```bash
cargo run -- migrate
```

Confirma que la tabla existe:

```bash
psql "$DATABASE_URL" -c "\d posts"
```

---

## Paso 10: Probar el ORM

Vamos a leer y escribir filas desde el código. El ORM te permite trabajar con las filas de la base de datos como structs de Rust en lugar de SQL en crudo, como el ORM de Django, Eloquent o Active Record.

Edita temporalmente `src/main.rs` para ejecutar una prueba rápida de crear-y-leer antes de arrancar el servidor. Reemplaza el cuerpo del `Cli` con una prueba de humo del ORM improvisada (conserva el `#[rustango::main]` del generador de andamiaje y las declaraciones `mod` al principio del archivo):

```rust
mod blog;
mod models;
mod urls;
mod views;

use crate::blog::models::Post;
use rustango::{Auto, Model};

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let pool = rustango::sql::sqlx::PgPool::connect(&std::env::var("DATABASE_URL")?).await?;

    // CREATE
    let mut p = Post {
        id: Auto::default(),
        title: "First post".into(),
        body: "Hello, world.".into(),
        status: "draft".into(),
        author_id: 1,
        published_at: Auto::default(),
        deleted_at: None,
    };
    p.save(&pool).await?;
    println!("created post id = {}", p.id.get().copied().unwrap());

    // READ
    let posts = Post::objects().fetch_on(&pool).await?;
    for post in &posts {
        println!("- {}", post.title);
    }

    Ok(())
}
```

Qué está pasando aquí, en términos sencillos:

- `pool` es el pool de conexiones a la base de datos compartido. Le pasas una referencia (`&pool`) a las llamadas de consulta en lugar de abrir una conexión nueva cada vez.
- Las llamadas a la base de datos son asíncronas, así que cada una termina en `.await` — eso pausa hasta que llega el resultado y luego continúa. El `?` tras un `.await` dice "si esto dio error, detente y devuelve el error".
- `main` devuelve un `Result`, el tipo éxito-o-error de Rust, que es por lo que funcionan `?` y el `Ok(())` de cierre.
- Para guardar una fila, llama a `.save(&pool)` sobre ella. Para leer filas, construye una consulta con `Post::objects()` y ejecútala con `.fetch_on(&pool)` — el equivalente aproximado de `Post.objects.all()` de Django. (`.save(&pool)` / `.fetch_on(&pool)` reciben un `sqlx::PgPool`; la variante escueta `.fetch(&pool)` recibe en su lugar un `rustango::sql::Pool` multi-backend — consulta la [guía del ORM](orm.md).)

Ejecútalo:

```bash
cargo run
```

Deberías ver el id de tu nueva publicación y las filas leídas de vuelta. Restaura `src/main.rs` a su forma de servidor andamiada una vez que hayas confirmado que funciona — el siguiente paso parte de ahí.

---

## Paso 11: Activar el auto-admin

**Rustango** incluye una interfaz de administración generada para tus modelos, igual que el admin de Django. El generador de andamiaje ya te dio un helper `admin_router(pool)` en `src/urls.rs` que construye el auto-admin a partir de un pool — solo tienes que anidarlo bajo `/admin` y pasarlo al `Cli`.

Primero, dale un título al admin en `src/urls.rs`. El `admin_prefix` debe coincidir con la ruta bajo la que lo anidarás en el siguiente paso (`/admin`) para que los propios enlaces y las acciones de formulario del admin se resuelvan:

```rust
pub fn admin_router(pool: PgPool) -> Router {
    admin::Builder::new(pool)
        .title("Myblog Admin")
        .admin_prefix("/admin") // must match the `.nest("/admin", …)` below
        .build()
}
```

Luego conecta un pool en `src/main.rs` y anida el admin en el router de la API antes de entregárselo al `Cli`. Conserva la línea `mod blog;` del Paso 7 — eso es lo que registra tu modelo `Post` con el admin:

```rust
mod blog;
mod models;
mod urls;
mod views;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let pool = rustango::sql::sqlx::PgPool::connect(&std::env::var("DATABASE_URL")?).await?;

    let api = urls::api().nest("/admin", urls::admin_router(pool));

    rustango::manage::Cli::new()
        .api(api)
        .with_health() // /health + /ready endpoints
        .run()
        .await
}
```

`Cli::new()...run()` es el mismo despachador unificado que generó el generador de andamiaje — sigue sirviendo cada `cargo run -- <verb>`; solo has enriquecido el router que sirve en tiempo de runserver.

Ejecútalo:

```bash
cargo run
```

Abre <http://localhost:8080/admin> (sin barra final). Verás la página de inicio del admin con un enlace `posts`. Haz clic en él para ver tu publicación en borrador en la lista, haz clic en la publicación para abrir su formulario de edición y guarda. La pestaña de rastro de auditoría registra cada escritura.

---

## Paso 12: Construir la API JSON

Un ViewSet expone un modelo como una API REST con endpoints de listar, crear, recuperar, actualizar y eliminar, muy parecido a un ViewSet de Django REST Framework o a un controlador de recursos de API de Laravel.

### 12a. Generar el ViewSet

Genera el andamiaje del archivo y luego rellena qué campos y comportamientos exponer:

```bash
cargo run -- make:viewset PostViewSet --model Post
```

Edita `src/post_view_set.rs`:

```rust
use rustango::ViewSet;
use crate::blog::models::Post;

#[derive(ViewSet)]
#[viewset(
    model         = Post,
    fields        = "id, title, body, status, author_id, published_at",
    filter_fields = "author_id, status",
    search_fields = "title, body",
    ordering      = "-published_at",
    page_size     = 20,
)]
pub struct PostViewSet;
```

Registra el nuevo módulo añadiendo `mod post_view_set;` junto a las otras declaraciones `mod` al principio de `src/main.rs`.

### 12b. Montar las rutas

Conecta las rutas del ViewSet al router de la app (la versión de **Rustango** de un archivo `urls.py` o `routes/api.php`). El router del ViewSet necesita el pool de la base de datos, así que constrúyelo en `src/main.rs`, donde vive el pool, y fusiónalo en el agregador `urls::api()`:

```rust
let api = urls::api()
    .nest("/admin", urls::admin_router(pool.clone()))
    .merge(crate::post_view_set::PostViewSet::router("/api/posts", pool));

rustango::manage::Cli::new()
    .api(api)
    .with_health()
    .run()
    .await
```

(`urls::api()` es el agregador que generó el generador de andamiaje; `manage startapp` fusiona las rutas de cualquier sub-app de la misma manera.)

### 12c. Probar los endpoints

Arranca el servidor:

```bash
cargo run
```

En otra terminal, consulta la API con `curl`:

```bash
curl http://localhost:8080/api/posts                                    # list
curl -X POST http://localhost:8080/api/posts \
     -H "content-type: application/json" \
     -d '{"title":"From API","body":"Yo","status":"published","author_id":1}'
curl http://localhost:8080/api/posts/1                                   # retrieve
curl "http://localhost:8080/api/posts?search=API&ordering=-id"            # search + sort
curl "http://localhost:8080/api/posts?status__ne=draft"                   # lookup operator
```

---

## Paso 13: Dar forma a la salida con un Serializer

Por defecto, el ViewSet devuelve todos los campos del modelo. Un Serializer te permite controlar la forma de la respuesta: ocultar campos internos, renombrarlos o marcar algunos como de solo lectura. Es el mismo papel que un serializer de DRF o un recurso de API de Laravel.

```bash
cargo run -- make:serializer PostSerializer --model Post
```

Edita `src/post_serializer.rs`:

```rust
use rustango::{Auto, Serializer};
use crate::blog::models::Post;

#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
pub struct PostSerializer {
    pub id: Auto<i64>,
    pub title: String,

    #[serializer(source = "body")]                      // rename in API
    pub content: String,

    #[serializer(read_only)]                            // include in GET, ignore in POST/PUT
    pub published_at: Auto<chrono::DateTime<chrono::Utc>>,
}
```

El tipo de cada campo del serializer refleja el campo correspondiente del modelo, así que `id` y `published_at` conservan su envoltorio `Auto<…>` del modelo (un `Auto<i64>` sigue serializándose a un entero JSON plano). Luego registra el módulo añadiendo `mod post_serializer;` junto a las otras declaraciones `mod` en `src/main.rs`.

Conecta el serializer al ViewSet con el atributo `serializer` — las respuestas de listar, recuperar y crear se renderizan entonces a través de él (la proyección `fields` a nivel de campo se omite en favor de la forma del serializer):

```rust
#[derive(ViewSet)]
#[viewset(
    model = Post,
    serializer = crate::post_serializer::PostSerializer,
    ordering = "-published_at",
)]
pub struct PostViewSet;
```

Esto funciona de forma idéntica en PostgreSQL, MySQL y SQLite. Los overrides `method` / `read_only` / `source` / `write_only` se aplican todos a la respuesta, y **los cuerpos de las peticiones también se validan a través del serializer**: `create` / `update` ejecutan su `validate()` (por campo y entre campos), devolviendo un `400` con forma de DRF (`{field: [messages]}`) en caso de fallo, y los campos de solo lectura / calculados que un cliente envíe se ignoran. (Nota: los campos de serializer `nested` / `many` necesitan que las filas relacionadas se carguen mediante `select_related`; de lo contrario se renderizan como su valor por defecto.) Consulta la [guía de ViewSets](viewsets.md) para el comportamiento completo de entrada + salida.

---

## Paso 14: Añadir autenticación JWT

Los JWT son tokens firmados que le entregas a un cliente tras el login y compruebas en cada petición, un patrón común para la autenticación de APIs. El módulo `rustango::jwt` de **Rustango** los emite y los verifica (HS256) y está activo por defecto — sin ningún feature flag adicional.

### 14a. Emitir un token en el login

Incorpora el id del usuario (el "subject" del token) y cualquier claim personalizado, como roles, en un token firmado, y luego entrégaselo al cliente:

```rust
use rustango::jwt::{encode, Claims};
use std::time::Duration;

// Derive the signing key from your session secret.
let secret = std::env::var("RUSTANGO_SESSION_SECRET")?.into_bytes();

let mut claims = Claims::new(user_id.to_string());   // subject = user id
claims.set("roles", vec!["editor"]);
let token = encode(&claims.ttl(Duration::from_secs(900)), &secret)?;

// Send `token` to the client (e.g. in the login response body).
```

### 14b. Verificar el token en cada petición

Decodifica el token — esto comprueba la firma y la caducidad — y luego vuelve a leer los claims. Si falta o es inválido, rechaza la petición como no autorizada:

```rust
use rustango::jwt::decode;

let claims = decode(&access_token, &secret)
    .map_err(|_| StatusCode::UNAUTHORIZED)?;

let user_id = claims.subject().ok_or(StatusCode::UNAUTHORIZED)?;
let roles: Vec<String> = claims.get("roles").unwrap_or_default();
```

### 14c. Ciclo de vida de access + refresh

`rustango::jwt` emite tokens únicos sin estado. Para el patrón completo — tokens de **access** de corta duración, un token de **refresh** de larga duración en una cookie HttpOnly, rotación y una lista negra de JTI para la revocación — habilita la feature `tenancy` y usa `rustango::tenancy::jwt_lifecycle::JwtLifecycle`, cuyos métodos `issue_pair_with` / `verify_access` / `refresh` gestionan el par por ti.

---

## Paso 15: Añadir middleware de seguridad

El middleware envuelve cada petición para añadir comportamiento transversal. Aquí apilas IDs de petición, registro de acceso, límite de tasa, CORS y cabeceras de seguridad en una cadena. Cada `.method(...)` añade una capa, similar al middleware de Django o a la pila de middleware de Laravel. Consulta la [guía de Middleware](middleware.md) para el catálogo completo de capas y las reglas de ordenamiento.

```rust
use rustango::security_headers::{SecurityHeadersLayer, SecurityHeadersRouterExt, CspBuilder};
use rustango::cors::{CorsLayer, CorsRouterExt};
use rustango::rate_limit::{RateLimitLayer, RateLimitRouterExt};
use rustango::access_log::{AccessLogLayer, AccessLogRouterExt};
use rustango::request_id::{RequestIdLayer, RequestIdRouterExt};
use rustango::health::health_router;
use std::time::Duration;

let app = urls::api()
    .nest("/admin", urls::admin_router(pool.clone()))
    .merge(crate::post_view_set::PostViewSet::router("/api/posts", pool.clone()))
    .merge(health_router(pool.clone()))                        // /health, /ready
    .request_id(RequestIdLayer::default())
    .access_log(AccessLogLayer::default())                      // PII-redacted
    .rate_limit(RateLimitLayer::per_ip(60, Duration::from_secs(60)))
    .cors(CorsLayer::new()
        .allow_origins(vec!["https://app.example.com"])
        .allow_methods(vec!["GET", "POST", "PUT", "PATCH", "DELETE"]))
    .security_headers(
        SecurityHeadersLayer::strict()
            .csp(CspBuilder::strict_starter().build()),
    );
```

Entrega la `app` terminada al `Cli` exactamente como antes — `rustango::manage::Cli::new().api(app).with_welcome().run().await` — y cada petición fluye ahora a través de la pila de middleware completa.

---

## Paso 16: Escribir pruebas

**Rustango** incluye un cliente de pruebas que dirige tu router en el mismo proceso, para que puedas hacer aserciones sobre respuestas HTTP reales sin arrancar un servidor, muy parecido al cliente de pruebas de Django o a las pruebas HTTP de Laravel. Genera el andamiaje de un archivo de pruebas:

```bash
cargo run -- make:test PostSmoke      # generates tests/post_smoke.rs
```

Los generadores `make:*` toman un nombre en PascalCase; `PostSmoke` se convierte en el archivo en snake_case `tests/post_smoke.rs`.

Edita `tests/post_smoke.rs`. Las pruebas de integración viven en una crate separada, así que construyen el router bajo prueba directamente a partir del ViewSet (la misma llamada `router(...)` que montaste en el Paso 12b):

```rust
use rustango::test_client::TestClient;
use myblog::post_view_set::PostViewSet;
use rustango::sql::sqlx::PgPool;
use serde_json::json;

async fn app() -> axum::Router {
    let pool = PgPool::connect(&std::env::var("DATABASE_URL").unwrap()).await.unwrap();
    PostViewSet::router("/api/posts", pool)
}

#[tokio::test]
async fn list_posts_returns_200() {
    let client = TestClient::new(app().await);
    let response = client.get("/api/posts").send().await;
    assert_eq!(response.status, 200);
    let v = response.json_value();
    assert!(v["results"].is_array());
}

#[tokio::test]
async fn create_post_returns_the_new_object() {
    let client = TestClient::new(app().await);
    let response = client.post("/api/posts")
        .json(&json!({
            "title": "Test",
            "body":  "x",
            "status": "draft",
            "author_id": 1,
        }))
        .send().await;
    assert_eq!(response.status, 201);
    let v: serde_json::Value = response.json();
    assert_eq!(v["title"], "Test");
}
```

> **Atención:** las pruebas de integración en `tests/` solo pueden hacer `use myblog::…` si la crate expone un target de librería. Un andamiaje nuevo es solo binario (`src/main.rs`, sin `src/lib.rs`), así que añade un `src/lib.rs` de una línea que reexporte los módulos que quieras probar — `pub mod models; pub mod post_view_set; pub mod urls;` — y conserva las líneas `mod …;` correspondientes en `src/main.rs`. (Si prefieres no añadir un target de librería, construye el router completamente en línea dentro de la prueba, tal como `make:test` genera su `app()`.)

Ejecuta las pruebas:

```bash
cargo test --test post_smoke
```

---

## Paso 17: Ejecutar la comprobación del sistema

Antes de desplegar, ejecuta el comprobador integrado. Señala malas configuraciones comunes (como un `RUSTANGO_SESSION_SECRET` débil o una base de datos inalcanzable), similar a `check --deploy` de Django.

```bash
cargo run -- check --deploy
```

En tu entorno de desarrollo local verás algo como:

```
running rustango system check (deploy mode)...
  [info]    6 models registered via inventory
  [info]    database reachable
  [info]    1 migration(s) on disk
  [info]    RUSTANGO_SESSION_SECRET length OK
  [info]    config tier resolved to `dev`
  [warning] RUSTANGO_ENV is unset — set to `prod` so config loaders pick the right tier
  [warning] DATABASE_URL points at localhost / 127.0.0.1 — verify this is intended in production
  [warning] RUSTANGO_APEX_DOMAIN is unset / `localhost` — set it for tenancy projects
```

(Los recuentos exactos de modelos/migraciones dependen de tu proyecto.) Esas tres advertencias son las esperadas para el entorno de desarrollo. En una configuración de producción — `RUSTANGO_ENV=prod`, una `DATABASE_URL` de base de datos gestionada, un dominio ápice configurado — desaparecen y verás `all checks passed`. Corrige cualquier advertencia o error restante antes de hacer push a producción.

---

## Paso 18: Desplegar a producción

Cómo despliegas depende de tu plataforma (Fly, Railway, Kubernetes, ECS a secas, etc.). Los pasos del lado del framework son los mismos en todas partes; el flag `--release` compila un binario optimizado:

```bash
# 1. Set production env
export RUSTANGO_ENV=prod
export DATABASE_URL=postgres://prod-host/myblog
export RUSTANGO_SESSION_SECRET=$(openssl rand -base64 32)

# 2. Run migrations
cargo run --release -- migrate

# 3. Audit
cargo run --release -- check --deploy

# 4. Build binary
cargo build --release

# 5. Run with a process supervisor (systemd / docker / k8s)
./target/release/myblog
```

Asegúrate de que tu proxy inverso:
- Termine HTTPS
- Reenvíe `X-Forwarded-For` para obtener IPs precisas en `AccessLogLayer`
- Reenvíe `X-Forwarded-Host`, `X-Forwarded-Proto`
- Use `axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())` para que `ConnectInfo` quede poblado para el límite de tasa + el filtrado de IP

---

## Adónde ir a continuación

| Tema | Doc |
|---|---|
| Versión ejecutable de esta guía | [`examples/getting_started_blog`](../crates/rustango/examples/getting_started_blog) |
| Cada subcomando de `manage` | [`docs/manage.md`](manage.md) |
| Recetario del ORM (filtros avanzados, agregaciones, M2M, soft delete) | [`docs/orm.md`](orm.md) |
| Middleware (el catálogo completo de capas + ordenamiento) | [`docs/middleware.md`](middleware.md) |
| Benchmarks de rendimiento (vs. Go) | [`docs/benchmarks.md`](benchmarks.md) |
| Convenciones de la API (nomenclatura, patrones builder, feature gates) | [`docs/api-conventions.md`](api-conventions.md) |
| Funciones de seguridad en profundidad | [`docs/security.md`](security.md) |
| Auditoría de paridad con Django | [`docs/django-parity-audit-2026-05-21.md`](django-parity-audit-2026-05-21.md) |
| Multi-tenancy | [README — sección Multi-tenancy](../README.md#multi-tenancy) |
| Documentación de la API | <https://docs.rs/rustango> |

Si te topas con algo que no funciona o no queda claro, abre un issue.
