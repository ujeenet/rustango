# Primeros pasos: construir un blog con Rustango

Este recorrido te lleva desde un directorio vacío hasta un blog desplegado: publicaciones, una interfaz de administración, una API JSON con registro e inicio de sesión (JWT) y pruebas. De principio a fin.

> **Tiempo:** ~45 minutos para el recorrido completo, ~10 minutos si solo quieres verlo funcionar.
>
> **Versión ejecutable:** cada paso a continuación está replicado en un ejemplo probado y compilable en [`crates/rustango/examples/getting_started_blog`](https://github.com/ujeenet/rustango/tree/develop/crates/rustango/examples/getting_started_blog). Si algún paso parece estar mal, compáralo con ese ejemplo.

[![Construir un blog con Rustango: generar la migración, aplicarla, arrancar el servidor y consultar la API JSON — todo desde un único binario](../img/getting-started.png)](../img/getting-started.png)

---

## Qué necesitas saber primero

Dos preguntas distintas, y la documentación solo respondía a la segunda.

**Se da por supuesto Rust.** No hace falta ser experto, pero deberías sentirte
cómodo con structs, traits, `Result` y `?`, y con el `async`/`.await` suficiente
para leer una función sin tener que buscar cosas. Si todavía no es tu caso,
primero está el [Rust Book](https://doc.rust-lang.org/book/); esta guía no va a
enseñarte el lenguaje que hay debajo.

**No se da por supuesta experiencia en backend web.** Si nunca has construido
una API web, empieza por [Fundamentos de las API web](glossary.md#fundamentos-de-las-api-web)
en el glosario. Es una introducción de cinco minutos a peticiones, rutas,
handlers y migraciones, y está escrita justo para cubrir ese hueco. Vuelve aquí
después.

**No se da por supuesta experiencia previa con ningún framework web.** Cuando
esta documentación hace alguna comparación, es un apunte al margen, nunca la
explicación: si un paralelismo no te dice
nada, sáltatelo y el paso se sostiene igual por sí solo. Cuando un término hace
trabajo de verdad, el [glosario](glossary.md) lo define en lenguaje llano.

## Qué necesitas instalado

| Herramienta | Para qué | Instalación |
|---|---|---|
| Rust 1.88+ | Compilador | <https://rustup.rs> |
| Una base de datos | Esta guía usa Postgres | ver [Elegir base de datos](#elegir-base-de-datos) abajo |
| `psql` (opcional) | Inspeccionar la BD | `brew install libpq` / `apt install postgresql-client` |

```bash
rustc --version    # should print 1.88+
```

### Elegir base de datos

**Docker no es obligatorio.** Esta guía lo usa porque un solo comando te da un
Postgres desechable, pero nada en rustango depende de él. Elige la fila que
encaje con tu máquina; todo lo demás es idéntico:

| Si quieres | Haz esto | Notas |
|---|---|---|
| **Ningún servidor de base de datos** | Ejecutar con SQLite (abajo) | Nada que instalar. Lo mejor para aprender. |
| Postgres **sin** Docker | Instalar Postgres de forma nativa y apuntar `DATABASE_URL` a `localhost` | Ver [Postgres nativo](#postgres-nativo-sin-docker). |
| Postgres **con** Docker | `docker compose up -d` en el proyecto generado | Lo que asume el resto de esta guía. |
| MySQL o MariaDB | Generar el proyecto con `--backend mysql` | Ver [MySQL](#mysql). |

Elijas la que elijas, lo único que cambia es `DATABASE_URL`. Esta es la forma de cada una:

```bash
DATABASE_URL=postgres://user:password@localhost:5432/myblog_dev
DATABASE_URL=mysql://user:password@localhost:3306/myblog_dev
DATABASE_URL=sqlite://myblog_dev.db?mode=rwc
```

#### SQLite — sin configuración

Genera el proyecto directamente para SQLite: no hay nada que instalar, nada
que arrancar y nada que editar después.

```bash
cargo rustango new myblog --backend sqlite
```

El `.env.example`, el `docker-compose.yml` y los niveles de configuración
generados están todos escritos para SQLite, la base de datos es un fichero que
crea el primer `cargo run -- migrate`, y puedes saltarte
[el paso 4](#paso-4-arrancar-la-base-de-datos) por completo.

Si ya generaste un proyecto con Postgres y quieres cambiarlo, cada plantilla
mantiene los tres backends cableados — así que es un flag más una URL:

```bash
cargo run --no-default-features --features sqlite
```

```bash
DATABASE_URL=sqlite://myblog_dev.db?mode=rwc
```

`mode=rwc` le dice a SQLite que cree el fichero si no existe. Todo lo de esta
guía — modelos, migraciones, el admin, el ORM — funciona igual; solo quedan
fuera las funciones específicas de Postgres (operadores `JSONB`, multi-tenancy
en modo esquema).

#### Postgres nativo (sin Docker)

Instala Postgres con tu gestor de paquetes (`brew install postgresql@16`,
`apt install postgresql`, o el instalador de Windows en
<https://www.postgresql.org/download/windows/>) y crea el rol y la base de datos
que espera la configuración generada:

```bash
createuser -s rustango          # o: CREATE ROLE rustango LOGIN SUPERUSER PASSWORD 'rustango';
createdb myblog_dev -O rustango
```

El `.env.example` generado apunta al nombre del servicio de Docker. Cambia el
host a `localhost`:

```bash
# .env  —  `postgres` es el nombre del servicio de docker-compose; en nativo, localhost
DATABASE_URL=postgres://rustango:rustango@localhost:5432/myblog_dev
```

> **¿En Windows?** El backend Hyper-V / WSL2 de Docker Desktop es una causa
> frecuente de fallos al arrancar. Si te da guerra, usa la vía SQLite de arriba
> para aprender el framework y vuelve a Docker cuando prepares el despliegue —
> para eso está realmente la configuración de contenedores.

> **¿Ya tienes Postgres corriendo en local?** Entonces el puerto 5432 está
> ocupado y el contenedor pierde la carrera en silencio. Tu aplicación se
> conecta al servidor local, que no tiene ninguna de tus tablas. El error que
> devuelve no es legible, porque un servidor que no está en inglés manda su
> mensaje en su propia codificación. O paras el servicio local, o mueves el
> contenedor a otro puerto.

#### MySQL

Genera el proyecto con `--backend mysql` y el `.env.example`, el
`docker-compose.yml` y los niveles de configuración generados estarán todos
escritos para MySQL:

```bash
cargo rustango new myblog --backend mysql
```

Ejecutarlo de forma nativa en lugar de en el contenedor requiere una base de
datos y un usuario:

```sql
CREATE DATABASE myblog_dev CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_as_cs;
CREATE USER 'rustango'@'localhost' IDENTIFIED BY 'rustango';
GRANT ALL PRIVILEGES ON myblog_dev.* TO 'rustango'@'localhost';
```

```bash
DATABASE_URL=mysql://rustango:rustango@localhost:3306/myblog_dev
```

Merece la pena poner `utf8mb4` a propósito: el `utf8` antiguo de MySQL es de
tres bytes y no puede almacenar un emoji, algo que aparece mucho más tarde como
una escritura que falla en una sola fila. MariaDB funciona con el mismo driver y
el mismo esquema de URL.

La collation también importa. Las columnas heredan la de la base de datos, y la de
MySQL (`utf8mb4_0900_ai_ci`) ignora mayúsculas y acentos: `.eq("/About")` encuentra
`/about` y una columna `unique` rechaza `Hero` junto a `hero`, a diferencia de
PostgreSQL y SQLite. `utf8mb4_0900_as_cs` compara como ellos. Evita una collation
`_bin`: el driver no puede leer sus columnas de texto como `String`.
`manage check --deploy` avisa de ambas cosas.

---

## Paso 1: Instalar el generador de andamiaje

El generador de andamiaje crea esqueletos de proyectos y apps por ti.

```bash
cargo install cargo-rustango
```

Esto añade el subcomando `cargo rustango ...` de forma global. Confirma que está ahí:

```bash
cargo rustango --help
```

La versión del propio generador de andamiaje es la que fija tu proyecto, así que instalar el más reciente te da el rustango más reciente. Para generar un proyecto sobre una versión anterior, instala ese generador en su lugar (`cargo install cargo-rustango --version 0.66.0`) — véase [Andamiaje](scaffolding.md#la-versión-del-propio-generador-es-la-que-obtiene-tu-proyecto).

---

## Paso 2: Crear el proyecto

Esto genera el andamiaje de un proyecto nuevo, con estructura, ajustes y migraciones listos.

```bash
cd ~/projects                                 # wherever you keep code
cargo rustango new myblog                     # default = fullstack template
cd myblog
```

Ejecuta `cargo rustango new` sin argumento alguno y te pregunta la plantilla,
el backend y las funcionalidades extra, y luego imprime la línea de comandos
equivalente antes de crear nada — véase [Andamiaje](scaffolding.md).

Esto es lo que se generó:

```
myblog/
├── Cargo.toml                  # rustango + axum + sqlx + tokio
├── .env.example                # template for DATABASE_URL etc.
├── .gitignore
├── docker-compose.yml          # Postgres in a container
├── Dockerfile                  # release image; Dockerfile.dev is the dev one
├── README.md                   # project-specific
├── config/                     # tiered settings (default + dev/staging/prod)
├── migrations/                 # empty — `cargo run -- makemigrations` populates
├── system/migrations/          # the framework's own tables
└── src/
    ├── lib.rs                  # the app as a library: `pub mod models; …`
    ├── main.rs                 # entry point: `Cli::new().api(urls::api()).run()`
    ├── models.rs               # every #[derive(Model)] lives here
    ├── views.rs                # axum request handlers
    └── urls.rs                 # `pub fn api()` route aggregator
```

Hay un único binario: `cargo run` arranca el servidor HTTP, y cada verbo de gestión (`migrate`, `makemigrations`, `startapp`, `check`, …) pasa por el mismo binario mediante `cargo run -- <verb>`. No hay un binario `manage` aparte.

Los módulos de la app viven en el target de librería (`src/lib.rs`), y `src/main.rs` llega a ellos por el nombre de la crate (`use myblog::urls;`). Eso es lo que permite que las pruebas en `tests/` y los binarios extra en `src/bin/` usen tus modelos.

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

La configuración vive en un archivo `.env`. Copia la plantilla:

```bash
cp .env.example .env
```

El `.env` generado es compatible con Docker de fábrica. Como vamos a ejecutar `cargo` en el host (no dentro del contenedor de desarrollo), cambia el host de la base de datos de `postgres` a `localhost`:

```bash
DATABASE_URL=postgres://rustango:rustango@localhost:5432/myblog_dev
```

Las credenciales, el puerto y el nombre de la base de datos (`myblog_dev`) ya coinciden con el servicio Postgres del `docker-compose.yml`, así que no necesitas tocarlos.

`RUSTANGO_SESSION_SECRET` firma sesiones y tokens. En el `.env.example` generado queda **comentado**, y para desarrollo puedes dejarlo así: el primer arranque genera una clave en `./var/` y la reutiliza, de modo que los reinicios no te cierran la sesión.

Para producción pon una real — 32 bytes en base64, desde tu gestor de secretos o:

```bash
openssl rand -base64 32     # paste output as RUSTANGO_SESSION_SECRET value
```

Un valor que no sean 32 bytes de base64 no se puede usar. Fuera del nivel prod, el servidor lo avisa al arrancar y recurre a la clave generada, así que atento a ese aviso si has puesto una y las sesiones se comportan como si no. Con `RUSTANGO_ENV=prod`, un secreto ausente o inválido impide que el servidor arranque.

---

## Paso 4: Arrancar la base de datos

> **¿Usas SQLite?** Sáltate este paso — no hay servidor que arrancar. Asegúrate
> de que `.env` tenga `DATABASE_URL=sqlite://myblog_dev.db?mode=rwc` y añade
> `--no-default-features --features sqlite` a cada `cargo run` de abajo.
>
> **¿Usas un Postgres nativo?** Ya está corriendo como servicio; solo comprueba
> que `psql "$DATABASE_URL" -c "SELECT version();"` funciona y sigue adelante.


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

Las migraciones crean las tablas de tu base de datos. Ejecútalas una vez para preparar las propias tablas del framework:

```bash
cargo run -- migrate
```

La primera compilación tarda ~2 minutos (Rust compila todo desde el código fuente). Un proyecto nuevo aún no incluye archivos de migración propios, pero `migrate` no es un no-op: primero genera las migraciones propias del framework a partir de los modelos compilados y las aplica, así que verás algunas líneas `applied …` en lugar de `nothing to migrate`. (Ese mensaje solo aparece cuando todo — framework y proyecto — ya está al día.) También crea la tabla de registro de auditoría, para que los modelos auditados funcionen en cuanto los añadas. Generarás tu primera migración de proyecto en el Paso 9.

Comprueba el estado de las migraciones:

```bash
cargo run -- showmigrations
```

En un proyecto nuevo esto imprime `(no migrations in ./migrations)` y luego la cadena propia del framework bajo `System migrations in ./system/migrations:`. Una vez que crees un modelo y ejecutes `makemigrations` (Paso 9), cada migración aplicada muestra aquí una `[X]`.

---

## Paso 6: Primer arranque

Arranca el servidor para asegurarte de que todo está conectado.

```bash
cargo run
```

Verás:

```
INFO rustango::server: listening on http://127.0.0.1:8080
```

`cargo run` también aplica las migraciones pendientes antes de empezar a escuchar.

Abre <http://localhost:8080> en tu navegador. El andamiaje incluye un handler raíz sencillo (`views::index`) que te saluda con **Hello from Rustango!** y un enlace al admin — eso confirma que **Rustango** está funcionando. (Los proyectos que no definen su propia ruta `/` obtienen en su lugar una página de bienvenida integrada, mediante `Cli::with_welcome()`.)

Pulsa Ctrl-C para detenerlo.

---

## Paso 7: Crear una app

Una "app" es un módulo de funcionalidad autocontenido. Tu app de blog contendrá el modelo Post, sus rutas y sus plantillas.

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

`startapp` conecta el nuevo módulo por ti: declara `pub mod blog;` en `src/lib.rs` (o `mod blog;` en `src/main.rs` en un proyecto antiguo solo binario) e inserta una línea `.merge(crate::blog::urls::api())` en el agregador `api()` de `src/urls.rs`, de modo que las rutas del blog se componen en la app automáticamente. No hace falta registrar el módulo manualmente.

---

## Paso 8: Definir un modelo

Un modelo es una tabla de base de datos descrita como un struct de Rust — una clase de tipo Active Record en Rust. Abre `src/blog/models.rs` y define tu `Post`. (Para la referencia completa — cada tipo de campo, claves primarias personalizadas y todos los atributos — consulta la [guía de Modelos](models.md).)

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

Ahora convierte ese modelo en una tabla real. Primero, genera la migración a partir de tu modelo:

```bash
cargo run -- makemigrations
```

Verás algo como:

```
wrote ./migrations/0001_create_item_and_posts.json
    + CreateTable("item")
    + CreateTable("posts")
    + CreateIndex { name: "posts_status_published_at_idx", table: "posts", columns: ["status", "published_at"], ... }
```

Esta primera migración crea tus modelos — `posts`, además del modelo inicial `item` que el andamiaje incluyó en `src/models.rs`. Las tablas propias del framework (usuarios del admin, tipos de contenido, registro de auditoría, …) no están en ella: tienen su propia cadena en `system/migrations/`, que el primer `migrate` escribe y aplica antes que la tuya. Abre el JSON si quieres: contiene las operaciones más una instantánea completa del esquema.

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

Vamos a leer y escribir filas desde el código. El ORM te permite trabajar con las filas de la base de datos como structs de Rust en lugar de SQL en crudo.

Edita temporalmente `src/main.rs` para ejecutar una prueba rápida de crear-y-leer antes de arrancar el servidor. Reemplaza el cuerpo del `Cli` con una prueba de humo del ORM improvisada (conserva el `#[rustango::main]` del generador de andamiaje). Los modelos viven en la librería, así que `main.rs` los nombra por el nombre de la crate:

```rust
use myblog::blog::models::Post;
use rustango::sql::{FetcherPool, Pool};
use rustango::{Auto, Model};

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let pool = Pool::connect(&std::env::var("DATABASE_URL")?).await?;

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
    let posts = Post::objects().fetch(&pool).await?;
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
- Para guardar una fila, llama a `.save(&pool)` sobre ella. Para leer filas, construye una consulta con `Post::objects()` y ejecútala con `.fetch(&pool)` — sin filtros, eso devuelve todas las filas de la tabla.
- `.fetch(…)` viene del trait `FetcherPool`, y por eso los imports lo traen. Sin esa línea el método no existe y el compilador lo dice sin explicar por qué.
- Estas son las llamadas multi-backend, y todo lo anterior compila sin cambios en las tres bases de datos. Consulta la [guía del ORM](orm.md).

Ejecútalo:

```bash
cargo run
```

Deberías ver el id de tu nueva publicación y las filas leídas de vuelta. Restaura `src/main.rs` a su forma de servidor andamiada una vez que hayas confirmado que funciona — el siguiente paso parte de ahí. Quita también las tres líneas `use` de arriba: la forma de servidor no las usa y el compilador avisa de imports sin usar.

---

## Paso 11: Activar el auto-admin

**Rustango** incluye una interfaz de administración generada para tus modelos — un backoffice listo para explorar, buscar y editar tus datos sin escribir código. El scaffold fullstack ya la deja cableada: un helper en `src/urls.rs` convierte un pool en un router de admin, y `src/main.rs` lo anida bajo `/admin`.

El helper pone el admin detrás de un inicio de sesión. Lee sus opciones de la configuración cargada, y `admin_prefix` debe coincidir con la ruta bajo la que está anidado para que los propios enlaces y las acciones de formulario del admin se resuelvan. `admin_prefix(settings)` devuelve `[admin] url_prefix` (o `[routes] admin_url`), y si no, `/admin`:

```rust
use rustango::admin;
use rustango::config::Settings;
use rustango::session::{tier_from_env, try_load_session_secret_for_tier, SessionSecretError};
use rustango::sql::Pool;

pub fn admin_router(pool: Pool, settings: &Settings) -> Result<Router, SessionSecretError> {
    let secret = try_load_session_secret_for_tier(
        &tier_from_env(),
        std::path::Path::new("./var/.rustango_session.key"),
    )?;
    Ok(admin::Builder::from_settings(pool, settings)
        .admin_prefix(admin_prefix(settings)) // the path main.rs nests it at
        .with_session_auth(secret)
        .build())
}
```

> **Conserva `.with_session_auth(...)`.** Sin él, el admin no tiene inicio de sesión: cualquiera que llegue a `/admin` puede leer, editar y borrar todos los modelos. `check --deploy` avisa cuando un admin se construye sin él (consulta [admin.md](admin.md)).

`Builder::new` acepta el pool de cualquier backend, así que este helper no nombra ningún driver y funciona en los tres.

`src/main.rs` entrega el helper al `Cli`, que construye el admin a partir del pool con el que sirve, así que verbos como `makemigrations` siguen funcionando sin base de datos. Conserva la línea `pub mod blog;` del Paso 7 en `src/lib.rs` — eso es lo que registra tu modelo `Post` con el admin. El `main.rs` generado se ve así (recortado):

```rust
use myblog::urls;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let loaded = rustango::config::Settings::load_from_env();
    let settings = loaded.as_ref().cloned().unwrap_or_default();
    let admin_settings = settings.clone();
    rustango::manage::Cli::new()
        .api(urls::api())
        .try_nest_with(urls::admin_prefix(&settings), move |pool| {
            urls::admin_router(pool, &admin_settings)
        })
        .with_health() // /health + /ready endpoints
        .with_loaded_settings(loaded)
        .run()
        .await
}
```

Crea la cuenta con la que iniciarás sesión (te pedirá una contraseña):

```bash
cargo run -- create-admin alice --superuser
```

`Cli::new()...run()` es el mismo despachador unificado que generó el generador de andamiaje — sigue sirviendo cada `cargo run -- <verb>`; solo has enriquecido el router que sirve en tiempo de runserver.

Ejecútalo:

```bash
cargo run
```

Abre <http://localhost:8080/admin> (sin barra final) e inicia sesión como `alice`. Verás la página de inicio del admin con un enlace `posts`. Haz clic en él para ver tu publicación en borrador en la lista, haz clic en la publicación para abrir su formulario de edición y guarda. La pestaña de rastro de auditoría registra cada escritura.

---

## Paso 12: Construir la API JSON

Un ViewSet expone un modelo como una API REST con endpoints de listar, crear, recuperar, actualizar y eliminar — a partir de una sola declaración, sin escribir las rutas a mano.

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
    read_only,
)]
pub struct PostViewSet;
```

Registra el nuevo módulo añadiendo `pub mod post_view_set;` a `src/lib.rs` (`make:viewset` te imprime esta línea).

Mantén `read_only` por ahora: la API solo sirve list y retrieve. Sin él, el ViewSet también serviría create, update y delete a cualquiera, así que se niega a montar hasta que digas quién puede escribir. El [Paso 14](#paso-14-registro-inicio-de-sesión-y-autenticación-jwt) añade cuentas y activa las escrituras para usuarios con sesión.

### 12b. Montar las rutas

Conecta las rutas del ViewSet a la app. El router del ViewSet necesita el pool de la base de datos, y `urls::api()` no tiene ninguno, así que añade un `nest_with` más a la cadena del `Cli` en `src/main.rs`. Recibe el pool del servidor, igual que el admin:

```rust
rustango::manage::Cli::new()
    .api(urls::api())
    .try_nest_with(urls::admin_prefix(&settings), move |pool| {
        urls::admin_router(pool, &admin_settings)
    })
    .nest_with("/api", |pool| myblog::post_view_set::PostViewSet::router("/posts", pool))
    .with_health()
    .with_loaded_settings(loaded)
    .run()
    .await
```

El ViewSet responde ahora en `/api/posts` y `/api/posts/{pk}`. La ruta es la tabla del modelo; el comentario al final de `src/post_view_set.rs` muestra esta misma línea, y `make:viewset` imprime la URL.

### 12c. Probar los endpoints

Arranca el servidor:

```bash
cargo run
```

En otra terminal, consulta la API con `curl`:

```bash
curl http://localhost:8080/api/posts                                    # list
curl http://localhost:8080/api/posts/1                                   # retrieve
curl "http://localhost:8080/api/posts?search=API&ordering=-id"            # search + sort
curl "http://localhost:8080/api/posts?status__ne=draft"                   # lookup operator
```

Por ahora un `POST` responde `405 Method Not Allowed`. Escribirás posts por la API en el Paso 14.

---

## Paso 13: Dar forma a la salida con un Serializer

Por defecto, el ViewSet devuelve todos los campos del modelo. Un Serializer te permite controlar la forma de la respuesta: ocultar campos internos, renombrarlos o marcar algunos como de solo lectura. Es el contrato entre tus modelos y el JSON que sirve tu API.

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

    pub author_id: i64,                                 // writable: NOT NULL with no default

    #[serializer(read_only)]                            // include in GET, ignore in POST/PUT
    pub published_at: Auto<chrono::DateTime<chrono::Utc>>,
}
```

Una vez que hay un serializer conectado, **sus campos son toda la superficie de escritura**: cualquier cosa que un cliente envíe y que no esté listada aquí se descarta antes del `INSERT`. Por eso aparece `author_id`. En el modelo es `NOT NULL` y no tiene valor por defecto, así que omitirlo hace que toda creación falle por la restricción not-null. `status` puede quedarse fuera porque el modelo le da `default = "'draft'"`.

El tipo de cada campo del serializer refleja el campo correspondiente del modelo, así que `id` y `published_at` conservan su envoltorio `Auto<…>` del modelo (un `Auto<i64>` sigue serializándose a un entero JSON plano). Luego registra el módulo añadiendo `pub mod post_serializer;` a `src/lib.rs`.

Conecta el serializer al ViewSet con el atributo `serializer` — las respuestas de listar, recuperar y crear se renderizan entonces a través de él. `fields` sigue siendo obligatorio: un body solo escribe los campos listados aquí y en el serializer:

```rust
#[derive(ViewSet)]
#[viewset(
    model = Post,
    fields = "id, title, body, author_id, published_at",
    serializer = crate::post_serializer::PostSerializer,
    ordering = "-published_at",
    read_only,
)]
pub struct PostViewSet;
```

Esto funciona de forma idéntica en PostgreSQL, MySQL y SQLite. Los overrides `method` / `read_only` / `source` / `write_only` se aplican todos a la respuesta, y **los cuerpos de las peticiones también se validan a través del serializer**: `create` / `update` ejecutan su `validate()` (por campo y entre campos), devolviendo un `422` (`validation_failed`) con una lista de mensajes por campo en `details` (`{field: [messages]}`) en caso de fallo, y los campos de solo lectura / calculados que un cliente envíe se ignoran. (Nota: los campos de serializer `nested` / `many` necesitan que las filas relacionadas se carguen mediante `select_related`; de lo contrario se renderizan como su valor por defecto.) Consulta la [guía de ViewSets](viewsets.md) para el comportamiento completo de entrada + salida.

---

## Paso 14: Registro, inicio de sesión y autenticación JWT

Hasta ahora cualquiera puede leer posts y nadie puede escribirlos. En este paso la gente crea una cuenta, inicia sesión y recibe un token. Con el token puede escribir posts. La lectura sigue siendo pública.

Primero, algunas palabras:

- Un **hash de contraseña** es una transformación de un solo sentido de una contraseña. Guardas el hash, nunca la contraseña. Al iniciar sesión se vuelve a calcular el hash del intento y se compara.
- Un **JWT** (JSON Web Token) es un texto pequeño y firmado. El servidor lo firma con una clave secreta, así que nadie puede cambiarlo sin la clave. Contiene el id del usuario y una hora de caducidad.
- El cliente devuelve el token en cada petición, en la cabecera `Authorization: Bearer <token>`.
- Un **middleware** es código que se ejecuta antes de tu handler. Aquí comprueba el token.

`rustango::passwords` calcula hashes de contraseñas (argon2id). `rustango::jwt` firma y comprueba tokens (HS256). Ambos están activos por defecto. El código terminado está en el proyecto de ejemplo [`getting_started_blog`](https://github.com/ujeenet/rustango/tree/develop/crates/rustango/examples/getting_started_blog), y su `tests/auth_flow.rs` comprueba cada caso de abajo.

### 14a. Un modelo de usuario

Añade un segundo modelo en `src/blog/models.rs`, debajo de `Post`. Los imports del principio del archivo se quedan como están:

```rust
/// Someone who can sign in to the API (Step 14).
#[derive(Model, Clone, Debug)]
#[rustango(table = "api_users", display = "username")]
pub struct ApiUser {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(max_length = 150, unique)]
    pub username: String,

    // An argon2 hash, never the password itself.
    #[rustango(max_length = 255)]
    pub password_hash: String,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}
```

`unique` hace que la base de datos rechace una segunda fila con el mismo nombre de usuario. Crea y aplica la migración, como en el Paso 9:

```bash
cargo run -- makemigrations     # wrote ./migrations/0002_create_api_users.json
cargo run -- migrate
```

Nunca incluyas `password_hash` en un ViewSet ni en un serializer: el framework se niega a montar uno que nombre una columna de credenciales, así que un hash no puede filtrarse por la API. El admin muestra la nueva tabla y enmascara esa columna.

### 14b. La clave de firma

El servidor necesita una clave secreta para firmar tokens. Crea una con al menos 32 bytes aleatorios y ponla en `.env`:

```bash
echo "JWT_SECRET=$(openssl rand -base64 32)" >> .env
```

En PowerShell, ejecuta las mismas tres líneas que el Paso 18 usa para `RUSTANGO_SESSION_SECRET` y añade el valor a `.env` como `JWT_SECRET=...`. Usa otra clave en producción y mantenla fuera de git. `.env` ya está en `.gitignore`.

### 14c. Registrarse, iniciar sesión, comprobar el token

Crea `src/blog/auth.rs`. Contiene todo el código de inicio de sesión. Aquí viene en tres partes; pégalas una tras otra. Después añade `pub mod auth;` a `src/blog/mod.rs`.

**Parte 1: imports, tipos y pequeños ayudantes.**

```rust
//! Sign-up, sign-in and the Bearer-token guard for the blog API (Step 14).

use std::collections::HashMap;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rustango::api_errors::ApiError;
use rustango::core::{ModelSchema, WhereExpr};
use rustango::jwt::{self, Claims};
use rustango::login_throttle::{self, ClientIp, LoginRefused, LoginScope};
use rustango::passwords::{self, PasswordError};
use rustango::prelude::*;
use rustango::sql::{sqlx, ExecError, Pool};
use rustango::viewset::{match_nothing, ViewSetFilter, WritePin};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::blog::models::{ApiUser, Post};

/// How long a token from `/api/login` stays valid.
pub const TOKEN_TTL: Duration = Duration::from_secs(15 * 60);

/// What `/api/register` and `/api/login` read from the request body.
#[derive(Deserialize)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

/// What `/api/login` sends back.
#[derive(Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: &'static str,
    pub expires_in: u64,
}

/// The signed-in user. The guard puts it on the request for the handlers.
#[derive(Clone, Debug)]
pub struct CurrentUser {
    pub id: i64,
    pub username: String,
}

/// The signing key, from the `JWT_SECRET` environment variable.
fn jwt_secret() -> Result<Vec<u8>, ApiError> {
    match std::env::var("JWT_SECRET") {
        Ok(secret) if secret.len() >= 32 => Ok(secret.into_bytes()),
        _ => Err(ApiError::logged(
            StatusCode::INTERNAL_SERVER_ERROR,
            "auth",
            "JWT_SECRET is unset or shorter than 32 bytes",
        )),
    }
}

/// Methods that only read. Everything else needs a token.
pub fn is_read(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// Usernames are stored lower-case, so `Alice` and `alice` are one account.
fn normalize(username: &str) -> String {
    username.trim().to_lowercase()
}

/// A 500 that logs the cause and tells the client nothing about it.
fn server_error(e: impl std::fmt::Display) -> ApiError {
    ApiError::logged(StatusCode::INTERNAL_SERVER_ERROR, "auth", e)
}

/// The same answer for an unknown user and a wrong password.
fn bad_credentials() -> ApiError {
    ApiError::unauthorized("invalid username or password")
}
```

`ApiError` envía la forma habitual de error JSON del framework (`{"error": ..., "message": ..., "status": ...}`), así que tus errores se parecen a los del ViewSet.

**Parte 2: los dos handlers.** Un handler es una `async fn` que responde a una ruta. `State(pool)` le pasa el pool de la base de datos; `Json(body)` lee el cuerpo JSON en `Credentials`.

```rust
/// `POST /api/register` — create an account. 201 on success, 409 if the name is taken.
pub async fn register(
    State(pool): State<Pool>,
    Json(body): Json<Credentials>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let username = normalize(&body.username);
    let name_ok = (3..=150).contains(&username.len())
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !name_ok {
        return Err(ApiError::validation(
            "username: 3-150 characters, letters, digits, '_', '-' or '.'",
        )
        .with_field("username"));
    }
    // An upper limit too: hashing a huge password costs the server.
    if body.password.len() > 128 || !passwords::strength_score(&body.password).is_empty() {
        return Err(ApiError::validation(
            "password: 12-128 characters, with a digit or a symbol, not a common one",
        )
        .with_field("password"));
    }

    // argon2 is slow on purpose; the `_async` call keeps the server responsive.
    let password_hash = passwords::hash_async(&body.password)
        .await
        .map_err(server_error)?;

    let mut user = ApiUser {
        id: Auto::default(),
        username,
        password_hash,
        created_at: Auto::default(),
    };
    match user.save(&pool).await {
        Ok(_) => {}
        // The unique index on `username` decides, so two parallel sign-ups cannot both win.
        Err(ExecError::Driver(sqlx::Error::Database(db))) if db.is_unique_violation() => {
            return Err(ApiError::conflict("username already taken").with_field("username"));
        }
        Err(e) => return Err(server_error(e)),
    }

    let id = user.id.get().copied().unwrap_or_default();
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": id, "username": user.username })),
    ))
}

/// `POST /api/login` — check the password and hand out a short-lived token.
pub async fn login(
    State(pool): State<Pool>,
    ip: ClientIp,
    Json(body): Json<Credentials>,
) -> Result<Json<TokenResponse>, Response> {
    // First: with no valid key every login is a 500, right password or not.
    let secret = jwt_secret().map_err(IntoResponse::into_response)?;
    let username = normalize(&body.username);

    // Before any lookup: 429 once this IP (or everyone) failed too often.
    // `Sso(name)`: its own total limit, a per-IP limit shared with SSO sign-ins, no account lock.
    let scope = LoginScope::Sso("blog-api".to_owned());
    let attempt = login_throttle::shared()
        .begin(&scope, &ip, &username)
        .await
        .map_err(IntoResponse::into_response)?;

    let found = ApiUser::objects()
        .where_(ApiUser::username.eq(username))
        .first(&pool)
        .await
        .map_err(|e| server_error(e).into_response())?;

    let Some(user) = found else {
        // Do the same slow work as a real check, so timing does not reveal the name is unknown.
        if passwords::verify_dummy_async(&body.password).await.is_err() {
            return Err(LoginRefused::Busy.into_response()); // 503: no hashing slot free
        }
        attempt.failed().await;
        return Err(bad_credentials().into_response());
    };
    let password_ok = match passwords::verify_async(&body.password, &user.password_hash).await {
        Ok(ok) => ok,
        Err(PasswordError::Busy) => return Err(LoginRefused::Busy.into_response()),
        // A corrupt stored hash counts as a wrong password, not a server error.
        Err(_) => false,
    };
    if !password_ok {
        attempt.failed().await;
        return Err(bad_credentials().into_response());
    }
    attempt.succeeded().await;

    let id = user.id.get().copied().unwrap_or_default();
    let claims = Claims::new(id.to_string()).ttl(TOKEN_TTL); // subject = user id
    let access_token =
        jwt::encode(&claims, &secret).map_err(|e| server_error(e).into_response())?;
    Ok(Json(TokenResponse {
        access_token,
        token_type: "Bearer",
        expires_in: TOKEN_TTL.as_secs(),
    }))
}
```

Lo que hace `login`, en orden:

- `jwt_secret()` va primero. Sin un `JWT_SECRET` válido todo inicio de sesión responde `500`, así que una contraseña correcta y una incorrecta se ven igual.
- `login_throttle::shared().begin(...)` es la misma barrera que usan los inicios de sesión integrados. Cuenta los intentos fallidos por IP de cliente y en total, y responde `429 Too Many Requests` cuando hay demasiados. Informas de cada resultado con `attempt.failed()` o `attempt.succeeded()`; un éxito devuelve la cuenta. El scope `LoginScope::Sso("blog-api")` da a este login su propio límite total; su límite por IP se comparte con los inicios de sesión SSO del framework, y no tiene bloqueo de cuenta.
- Un nombre de usuario desconocido y una contraseña incorrecta reciben el mismo `401` con el mismo mensaje. Si no, un atacante podría probar nombres y saber qué cuentas existen.
- `verify_dummy_async` hace una comprobación completa contra un hash falso cuando el usuario no existe, así que ambos casos tardan más o menos lo mismo.
- `Claims::new(id)` pone el id del usuario en el token como su *subject*. `.ttl(TOKEN_TTL)` hace que caduque a los 15 minutos.

**Parte 3: el guardián y la regla de los posts.**

```rust
/// The user behind an `Authorization: Bearer <token>` header, or a 401.
pub async fn authenticate(pool: &Pool, parts: &Parts) -> Result<CurrentUser, ApiError> {
    let unauthorized = || ApiError::unauthorized("missing, invalid or expired token");

    let token = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim())
        .ok_or_else(unauthorized)?;
    // `decode` checks the signature and the expiry time.
    let claims = jwt::decode(token, &jwt_secret()?).map_err(|_| unauthorized())?;
    let id: i64 = claims
        .subject()
        .and_then(|sub| sub.parse().ok())
        .ok_or_else(unauthorized)?;

    // A deleted account's tokens stop working at once.
    let user = ApiUser::objects()
        .where_(ApiUser::id.eq(id))
        .first(pool)
        .await
        .map_err(server_error)?
        .ok_or_else(unauthorized)?;
    Ok(CurrentUser {
        id,
        username: user.username,
    })
}

/// Middleware: GET, HEAD and OPTIONS pass, every other method needs a valid token.
pub async fn require_user_for_writes(
    State(pool): State<Pool>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if is_read(request.method()) {
        return Ok(next.run(request).await);
    }
    let (mut parts, body) = request.into_parts();
    let user = authenticate(&pool, &parts).await?;
    parts.extensions.insert(user);
    Ok(next.run(Request::from_parts(parts, body)).await)
}

/// ViewSet rule: a new post's author is the signed-in user, and only the
/// author may change or delete it. Reads see every post.
pub struct AuthorFromToken;

impl ViewSetFilter for AuthorFromToken {
    fn filter(&self, _: &HashMap<String, String>, _: &'static ModelSchema) -> Vec<WhereExpr> {
        Vec::new()
    }

    fn filter_with(
        &self,
        parts: &Parts,
        _: &HashMap<String, String>,
        schema: &'static ModelSchema,
    ) -> Vec<WhereExpr> {
        if is_read(&parts.method) {
            return Vec::new();
        }
        match parts.extensions.get::<CurrentUser>() {
            Some(user) => vec![Post::author_id.eq(user.id).into()],
            // No user: match no row. An empty Vec would mean "every row".
            None => vec![match_nothing(schema)],
        }
    }

    fn write_pins(&self, parts: &Parts, _: &'static ModelSchema) -> Vec<WritePin> {
        match parts.extensions.get::<CurrentUser>() {
            // Whatever `author_id` the body sends, this value is stored.
            Some(user) => vec![WritePin::field("author_id", user.id)],
            None => vec![WritePin::Deny],
        }
    }
}
```

- `require_user_for_writes` es el middleware. Las peticiones `GET`, `HEAD` y `OPTIONS` pasan (`is_read`). Para cualquier otro método llama a `authenticate`, que responde `401` si falta la cabecera, si el token se ha cambiado o ha caducado, o si el usuario ya no existe. Si todo va bien, pone un `CurrentUser` en la petición, donde el código siguiente puede leerlo.
- `AuthorFromToken` es un *filter backend* del ViewSet: puede limitar qué filas ve una petición y fijar valores al escribir. `write_pins` guarda el id del usuario con sesión en `author_id` al crear, diga lo que diga el cuerpo, y una actualización nunca puede cambiarlo. Sin el pin, un cliente podría poner a otro usuario como autor; el pin es lo que lo impide. `filter_with` deja que un usuario cambie o borre solo sus propios posts; el post de otra persona responde `404`.

### 14d. Montar las rutas

Añade las rutas de inicio de sesión a `src/blog/urls.rs`. Necesitan el pool de la base de datos, así que tienen su propia función junto a `api()`. El archivo ya tiene `use axum::Router;`; añade arriba los demás imports:

```rust
use std::time::Duration;

use axum::routing::post;
use rustango::rate_limit::{RateLimitLayer, RateLimitRouterExt};
use rustango::sql::Pool;

use super::auth;

/// `/register` and `/login` (Step 14). They need the pool, so `main.rs` mounts them.
pub fn auth_routes(pool: Pool) -> Router {
    // Each sign-up costs a password hash: 10 a minute per client IP.
    let register = Router::new()
        .route("/register", post(auth::register))
        .rate_limit(RateLimitLayer::per_ip(10, Duration::from_secs(60)));
    Router::new()
        .route("/login", post(auth::login)) // throttled inside the handler
        .merge(register)
        .with_state(pool)
}
```

Después añade a `src/urls.rs` una función que reúne todo lo que va bajo `/api`:

```rust
/// Everything `main.rs` mounts under `/api`: the posts and the sign-in routes (Step 14).
pub fn rest_api(pool: Pool) -> Router {
    crate::post_view_set::PostViewSet::router("/posts", pool.clone())
        .merge(crate::blog::urls::auth_routes(pool))
}
```

En `src/main.rs`, sustituye la línea `/api` del Paso 12b por:

```rust
.nest_with("/api", urls::rest_api)
```

### 14e. Proteger las escrituras de posts

Sustituye `src/post_view_set.rs` por la versión de abajo. Los atributos de `#[derive(ViewSet)]` no tienen opción para un filter backend, así que este archivo usa las mismas opciones a través del builder `ViewSet`. `PostViewSet::router(prefix, pool)` conserva su nombre, así que el resto de tu código no cambia.

```rust
use axum::middleware::from_fn_with_state;
use axum::Router;
use rustango::core::Model as _;
use rustango::sql::Pool;
use rustango::viewset::ViewSet;

use crate::blog::auth::{require_user_for_writes, AuthorFromToken};
use crate::blog::models::Post;
use crate::post_serializer::PostSerializer;

/// The Post API: anyone can read, writes need a Bearer token (Step 14).
pub struct PostViewSet;

impl PostViewSet {
    pub fn router(prefix: &str, pool: impl Into<Pool>) -> Router {
        let pool = pool.into();
        ViewSet::for_model(
            Post::SCHEMA,
            &["id", "title", "body", "author_id", "published_at"],
        )
        .serializer::<PostSerializer>()
        .filter_fields(&["author_id", "status"])
        .search_fields(&["title", "body"])
        .ordering(&[("published_at", true)])
        .page_size(20)
        .filter_backend(AuthorFromToken)
        // Writes pass the framework's gate because the layer below guards them.
        .allow_anonymous()
        .router(prefix, pool.clone())
        .layer(from_fn_with_state(pool, require_user_for_writes))
    }
}
```

`allow_anonymous()` le dice al framework que las escrituras no están abiertas por error: tu propio middleware, añadido con `.layer(...)`, comprueba cada escritura antes de que el ViewSet la vea. Las dos líneas importan: sin `allow_anonymous()` el router se niega a montar, y sin `.layer(...)` cualquiera podría escribir.

El serializer del Paso 13 sigue listando `author_id`, así que las respuestas lo muestran. Un cliente aún puede enviarlo, pero el pin sustituye el valor.

### 14f. Probarlo con curl

Arranca el servidor con `cargo run` y, en otra terminal:

```bash
# 1. Create an account: 201
curl -i -X POST http://localhost:8080/api/register \
     -H "content-type: application/json" \
     -d '{"username":"alice","password":"correct-Horse-battery-9"}'

# 2. Log in: 200 with {"access_token":"eyJ...","token_type":"Bearer","expires_in":900}
curl -X POST http://localhost:8080/api/login \
     -H "content-type: application/json" \
     -d '{"username":"alice","password":"correct-Horse-battery-9"}'

# 3. Keep the token in a shell variable (paste your own value)
TOKEN=eyJ...

# 4. Write a post with the token: 201, and "author_id" is alice's id
curl -X POST http://localhost:8080/api/posts \
     -H "authorization: Bearer $TOKEN" \
     -H "content-type: application/json" \
     -d '{"title":"Hello","content":"My first signed-in post"}'

# 5. The same request without the token: 401
curl -i -X POST http://localhost:8080/api/posts \
     -H "content-type: application/json" \
     -d '{"title":"Nope","content":"x"}'

# 6. Reading stays public: 200
curl http://localhost:8080/api/posts
```

Una contraseña incorrecta, o un nombre de usuario que no existe, da el mismo `401 {"error":"unauthorized","message":"invalid username or password",...}`. A los 15 minutos el token caduca y las escrituras vuelven a responder `401`: inicia sesión otra vez.

> **Buenas prácticas desde el principio**
>
> - **Secreto desde el entorno.** `JWT_SECRET` viene de `.env` o del entorno del servidor, nunca del código ni de git. Usa 32 bytes aleatorios o más (`openssl rand -base64 32`) y una clave distinta por entorno.
> - **Tokens de vida corta.** 15 minutos limitan el daño de un token robado. No lo subas a días para ahorrar un inicio de sesión.
> - **Nunca registres secretos.** No registres ni imprimas contraseñas, tokens ni la cabecera `Authorization`, y nunca pongas un token en una URL. El `AccessLogLayer` del Paso 15 no registra cabeceras ni cuerpos y oculta los valores de la query.
> - **Errores de login genéricos.** El mismo `401` para un usuario desconocido y una contraseña incorrecta, como arriba.
> - **Limita los intentos de login.** Mantén la barrera `login_throttle` en `login` y el límite de tasa en `/register`. Detrás de un proxy, configura los proxies de confianza (Paso 18), o todos los clientes compartirán la IP del proxy.
> - **HTTPS en producción.** Un token enviado por HTTP plano puede leerse por el camino. Termina HTTPS en tu proxy (Paso 18).
> - **Hashes solo en el servidor, solo con argon2.** Usa `passwords::hash_async` / `verify_async`; nunca guardes una contraseña ni un hash rápido como SHA-256.
> - **Nada de escrituras abiertas.** Cada ruta de escritura necesita una comprobación, como la capa de 14e. Un ViewSet con `allow_anonymous` y sin capa deja que cualquiera cambie tus datos.

### 14g. Siguiente paso: tokens de refresco

`rustango::jwt` emite tokens sueltos y sin estado: cuando uno caduca, el usuario vuelve a iniciar sesión, y no puedes anular un token antes de que caduque. Cuando necesites sesiones más largas o un cierre de sesión que actúe al momento, pasa al patrón completo: tokens de **acceso** de vida corta, un token de **refresco** de vida larga en una cookie HttpOnly, rotación y una lista negra de JTI para revocar. Activa la feature `tenancy` y usa `rustango::tenancy::jwt_lifecycle::JwtLifecycle`, cuyos métodos `issue_pair_with` / `verify_access` / `refresh` gestionan el par por ti.

---

## Paso 15: Añadir middleware de seguridad

El middleware envuelve cada petición para añadir comportamiento transversal. Este va en `src/main.rs`: sustituye la línea `nest_with` de `/api` del Paso 14d, para que las capas envuelvan cada ruta bajo `/api`. Aquí apilas IDs de petición, registro de acceso, límite de tasa, CORS y cabeceras de seguridad en una cadena. Cada `.method(...)` añade una capa. Consulta la [guía de Middleware](middleware.md) para el catálogo completo de capas y las reglas de ordenamiento.

```rust
use rustango::security_headers::{SecurityHeadersLayer, SecurityHeadersRouterExt, CspBuilder};
use rustango::cors::{CorsLayer, CorsRouterExt};
use rustango::rate_limit::{RateLimitLayer, RateLimitRouterExt};
use rustango::access_log::{AccessLogLayer, AccessLogRouterExt};
use rustango::request_id::{RequestIdLayer, RequestIdRouterExt};
use std::time::Duration;

.nest_with("/api", |pool| urls::rest_api(pool)
    .request_id(RequestIdLayer::default())
    .access_log(AccessLogLayer::default())                      // PII-redacted
    .rate_limit(RateLimitLayer::per_ip(60, Duration::from_secs(60)))
    .cors(CorsLayer::new()
        .allow_origins(vec!["https://app.example.com"])
        .allow_methods(vec!["GET", "POST", "PUT", "PATCH", "DELETE"]))
    .security_headers(
        SecurityHeadersLayer::strict()
            .csp(CspBuilder::strict_starter().build()),
    ))
```

El resto de la cadena del `Cli` queda igual. Cada petición a `/api` fluye ahora a través de la pila de middleware completa. `.with_welcome()` no hace nada aquí: el andamiaje ya enruta `/`, así que la página de bienvenida se omite con un aviso.

---

## Paso 16: Escribir pruebas

**Rustango** incluye un cliente de pruebas que dirige tu router en el mismo proceso, para que puedas hacer aserciones sobre respuestas HTTP reales sin arrancar un servidor y sin tocar la red. Genera el andamiaje de un archivo de pruebas:

```bash
cargo run -- make:test PostSmoke      # generates tests/post_smoke.rs
```

Los generadores `make:*` toman un nombre en PascalCase; `PostSmoke` se convierte en el archivo en snake_case `tests/post_smoke.rs`.

Edita `tests/post_smoke.rs`. Las pruebas de integración viven en una crate separada, así que construyen ellas mismas el router bajo prueba, con el mismo `urls::rest_api` que `main.rs` monta en el Paso 14d:

```rust
use myblog::urls;
use rustango::sql::Pool;
use rustango::test_client::TestClient;
use serde_json::json;

async fn app() -> axum::Router {
    // A test in `tests/` is a separate crate and never runs `main`, so
    // nothing has loaded `.env` for it. Without this line `DATABASE_URL`
    // is unset and both tests panic before reaching the database.
    let _ = dotenvy::dotenv();

    let pool = Pool::connect(&std::env::var("DATABASE_URL").unwrap()).await.unwrap();
    axum::Router::new().nest("/api", urls::rest_api(pool))
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
async fn create_post_without_a_token_returns_401() {
    let client = TestClient::new(app().await);
    let response = client.post("/api/posts")
        // The serializer's field names: `content` is `Post.body` renamed.
        .json(&json!({ "title": "Test", "content": "x" }))
        .send().await;
    assert_eq!(response.status, 401);
}
```

Dos cosas de este fragmento son fáciles de hacer mal:

- **`dotenvy::dotenv()`** — si lo omites fallan *ambas* pruebas, antes de hacer ninguna petición.
- **`content`, no `body`** — el serializer acepta los dos aquí, porque `source = "body"` mantiene el nombre del modelo en la entrada, pero `content` es el nombre que tu API publica de verdad.

La segunda prueba demuestra que el guardián del Paso 14 está en su sitio. Para probar una escritura con sesión, regístrate e inicia sesión primero con el mismo cliente y envía el token en una cabecera `authorization`. El [`tests/auth_flow.rs`](https://github.com/ujeenet/rustango/blob/develop/crates/rustango/examples/getting_started_blog/tests/auth_flow.rs) del proyecto de ejemplo lo hace para cada caso del Paso 14: registro, nombre repetido, inicio de sesión, contraseña incorrecta, usuario desconocido, tokens ausentes, alterados y caducados, y el autor tomado del token.

`Pool` es el pool multi-backend, y `router` acepta el pool de cualquier backend, así que este archivo compila sin cambios en PostgreSQL, MySQL y SQLite.

> **Atención:** las pruebas de integración en `tests/` solo pueden hacer `use myblog::…` para módulos declarados `pub mod` en `src/lib.rs`. Un andamiaje nuevo tiene ese target de librería; asegúrate de que `pub mod post_view_set;` y `pub mod urls;` están ahí. Un proyecto de un `cargo-rustango` antiguo puede ser solo binario: añade un `src/lib.rs` con las líneas `pub mod …;` que necesites, o construye el router en línea dentro de la prueba, tal como `make:test` genera su `app()`.

Ejecuta las pruebas:

```bash
cargo test --test post_smoke
```

---

## Paso 17: Ejecutar la comprobación del sistema

Antes de desplegar, ejecuta el comprobador integrado. Señala malas configuraciones comunes (como un `RUSTANGO_SESSION_SECRET` débil o una base de datos inalcanzable) antes de que aparezcan en producción.

```bash
cargo run -- check --deploy
```

`--deploy` trata un `RUSTANGO_SESSION_SECRET` ausente como un error, así que pon uno primero (Paso 3), incluso en local. En tu entorno de desarrollo local verás entonces algo como:

```
running rustango system check (deploy mode)...
  [info]    6 models registered via inventory
  [info]    database reachable
  [info]    1 migration(s) on disk
  [info]    RUSTANGO_SESSION_SECRET decodes to a valid ≥32-byte key
  [info]    config tier resolved to `dev`
  [warning] RUSTANGO_ENV is unset — set to `prod` so config loaders pick the right tier
  [warning] DATABASE_URL points at localhost / 127.0.0.1 — verify this is intended in production (…)
```

(Los recuentos exactos de modelos/migraciones y las notas extra dependen de tu proyecto y sus features.) Esas dos advertencias son las esperadas para el entorno de desarrollo. En una configuración de producción — `RUSTANGO_ENV=prod` y una `DATABASE_URL` de base de datos gestionada — desaparecen y verás `all checks passed`. Corrige cualquier advertencia o error restante antes de hacer push a producción.

---

## Paso 18: Desplegar a producción

Cómo despliegas depende de tu plataforma (Fly, Railway, Kubernetes, ECS a secas, etc.). Los pasos del lado del framework son los mismos en todas partes; el flag `--release` compila un binario optimizado:

```bash
# 1. Set production env
export RUSTANGO_ENV=prod
export DATABASE_URL=postgres://prod-host/myblog
export RUSTANGO_SESSION_SECRET=$(openssl rand -base64 32)

# 2. Run migrations (ship `migrations/` AND `system/migrations/` with the binary)
cargo run --release -- migrate

# 3. Audit
cargo run --release -- check --deploy

# 4. Build binary
cargo build --release

# 5. Run with a process supervisor (systemd / docker / k8s)
./target/release/myblog
```

En PowerShell:

```powershell
$env:RUSTANGO_ENV = "prod"
$env:DATABASE_URL = "postgres://prod-host/myblog"
$bytes = New-Object byte[] 32
[Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
$env:RUSTANGO_SESSION_SECRET = [Convert]::ToBase64String($bytes)
cargo run --release -- migrate
```

> **Revisa primero tus rutas de escritura.** ¿Hay algún ViewSet o vista todavía con `allow_anonymous` y sin capa de auth delante? Entonces cualquiera en internet puede cambiar esos datos. Hazlo `read_only` o protégelo como en el [Paso 14e](#14e-proteger-las-escrituras-de-posts).

Asegúrate de que tu proxy inverso:
- Termine HTTPS
- Reenvíe `X-Forwarded-For`, y la app monte `RealIpLayer::trust_proxies([...])` con ese proxy (`Cli::with_trusted_proxies([...])?` o `server::Builder::real_ip`), para obtener IPs precisas en `AccessLogLayer` y en los throttles (vea [security.md](security.md))
- Pase la cabecera `Host` original sin cambios: el enrutado de tenants, la detección del apex y `AllowedHostsLayer` solo leen `Host`
- Reenvíe `X-Forwarded-Proto` (y `X-Forwarded-Host`, que solo fija la URL de redirección SSO detrás de un proxy de confianza)

`Cli::run()` ya rellena `ConnectInfo` para el límite de tasa y el filtrado de IP. Si sirves el router tú mismo, usa `axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())`.

---

## Adónde ir a continuación

| Tema | Doc |
|---|---|
| Versión ejecutable de esta guía | [`examples/getting_started_blog`](https://github.com/ujeenet/rustango/tree/develop/crates/rustango/examples/getting_started_blog) |
| Tareas habituales (registro, subida de archivos, correo, tareas, despliegue) | [`docs/common-tasks.md`](common-tasks.md) |
| Cada subcomando de `manage` | [`docs/manage.md`](manage.md) |
| Recetario del ORM (filtros avanzados, agregaciones, M2M, soft delete) | [`docs/orm.md`](orm.md) |
| Middleware (el catálogo completo de capas + ordenamiento) | [`docs/middleware.md`](middleware.md) |
| Benchmarks de rendimiento (vs. Go) | [`docs/benchmarks.md`](benchmarks.md) |
| Convenciones de la API (nomenclatura, patrones builder, feature gates) | [`docs/api-conventions.md`](api-conventions.md) |
| Funciones de seguridad en profundidad | [`docs/security.md`](security.md) |
| Multi-tenancy | [`docs/tenancy.md`](tenancy.md) |
| Documentación de la API | <https://docs.rs/rustango> |

Si te topas con algo que no funciona o no queda claro, abre un issue.
