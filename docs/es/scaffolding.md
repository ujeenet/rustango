# Andamiaje

**Rustango** tiene dos capas de generación de código, ambas inspiradas en los generadores que conoces de Django y Laravel — así rara vez conectas boilerplate a mano:

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

Eso coloca un binario `cargo-rustango` en tu `PATH`; Cargo lo expone entonces como `cargo rustango` (del mismo modo que `django-admin` o el instalador `laravel` te dan un comando global).

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
| `tenancy` | Multi-tenancy: registry de tenants, bases por tenant, consola de operador |
| `csrf` | Middleware de protección CSRF para POST de formularios |
| `sso` / `admin-sso` | Inicio de sesión único OIDC, para usuarios de la app / para el sitio de admin |
| `passkey` | Autenticación WebAuthn / passkey |
| `cache-redis` / `cache-page` | Backend de caché Redis / caché de páginas completas |
| `email-smtp` | Transporte SMTP para el framework de email |
| `mcp` | Servidor Model Context Protocol para agentes de IA |
| `testkit` / `test_utils` | Constructores de esquema, factorías y constructores solo para tests |

`cargo rustango new --help` imprime esta lista. Los backends **no** son válidos aquí — usa `--backend`. Nombrar uno como funcionalidad se rechaza, porque fijaría el backend del framework mientras la funcionalidad propia del proyecto quedaría apagada, y `#[derive(Model)]` alinea lo que emite con las funcionalidades del proyecto.

---

## Qué se genera

Cada plantilla escribe un proyecto de Cargo autocontenido:

```text
<name>/
  Cargo.toml            # the rustango dependency + features for this template
  .env.example          # copy to .env (DATABASE_URL, RUSTANGO_SESSION_SECRET, …)
  .gitignore
  rust-toolchain.toml   # pins the Rust toolchain
  docker-compose.yml    # a Postgres service to develop against
  Dockerfile            # production image
  README.md
  config/
    default.toml        # settings shared across every environment
    dev_settings.toml   # per-tier overrides …
    staging_settings.toml
    prod_settings.toml
  migrations/           # JSON migration files (committed to git)
  src/
    main.rs             # the single binary — HTTP server + every manage verb
    models.rs           # your #[derive(Model)] structs
    views.rs            # request handlers ("views")
    urls.rs             # pub fn api() -> Router that aggregates your routes
```

### Un binario para todo

`src/main.rs` es el único punto de entrada. Arranca el servidor HTTP **y** despacha cada verbo de `manage` — no hay un `manage.py` ni un `src/bin/manage.rs` aparte:

```rust
mod models;
mod urls;
mod views;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    rustango::manage::Cli::new()
        .api(urls::api())
        .with_welcome()  // friendly `/` page until you add a root handler
        .with_health()   // /health + /ready endpoints (fullstack & tenant)
        .run()
        .await
}
```

Así que `cargo run` arranca el servidor, y `cargo run -- <verb>` ejecuta migraciones, generadores y lo demás.

Cómo difieren las plantillas dentro de `main.rs` / `urls.rs`:

- **api** — sin admin; `urls::api()` simplemente agrega tus propias rutas.
- **fullstack** — `urls.rs` también expone `admin_router(pool)` (construido a partir de `admin::Builder::new(pool).build()`) para que el auto-admin se monte en `/admin`.
- **tenant** — `main.rs` añade `.tenancy()`, sirviendo la consola de operador en el dominio ápice y cada tenant bajo su propio subdominio. Las propias tablas del framework se generan en una carpeta **`system/migrations/`** a partir de los modelos compilados (al estilo de Django) en el primer `cargo run -- migrate` — sin JSON de bootstrap entregado a mano, así que la primerísima migración funciona sin configuración adicional.

### Configuración por capas

Los ajustes cargan primero `config/default.toml`, luego `config/<RUSTANGO_ENV>_settings.toml` encima. `RUSTANGO_ENV` es `dev` por defecto, así que un `cargo run` recién generado funciona sin ediciones; establece `RUSTANGO_ENV=prod` en producción para recoger `prod_settings.toml`.

### Primera ejecución

```sh
cd <name>
cp .env.example .env
docker compose up -d        # start Postgres
cargo run -- migrate        # apply migrations
cargo run                   # serve
cargo run -- --help         # see every manage verb
```

---

## Añadir un módulo de funcionalidad: `manage startapp`

Este es el `startapp` de Django — genera un módulo autocontenido de modelos, vistas y rutas relacionados:

```sh
cargo run -- startapp blog
```

Escribe `src/blog/` conteniendo `mod.rs`, `models.rs` (un modelo inicial nombrado según la forma singular de la app — `blog` → `Blog`), `views.rs`, `urls.rs` y `tests.rs`, luego declara el módulo en `src/main.rs` y fusiona sus rutas en `urls::api()`.

Opciones:

- **`--into <dir>`** — genera el andamiaje bajo un directorio base distinto de `src/` (p. ej. un miembro del workspace).
- **`--with-manage-bin`** — emite además un `bin/manage.rs` (para diseños que prefieren un binario de manage aparte).

---

## Generar archivos sueltos: los comandos `make:*`

Dentro de un proyecto, los verbos `make:*` generan el andamiaje de un archivo cada vez. La referencia completa por flag vive en la [referencia de la CLI de manage](manage.md); las formas comunes son:

| Comando | Genera | Comparable a |
|---|---|---|
| `make:viewset <Name> [--model <M>]` | Un ViewSet CRUD al estilo de DRF | `ViewSet` de DRF |
| `make:serializer <Name> [--model <M>]` | Un serializer para dar forma a request/response | serializer de DRF |
| `make:api_routes <app>` | Un agregador de rutas de API para una app | — |
| `make:form <Name>` | Un formulario HTML con validación | `Form` de Django |
| `make:job <Name>` | Un handler de job en segundo plano | job de Laravel / Celery |
| `make:notification <Name>` | Una notificación multicanal | notificación de Laravel |
| `make:middleware <Name>` | Un esqueleto de middleware | middleware de Django / Laravel |
| `make:test <Name>` | Un módulo de pruebas usando el cliente de pruebas en el mismo proceso | — |

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
