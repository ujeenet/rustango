# Andamiaje

**Rustango** tiene dos capas de generación de código — así rara vez conectas boilerplate a mano:

1. **El generador de proyectos** — `cargo rustango new` crea un proyecto entero nuevo a partir de una plantilla.
2. **Generadores dentro del proyecto** — `manage startapp` y la familia `manage make:*` añaden apps, vistas, serializers, jobs y más dentro de un proyecto existente.

[![`cargo rustango new` genera un proyecto completo y listo para ejecutar — manifiesto de Cargo, niveles de configuración, Docker, migraciones y src — en un solo comando](../img/scaffolding.png)](../img/scaffolding.png)

## Tabla de contenidos

- [Instalar el generador](#instalar-el-generador)
- [Crear un proyecto: `cargo rustango new`](#crear-un-proyecto-cargo-rustango-new)
- [Qué se genera](#qué-se-genera)
- [Añadir un módulo de funcionalidad: `manage startapp`](#añadir-un-módulo-de-funcionalidad-manage-startapp)
- [Generar archivos sueltos: los comandos `make:*`](#generar-archivos-sueltos-los-comandos-make)
- [Un flujo típico](#un-flujo-típico)

---

## Instalar el generador

`cargo rustango` es un subcomando de Cargo. Instálalo una vez, de forma global:

```sh
cargo install cargo-rustango
```

Eso coloca un binario `cargo-rustango` en tu `PATH`; Cargo lo expone entonces como `cargo rustango` — un comando global disponible en cualquier parte, incluso fuera de un proyecto. La misma instalación añade `cargo-rgo`, así que `cargo rgo …` es una forma más corta de escribir cualquier comando `cargo rustango …`.

### La versión del propio generador es la que obtiene tu proyecto

`cargo rustango new` escribe `rustango = "MAJOR.MINOR"` en el `Cargo.toml` generado, tomada de **la versión del generador**, no de lo más reciente que haya en crates.io. Sea cual sea el generador que instales, esa es la versión que fija tu proyecto.

Eso es casi siempre lo que quieres, y por eso el comando de instalación de arriba no lleva versión fijada: el generador más nuevo escribe la fijación más nueva, y los dos no pueden separarse.

Conviene saberlo cuando quieres deliberadamente una versión anterior — para que coincida con un proyecto que ya está en ella, o para reproducir un reporte. Fija el generador, no el proyecto:

```sh
cargo install cargo-rustango --version 0.57.0
```

Comprueba cuál tienes con `cargo rustango --version`. Actualizarlo más adelante es el mismo comando con `--force`, y solo afecta a los proyectos que generes después — la fijación de un proyecto existente es una línea en su propio `Cargo.toml`, que editas tú.

---

## Crear un proyecto: `cargo rustango new`

```sh
cargo rustango new <name> [--template api|fullstack|tenant]
                          [--backend postgres|sqlite|mysql]
                          [--features <lista>]
```

- **`<name>`** — el nombre del proyecto (y de la crate). Debe ser un nombre de crate de Cargo válido (`[A-Za-z_][A-Za-z0-9_-]*`), y el directorio de destino no debe existir ya.
- **`--template` / `-t`** — qué plantilla inicial generar (por defecto: **fullstack**).
- **`--backend` / `-b`** — sobre qué base de datos corre el proyecto (por defecto: **postgres**).
- **`--features` / `-F`** — funcionalidades extra de **Rustango**, separadas por comas o espacios.
- **`--alias rgo`** — llamar a la dependencia `rgo` en lugar de `rustango` (ver [Un nombre más corto](#un-nombre-más-corto-rgo)).
- **`--interactive` / `-i`** — elegir desde menús en su lugar. Un `cargo rustango new` a secas en un terminal hace lo mismo.
- **`--help` / `-h`**, **`--version`** — uso y versión.

Ejecutado sin argumentos, pregunta:

```text
  rustango — new project
  (enter accepts the default; ctrl-c aborts)

  Project name: shop

  Template
    1) fullstack  ORM + auto-admin + forms — the usual starting point  (default)
    2) api        bare ORM + axum, no admin UI — for JSON-only services
    3) tenant     multi-tenancy: tenant registry + operator console
  > 3

  Database
    1) postgres  every feature, including schema-mode tenancy  (default)
    2) sqlite    a file beside the project — no server to run
    3) mysql     MySQL 8.0+ / MariaDB
  > 1

  Extra features  (numbers, e.g. `1 3 4` — enter for none)
     1) csrf         CSRF protection middleware for form POSTs
     2) sso          OIDC single sign-on for application users
     …
  > 1 2

  Same thing without the wizard:
    cargo rustango new shop --template tenant --backend postgres --features csrf,sso

  Create it? [Y/n]
```

El asistente fija exactamente los campos que fijan los flags e imprime la línea de comandos equivalente antes de escribir nada — usarlo una vez te enseña los flags, y hay un solo camino de código que decide qué contiene un proyecto. Sin terminal (un script, un job de CI) falla con un mensaje en lugar de esperar una respuesta que nadie va a dar.

### Las tres plantillas

Cada una se corresponde con una de las tres formas de app de **Rustango**:

| Plantilla | Lo que obtienes | Recurre a ella cuando |
|---|---|---|
| `api` | ORM + Axum a secas, **sin admin** | Servicios solo-JSON y microservicios |
| `fullstack` *(por defecto)* | ORM + el **auto-admin** | Una app web típica con back-office |
| `tenant` | Multi-tenancy + consola de operador + apps por tenant | SaaS que aloja muchos tenants aislados |

```sh
cargo rustango new myblog                      # fullstack (the default)
cargo rustango new api_demo  --template api
cargo rustango new shop      --template tenant
```

### Elegir la base de datos: `--backend`

```sh
cargo rustango new edge --backend sqlite
cargo rustango new shop --backend mysql
```

`--backend` decide qué usa `cargo run`, y da forma a todo el proyecto en consecuencia — la `DATABASE_URL` de `.env.example`, los servicios de `docker-compose.yml`, la `url` de cada nivel de configuración y las instrucciones de arranque del README. Elige `sqlite` y no hay servicio de base de datos alguno: es un fichero junto al proyecto, creado por el primer `cargo run -- migrate`.

Los tres backends siguen cableados en los `[features]` generados, así que los otros dos quedan a un flag de distancia:

```sh
cargo run --no-default-features --features sqlite
```

### Encender más del framework: `--features`

```sh
cargo rustango new saas --template tenant --features csrf,sso,cache-redis
```

Una plantilla enciende un conjunto razonable; `--features` añade las opciones que ninguna de ellas alcanza:

| Funcionalidad | Qué añade |
|---|---|
| `csrf` | Middleware de protección CSRF para POST de formularios |
| `sso` / `admin-sso` | Inicio de sesión único OIDC, para usuarios de la app / para el sitio de admin |
| `passkey` | Autenticación WebAuthn / passkey |
| `cache-redis` / `cache-page` | Backend de caché Redis / caché de páginas completas |
| `jobs` / `jobs-postgres` | Cola de trabajos en segundo plano, en proceso / respaldada por la base de datos y por tanto resistente a reinicios |
| `scheduler` | Tareas en segundo plano a intervalos fijos |
| `email-smtp` | Transporte SMTP para el framework de email |
| `mcp` | Servidor Model Context Protocol para agentes de IA |
| `http2` | Servir HTTP/2 sin cifrar (h2c) junto a HTTP/1 |
| `testkit` / `test_utils` | Constructores de esquema, factorías y constructores solo para tests |

`--features tenancy` solo elige `--template tenant`; con otra plantilla se rechaza, porque solo el `main.rs` de la plantilla tenant sirve la multi-tenancy. `cargo rustango new --help` imprime esta lista. Los backends **no** son válidos aquí — usa `--backend`. Nombrar uno como funcionalidad se rechaza, porque fijaría el backend del framework mientras la funcionalidad propia del proyecto quedaría apagada, y `#[derive(Model)]` alinea lo que emite con las funcionalidades del proyecto.

### Un nombre más corto: `rgo`

```sh
cargo rgo new shop --alias rgo
```

`--alias rgo` declara la dependencia con el nombre `rgo`, y el código generado usa rutas `rgo::` y atributos `#[rgo(...)]`. La crate `rgo` de crates.io es de otra persona, así que esto es un renombrado del paquete `rustango`:

```toml
[dependencies]
rgo = { package = "rustango", version = "0.61", default-features = false, features = ["batteries"] }

[features]
postgres = ["rgo/postgres"]
```

```rust
use rgo::prelude::*;

#[derive(Model, Debug, Clone)]
#[rgo(table = "item", display = "name")]
pub struct Item {
    #[rgo(primary_key)]
    pub id: Auto<i64>,
    #[rgo(max_length = 64)]
    pub name: String,
}
```

`#[rgo(...)]` significa exactamente lo mismo que `#[rustango(...)]`, llames como llames a la dependencia. Un struct o campo usa un solo nombre: mezclar `#[rustango(...)]` y `#[rgo(...)]` en el mismo elemento es un error de compilación. `manage startapp` y los generadores `make:*` leen el Cargo.toml y también escriben rutas `rgo::` y `#[rgo(...)]`.

---

## Qué se genera

Cada plantilla escribe un proyecto de Cargo autocontenido:

```text
<name>/
  Cargo.toml            # the rustango dependency + features for this template
  .env.example          # copy to .env (DATABASE_URL, RUSTANGO_SESSION_SECRET, …)
  .gitignore
  rust-toolchain.toml   # selects the `stable` toolchain + rustfmt/clippy/rust-analyzer
  docker-compose.yml    # the database service (none for sqlite) + a cargo-watch dev container
  Dockerfile            # production image (multi-stage, release, non-root)
  Dockerfile.dev        # the dev image docker-compose.yml builds
  .dockerignore
  README.md
  config/
    default.toml        # settings shared across every environment
    dev_settings.toml   # per-tier overrides …
    staging_settings.toml
    prod_settings.toml
  migrations/           # JSON migration files (committed to git)
  system/migrations/    # the framework's own tables, generated on first migrate (commit them)
  src/
    lib.rs              # the app as a library: `pub mod models; pub mod urls; pub mod views;`
    main.rs             # the single binary — HTTP server + every manage verb
    models.rs           # your #[derive(Model)] structs
    views.rs            # request handlers ("views")
    urls.rs             # pub fn api() -> Router that aggregates your routes
```

### Un binario para todo

`src/main.rs` es el único punto de entrada. Arranca el servidor HTTP **y** despacha cada verbo de `manage` — no hay un script de manage ni un `src/bin/manage.rs` aparte. Los módulos viven en el target de biblioteca (`src/lib.rs`), así que `main.rs` llega a ellos por el nombre del crate, y también un binario worker bajo `src/bin/`. El `main.rs` de la plantilla api, recortado:

```rust
use myapp::urls;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    rustango::manage::Cli::new()
        .api(urls::api())
        .with_health()   // /health + /ready endpoints (every template)
        .run()
        .await
}
```

Así que `cargo run` arranca el servidor, y `cargo run -- <verb>` ejecuta migraciones, generadores y lo demás.

Cómo difieren las plantillas dentro de `main.rs` / `urls.rs`:

- **api** — sin admin; `urls::api()` simplemente agrega tus propias rutas.
- **fullstack** — el mismo `urls.rs`, más `admin_prefix(&settings)` y un ayudante `admin_router(pool, &settings)` que `main.rs` monta con `Cli::nest_with(urls::admin_prefix(&settings), …)`, construido a partir del pool del servidor. El prefijo es `[admin] url_prefix` (o `[routes] admin_url`), si no `/admin`. `main.rs` carga los ajustes una vez y los pasa al `Cli` con `with_loaded_settings`. Recibe un `rustango::sql::Pool`, así que no nombra ningún driver, y queda detrás de un inicio de sesión: crea la cuenta con `cargo run -- create-admin <username> --superuser` (consulta [Primeros pasos, Paso 11](getting-started.md#paso-11-activar-el-auto-admin)).
- **tenant** — `main.rs` añade `.tenancy()`, sirviendo la consola de operador en el dominio ápice y cada tenant bajo su propio subdominio, y `.with_tenant_provisioning("migrations")` para las páginas **New tenant** y **Database servers** de la consola (bórralo para desactivarlas; guardar un servidor necesita `RUSTANGO_SECRET_KEY`). Las propias tablas del framework se generan en una carpeta **`system/migrations/`** a partir de los modelos compilados en el primer `cargo run -- migrate` — sin JSON de bootstrap entregado a mano, así que la primerísima migración funciona sin configuración adicional.

### Configuración por capas

Los ajustes cargan primero `config/default.toml`, luego `config/<RUSTANGO_ENV>_settings.toml` encima. `RUSTANGO_ENV` es `dev` por defecto, así que un `cargo run` recién generado funciona sin ediciones; establece `RUSTANGO_ENV=prod` en producción para recoger `prod_settings.toml`.

Los archivos de dev y staging definen un `[brand] tagline` de `dev` y `staging`. Para cambiarlo, o para poner tu propio nombre y logo, consulta [Marca](operator-console.md#marca) y las [claves de configuración](admin.md#configuración-admin-y-brand) del admin.

### Ajustes que no surten efecto por sí solos

No todas las claves las lee el framework. `Cli::with_settings` avisa al arrancar cuando se define una del primer grupo.

| Clave | Estado |
|---|---|
| `[sso]` (todas las claves) | Aún sin efecto. Configura el SSO del admin en la UI del admin. |
| `[auth.jwt] issuer`, `audience` | Aún sin efecto. |
| `[admin] primary_color`, `csrf_cookie_secure`, `session_timeout_minutes` | Aún sin efecto; solo las lee `check --deploy`. |
| `[database] url` | No se usa para conectar: cada pool lee la variable de entorno `DATABASE_URL`. |
| `[audit] retention_days` | Nada poda en base a ella; programa `audit-cleanup --days <N>`. |
| `[jobs]`, `[i18n]`, `[cache]`, `[mail]`, `[mcp]`, TTLs de `[auth.jwt]`, `[admin]` | Cableadas por el usuario: solo las lee el constructor al que llamas (`jobs::inmemory_from_settings`, `Translator::from_settings`, `cache::from_settings_async`, `email::from_settings`, `mcp::secure_tenant_router_from_settings`, `auth_routes::Config::with_jwt_settings`, `admin::Builder::from_settings`). |

### Primera ejecución

```sh
cd <name>
cp .env.example .env                     # then change `postgres` -> `localhost` in DATABASE_URL
docker compose up -d --no-deps postgres  # only the database: the rust service holds 8080
cargo run -- makemigrations              # generate the framework's (and your) tables
cargo run -- migrate                     # apply them
cargo run -- create-admin <username> --superuser  # fullstack: the admin login
cargo run                                # serve
cargo run -- --help                      # see every manage verb
```

---

## Añadir un módulo de funcionalidad: `manage startapp`

Genera un módulo autocontenido de modelos, vistas y rutas relacionados:

```sh
cargo run -- startapp blog
```

Escribe `src/blog/` conteniendo `mod.rs`, `models.rs` (un modelo inicial nombrado según la forma singular de la app — `blog` → `Blog`), `views.rs`, `urls.rs` y `tests.rs`, luego declara el módulo en `src/lib.rs` (`pub mod blog;`; `mod blog;` en `src/main.rs` en un proyecto sin `lib.rs`) y fusiona sus rutas en `urls::api()`.

Opciones:

- **`--into <dir>`** — genera el andamiaje bajo un directorio base distinto de `src/` (p. ej. un miembro del workspace).
- **`--with-manage-bin`** — emite además un `src/bin/manage.rs` (para diseños que prefieren un binario de manage aparte).

---

## Generar archivos sueltos: los comandos `make:*`

Dentro de un proyecto, los verbos `make:*` generan el andamiaje de un archivo cada vez. La referencia completa por flag vive en la [referencia de la CLI de manage](manage.md); las formas comunes son:

| Comando | Genera |
|---|---|
| `make:viewset <Name> [--model <M>]` | Un ViewSet CRUD |
| `make:serializer <Name> [--model <M>]` | Un serializer para dar forma a request/response |
| `make:api_routes <app>` | Un agregador de rutas de API para una app |
| `make:form <Name>` | Un formulario HTML con validación |
| `make:job <Name>` | Un handler de job en segundo plano |
| `make:scheduled <Name>` | Una tarea de intervalo fijo para `scheduler::Scheduler` |
| `make:worker <Name>` | Un binario worker bajo `src/bin/` que vacía la cola de jobs |
| `make:notification <Name>` | Una notificación multicanal |
| `make:middleware <Name>` | Un esqueleto de middleware |
| `make:test <Name>` | Un módulo de pruebas usando el cliente de pruebas en el mismo proceso |

```sh
cargo run -- make:viewset PostViewSet --model Post
cargo run -- make:serializer PostSerializer --model Post
cargo run -- make:test post_smoke
```

---

## Un flujo típico

```sh
cargo rustango new myblog                              # 1. scaffold the project
cd myblog
cargo run -- startapp blog                             # 2. add a feature module
# …add fields to src/blog/models.rs…
cargo run -- makemigrations                            # 3. generate a migration
cargo run -- migrate                                   # 4. apply it
cargo run -- make:viewset PostViewSet --model Post     # 5. expose a JSON API
cargo run                                              # 6. serve
```
