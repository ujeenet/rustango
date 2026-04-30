//! File body templates for `cargo rustango new`.
//!
//! Templates are plain `const &str` (or builder fns when they need
//! interpolation). Keeping them in-source means the binary has zero
//! runtime filesystem dependency and `cargo install cargo-rustango`
//! ships everything in one artifact. CI snapshot-tests the generated
//! output by running `cargo check` on each template.

use super::Template;

// ---------------- Cargo.toml ----------------

pub fn cargo_toml(name: &str, template: Template) -> String {
    let rustango_dep = template.rustango_features();
    format!(
        r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2021"
# `default-run` resolves the `cargo run` ambiguity that comes from
# shipping two binaries (the app and the manage CLI). Without it,
# bare `cargo run` errors with "could not determine which binary to
# run". Use `cargo run --bin manage -- <verb>` for the CLI.
default-run = "{name}"

# Empty `[workspace]` table makes this project standalone: if a parent
# directory has its own workspace `Cargo.toml`, cargo would otherwise
# refuse to build (see "current package believes it's in a workspace
# when it's not"). This declaration severs that link without taking on
# any workspace members. Delete it if you intentionally want the
# project to be a member of a parent workspace.
[workspace]

[dependencies]
rustango = {rustango_dep}
tokio = {{ version = "1", features = ["macros", "rt-multi-thread", "sync", "signal", "net"] }}
axum = {{ version = "0.8", default-features = false, features = ["tokio", "http1", "json", "form", "query"] }}
tower = {{ version = "0.5", features = ["util"] }}
serde = {{ version = "1", features = ["derive"] }}
serde_json = "1"
chrono = {{ version = "0.4", default-features = false, features = ["serde", "clock"] }}
tracing = "0.1"
tracing-subscriber = {{ version = "0.3", features = ["env-filter"] }}
dotenvy = "0.15"

[dev-dependencies]
tokio = {{ version = "1", features = ["macros", "rt-multi-thread"] }}
"#
    )
}

// ---------------- .env.example ----------------

pub const ENV_EXAMPLE: &str = "# Copy this file to .env and edit the values for your environment.
# `dotenvy::dotenv()` in src/bin/manage.rs picks it up at startup.
DATABASE_URL=postgres://rustango:rustango@localhost:5432/rustango_dev
RUSTANGO_BIND=127.0.0.1:8080

# Tenancy template only — apex domain + signing secret.
RUSTANGO_APEX_DOMAIN=localhost
RUSTANGO_SESSION_SECRET=change-me-base64-encoded-32-bytes-or-more
";

// ---------------- .gitignore ----------------

pub const GITIGNORE: &str = "/target
/.env
*.log
";

// ---------------- rust-toolchain.toml ----------------

/// Pin rustup to 1.88 in the new project so users on macOS who have
/// Homebrew's older `rust` binary on PATH (currently 1.86) don't get
/// the "rustc 1.86.0 is not supported by the following packages"
/// error when they `cd` into the project. rustup reads this file and
/// silently uses 1.88 inside the project regardless of which cargo
/// they invoked. v0.8 rustango requires 1.88 (workspace.package.rust-version).
pub const RUST_TOOLCHAIN: &str = "[toolchain]
channel = \"1.88\"
";

// ---------------- docker-compose.yml ----------------

pub fn docker_compose(name: &str) -> String {
    format!(
        r#"services:
  postgres:
    image: postgres:16-alpine
    environment:
      POSTGRES_USER: rustango
      POSTGRES_PASSWORD: rustango
      POSTGRES_DB: {name}_dev
    ports:
      - "5432:5432"
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U rustango -d {name}_dev"]
      interval: 2s
      timeout: 2s
      retries: 20
"#
    )
}

// ---------------- README.md ----------------

pub fn readme(name: &str, template: Template) -> String {
    let template_label = match template {
        Template::Api => "api (bare ORM + axum, no admin)",
        Template::Fullstack => "fullstack (ORM + auto-admin)",
        Template::Tenant => "tenant (multi-tenancy + operator console)",
    };
    format!(
        r#"# {name}

Generated with `cargo rustango new {name}` — template `{template_label}`.

## Run locally

```sh
cp .env.example .env
docker compose up -d
cargo run --bin manage -- migrate    # apply pending migrations
cargo run                            # boot the HTTP server
```

## Project layout

```text
src/
  main.rs         — boots the binary, wires the router
  models.rs       — every #[derive(Model)] lives here
  views.rs        — request handlers (Django-style "views")
  urls.rs         — pub fn router(pool) -> Router mapping paths → views
  bin/manage.rs   — Django-style migration / scaffolding CLI

migrations/       — JSON migration files (committed to git)
```

Adding a new model is one struct in `models.rs`; the auto-admin sees
it immediately. See <https://github.com/ujeenet/rustango> for the full
feature list.
"#
    )
}

// ---------------- src/main.rs ----------------

/// Per-template `main.rs` — api/fullstack get the simple sqlx pool +
/// `urls::router(pool)` shape; tenant gets `rustango::server::Builder`
/// which auto-mounts the operator console at the apex and the tenant
/// admin at every subdomain via host-based dispatch. Without Builder,
/// the tenant template would scaffold a server that 404s on `/admin`
/// because nothing wires the auto-admin or operator console in — the
/// v0.8.1 Builder does that work.
pub fn main_rs(template: Template) -> &'static str {
    match template {
        Template::Api => MAIN_RS_API,
        Template::Fullstack => MAIN_RS_FULLSTACK,
        Template::Tenant => MAIN_RS_TENANT,
    }
}

const MAIN_RS_API: &str = "//! Project entrypoint — boots the HTTP server (api template, no admin).
//!
//! `urls::api()` is the stateless aggregator that `manage startapp`
//! patches when you add new sub-apps. The pool flows through
//! request extensions (`Extension<PgPool>`) so every linked app's
//! handlers can pull it without each one declaring a state type.

mod models;
mod urls;
mod views;

use axum::Extension;
use rustango::sql::sqlx::PgPool;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(\"info\")),
        )
        .init();

    let url = require_env(\"DATABASE_URL\")?;
    let pool = PgPool::connect(&url).await?;
    let dir: &std::path::Path = \"./migrations\".as_ref();
    let _ = rustango::migrate::migrate(&pool, dir).await?;

    let app = urls::api().layer(Extension(pool));

    let bind = std::env::var(\"RUSTANGO_BIND\").unwrap_or_else(|_| \"127.0.0.1:8080\".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    eprintln!(\"server listening on http://{}\", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}

/// Read a required environment variable, returning a friendly error
/// instead of the bare `VarError(NotPresent)` that bubbles up from `?`.
fn require_env(key: &str) -> Result<String, String> {
    std::env::var(key).map_err(|_| {
        format!(
            \"missing env var `{key}`. Set it in your shell, or copy `.env.example` to `.env` (which is auto-loaded via dotenvy on startup): cp .env.example .env\"
        )
    })
}
";

const MAIN_RS_FULLSTACK: &str = "//! Project entrypoint — boots the HTTP server (fullstack template, ORM + auto-admin).
//!
//! `urls::api()` is the stateless aggregator (`manage startapp`
//! patches `.merge(...)` lines into it). `urls::admin_router(pool)`
//! builds the auto-admin and gets nested at `/admin`. The pool
//! flows through request extensions so every linked app's
//! handlers can pull it via `axum::Extension<PgPool>`.

mod models;
mod urls;
mod views;

use axum::Extension;
use rustango::sql::sqlx::PgPool;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(\"info\")),
        )
        .init();

    let url = require_env(\"DATABASE_URL\")?;
    let pool = PgPool::connect(&url).await?;
    let dir: &std::path::Path = \"./migrations\".as_ref();
    let _ = rustango::migrate::migrate(&pool, dir).await?;

    let app = urls::api()
        .nest(\"/admin\", urls::admin_router(pool.clone()))
        .layer(Extension(pool));

    let bind = std::env::var(\"RUSTANGO_BIND\").unwrap_or_else(|_| \"127.0.0.1:8080\".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    eprintln!(\"server listening on http://{}\", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}

/// Read a required environment variable, returning a friendly error
/// instead of the bare `VarError(NotPresent)` that bubbles up from `?`.
fn require_env(key: &str) -> Result<String, String> {
    std::env::var(key).map_err(|_| {
        format!(
            \"missing env var `{key}`. Set it in your shell, or copy `.env.example` to `.env` (auto-loaded via dotenvy on startup): cp .env.example .env\"
        )
    })
}
";

const MAIN_RS_TENANT: &str = r##"//! Tenant project entrypoint — host-dispatcher wiring.
//!
//! Mounted routes:
//!
//! * Apex (`localhost:8080`)            → operator console
//!   - `/login`               operator login form (admin / your password)
//!   - `/<table>`             registry CRUD: rustango_orgs, rustango_users, ...
//! * Subdomain (`acme.localhost:8080`)  → tenant admin + your `urls::api()`
//!   - `/__login`             tenant user login (alice / your password)
//!   - `/<table>`             tenant CRUD on every #[derive(Model)] type
//!   - your custom routes from `urls::api()` (default `/` and `/healthz`)
//!
//! Looks like a lot of wiring? It is — and v0.8.x will introduce a
//! `rustango::server::Builder::from_env().migrate("migrations")
//! .api(urls::api()).serve(...)` shorthand that collapses the body
//! below to ~6 lines. Until that version is on crates.io, this is
//! the canonical shape against published rustango v0.8.0.

mod models;
mod urls;
mod views;

use std::sync::Arc;

use rustango::sql::sqlx::PgPool;
use rustango::tenancy::{
    admin::TenantAdminBuilder,
    operator_console::{self, SessionSecret},
    ChainResolver, HeaderResolver, SubdomainResolver, TenantPools,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load .env so DATABASE_URL / RUSTANGO_APEX_DOMAIN /
    // RUSTANGO_SESSION_SECRET are visible without re-exporting them.
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,sqlx=warn")),
        )
        .init();

    let url = require_env("DATABASE_URL")?;
    let apex = std::env::var("RUSTANGO_APEX_DOMAIN").unwrap_or_else(|_| "localhost".into());
    let bind = std::env::var("RUSTANGO_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());

    let registry = PgPool::connect(&url).await?;
    let pools = Arc::new(TenantPools::new(registry.clone()));

    // Apply registry + tenant migrations on boot. `init_tenancy` is
    // idempotent: writes the bootstrap files only if missing.
    let dir = std::path::Path::new("migrations");
    rustango::tenancy::init_tenancy(dir)?;
    let _ = rustango::tenancy::migrate_registry(&pools, dir).await?;
    let _ = rustango::tenancy::migrate_tenants(&pools, dir, &url).await?;

    // Subdomain-first resolver, X-Org header fallback for curl.
    let resolver = ChainResolver::new()
        .push(SubdomainResolver::new(apex.clone()))
        .push(HeaderResolver::default());

    let tenant_admin = TenantAdminBuilder::new(pools.clone(), url.clone(), resolver)
        .with_session(SessionSecret::from_env_or_random())
        .build();
    let tenant_app = urls::api().fallback_service(tenant_admin);

    let operator = operator_console::router(registry, SessionSecret::from_env_or_random());

    // Host dispatch: apex → operator console, subdomain → tenant admin + user routes.
    let app = axum::Router::new().fallback_service(tower::service_fn({
        let operator = operator.clone();
        let tenants = tenant_app.clone();
        let apex = apex.clone();
        move |req: axum::http::Request<axum::body::Body>| {
            let mut operator = operator.clone();
            let mut tenants = tenants.clone();
            let apex = apex.clone();
            async move {
                use tower::ServiceExt as _;
                let host = req
                    .headers()
                    .get(axum::http::header::HOST)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.split(':').next().unwrap_or(s).to_owned())
                    .unwrap_or_default();
                let resp = if host == apex {
                    operator.as_service().oneshot(req).await
                } else {
                    tenants.as_service().oneshot(req).await
                };
                resp.map_err(|e| -> std::convert::Infallible {
                    panic!("axum router service is Infallible: {e}")
                })
            }
        }
    }));

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    eprintln!("server listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}

/// Read a required environment variable, returning a friendly error
/// instead of the bare `VarError(NotPresent)` that bubbles up from `?`.
fn require_env(key: &str) -> Result<String, String> {
    std::env::var(key).map_err(|_| {
        format!(
            "missing env var `{key}`. Set it in your shell, or copy `.env.example` to `.env` (auto-loaded via dotenvy on startup): cp .env.example .env"
        )
    })
}
"##;

// ---------------- src/models.rs ----------------

pub fn models_rs(template: Template) -> String {
    let header = "//! Project models — every #[derive(Model)] lives here.
//!
//! Adding a struct here makes it admin-visible automatically: the
//! macro populates the `inventory` registry that
//! `rustango::admin::router(pool)` walks.

use rustango::sql::Auto;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = \"item\", display = \"name\")]
pub struct Item {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub name: String,
    pub active: bool,
}
";

    if matches!(template, Template::Tenant) {
        format!(
            "{header}
// Tenancy registry models (Org, Operator, User) come along
// automatically via the `rustango::tenancy::*` import in
// src/bin/manage.rs — you don't need to redefine them here.
"
        )
    } else {
        header.to_owned()
    }
}

// ---------------- src/views.rs ----------------

pub const VIEWS_RS: &str = "//! Project views — request handlers (Django-style \"views\").

use axum::response::Html;

pub async fn index() -> Html<&'static str> {
    Html(
        \"<!doctype html>\\n\\
         <title>rustango app</title>\\n\\
         <h1>Hello from rustango!</h1>\\n\\
         <p>The auto-admin (if enabled) is at <a href=\\\"/admin\\\">/admin</a>.</p>\",
    )
}

pub async fn healthz() -> &'static str {
    \"ok\"
}
";

// ---------------- src/urls.rs ----------------

pub fn urls_rs(template: Template) -> String {
    match template {
        Template::Api => {
            // Stateless aggregator. `manage startapp <name>` auto-
            // patches a `.merge(crate::<name>::urls::api())` line
            // after `Router::new()` so additional apps compose
            // cleanly. Handlers that need the pool can read it from
            // request extensions (`Extension<PgPool>`) — main.rs
            // attaches it via `.layer(Extension(pool))`.
            "//! Project URL routing (template: api — no admin).
//!
//! `Router::new()` is the auto-mount anchor — `manage startapp`
//! inserts `.merge(crate::<name>::urls::api())` lines here.

use axum::routing::get;
use axum::Router;

use crate::views;

pub fn api() -> Router<()> {
    Router::new()
        .route(\"/\", get(views::index))
        .route(\"/healthz\", get(views::healthz))
}
"
                .to_owned()
        }
        Template::Fullstack => {
            // Stateless aggregator + a separate `admin_router(pool)`
            // helper that main.rs nests under `/admin`. Pool flows
            // through `Extension<PgPool>` (attached by main.rs) so
            // all apps' handlers can grab it without each one
            // declaring the pool as a state type.
            "//! Project URL routing (template: fullstack — ORM + auto-admin).
//!
//! `Router::new()` in `api()` is the auto-mount anchor —
//! `manage startapp` inserts `.merge(crate::<name>::urls::api())`
//! lines here. The auto-admin is built separately via
//! `admin_router(pool)` and nested at `/admin` from `main.rs`.

use axum::routing::get;
use axum::Router;
use rustango::admin;
use rustango::sql::sqlx::PgPool;

use crate::views;

pub fn api() -> Router<()> {
    Router::new()
        .route(\"/\", get(views::index))
        .route(\"/healthz\", get(views::healthz))
}

pub fn admin_router(pool: PgPool) -> Router {
    admin::Builder::new(pool).build()
}
"
                .to_owned()
        }
        Template::Tenant => {
            // Multi-tenant: main.rs uses `rustango::server::Builder`,
            // which expects a stateless `Router<()>` and injects the
            // `TenantContext` extension itself so handlers can use
            // `rustango::extractors::Tenant`. The user's routes mount
            // on every tenant subdomain alongside the auto-admin.
            "//! Project URL routing (template: tenant).
//!
//! `Builder::api(...)` mounts these routes on every tenant
//! subdomain alongside the auto-admin. Handlers can take
//! `rustango::extractors::Tenant` to resolve the current tenant +
//! get a tenant-scoped `&mut PgConnection`. Example:
//!
//! ```ignore
//! pub async fn list_items(mut t: rustango::extractors::Tenant)
//!     -> Result<axum::Json<Vec<crate::models::Item>>, axum::http::StatusCode> {
//!     let rows = crate::models::Item::objects()
//!         .fetch_on(t.conn()).await
//!         .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
//!     Ok(axum::Json(rows))
//! }
//! ```

use axum::routing::get;
use axum::Router;

use crate::views;

pub fn api() -> Router<()> {
    Router::new()
        .route(\"/\", get(views::index))
        .route(\"/healthz\", get(views::healthz))
}
"
                .to_owned()
        }
    }
}

// ---------------- src/bin/manage.rs ----------------

pub fn manage_rs(template: Template) -> String {
    match template {
        Template::Api | Template::Fullstack => {
            "//! Generated by `cargo rustango new`. Edit freely.
//!
//! UX: `cargo run --bin manage -- migrate`,
//! `cargo run --bin manage -- makemigrations`,
//! `cargo run --bin manage -- startapp <name>`, etc.

use rustango::sql::sqlx::PgPool;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Pull your models into this binary so `inventory` registers
    // them. Keep this in sync with src/main.rs's `mod models;`.
    #[allow(unused_imports)]
    use crate::models::*;

    let argv: Vec<String> = std::env::args().skip(1).collect();
    // Short-circuit verbs that don't touch the DB (scaffold,
    // makemigrations, help) **before** opening a Postgres connection
    // so users who haven't set DATABASE_URL yet can still scaffold
    // apps and generate migration files.
    let needs_db = !matches!(
        argv.first().map(String::as_str),
        None | Some(\"help\") | Some(\"--help\") | Some(\"-h\")
            | Some(\"startapp\") | Some(\"makemigrations\")
    );

    let _ = dotenvy::dotenv();
    let dir: &std::path::Path = \"./migrations\".as_ref();

    if needs_db {
        let url = std::env::var(\"DATABASE_URL\").map_err(|_| {
            \"missing env var `DATABASE_URL`. Set it in your shell, or copy `.env.example` to `.env` (auto-loaded via dotenvy on startup): cp .env.example .env\".to_owned()
        })?;
        let pool = PgPool::connect(&url).await?;
        rustango::migrate::manage::run(&pool, dir, argv).await?;
    } else {
        // No DB needed — hand the dispatcher a pool that lazy-fails
        // if a verb still tries to use it. We pass DATABASE_URL when
        // present (so e.g. `makemigrations` against a real DB works
        // when the user did set it) and a placeholder otherwise; the
        // placeholder pool builds without contacting Postgres.
        let url = std::env::var(\"DATABASE_URL\")
            .unwrap_or_else(|_| \"postgres://offline\".into());
        let pool = PgPool::connect_lazy(&url)?;
        rustango::migrate::manage::run(&pool, dir, argv).await?;
    }
    Ok(())
}

// `cargo run --bin manage` is a separate binary; pull the project's
// own crate root models into scope so `inventory::submit!` fires
// for every `#[derive(Model)]`. `#[path]` is the canonical way to
// re-locate a module file from a binary that doesn't share its
// parent's tree (binary-only projects don't have a lib.rs to import
// from).
#[path = \"../models.rs\"]
mod models;
"
                .to_owned()
        }
        Template::Tenant => {
            "//! Generated by `cargo rustango new --template tenant`. Edit freely.
//!
//! Tenancy-aware dispatcher: `create-tenant`, `migrate-tenants`,
//! `run-server`, `create-operator`, `create-user`, plus everything
//! the single-tenant `manage` offers.

use rustango::sql::sqlx::PgPool;
use rustango::tenancy::TenantPools;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // Short-circuit help **before** connecting to Postgres so users
    // who haven't yet copied `.env.example` to `.env` can still read
    // the verb list. Same for the no-args case (running `manage` with
    // nothing prints the help instead of an opaque error).
    match argv.first().map(String::as_str) {
        None | Some(\"help\") | Some(\"--help\") | Some(\"-h\") => {
            print!(\"{}\", HELP);
            return Ok(());
        }
        _ => {}
    }

    let _ = dotenvy::dotenv();
    let registry_url = std::env::var(\"DATABASE_URL\").map_err(|_| {
        \"missing env var `DATABASE_URL`. Set it in your shell, or copy `.env.example` to `.env` (auto-loaded via dotenvy on startup): cp .env.example .env\".to_owned()
    })?;
    let pool = PgPool::connect(&registry_url).await?;
    let pools = TenantPools::new(pool);
    let dir: &std::path::Path = \"./migrations\".as_ref();
    rustango::tenancy::manage::run(&pools, &registry_url, dir, argv).await?;
    Ok(())
}

const HELP: &str = r#\"rustango manage CLI — tenancy-aware dispatcher

USAGE:
  cargo run --bin manage -- <verb> [args]

TENANT MANAGEMENT:
  create-tenant <slug> [--display-name <s>] [--mode schema|database]
                       [--host-pattern <s>] [--database-url <s>] [--no-migrate]
                       Provision a new tenant. Schema mode (default) gives the
                       tenant its own Postgres schema; database mode points at
                       a fully separate DB via --database-url.
  drop-tenant   <slug> [--confirm <slug>]
                       Soft-delete (active=false). Data preserved.
  purge-tenant  <slug> [--confirm <slug>] [--purge-database]
                       HARD-delete: drops schema (or DB with --purge-database).
  list-tenants         Print every Org row in the registry.

USER / OPERATOR MANAGEMENT:
  create-operator <username> --password <p>
                       Operator-level account; signs into the apex /login.
  create-user <slug> <username> --password <p> [--superuser]
                       Tenant-scoped user; signs into <slug>.<apex>/__login.

MIGRATIONS:
  init-tenancy         Materialize bootstrap migrations into ./migrations/.
  makemigrations       Diff models against latest snapshot, emit a new JSON file.
  migrate              Apply registry-scoped, then tenant-scoped, migrations.
  migrate-registry     Apply registry-scoped migrations only.
  migrate-tenants      Apply tenant-scoped migrations to every active org.
  showmigrations       List which migrations are applied / pending.
  downgrade            Roll back the most recent migration.

SERVER:
  run-server [--bind <addr>]
                       Boot the HTTP server with admin + operator console.

SCAFFOLDING:
  startapp <name> [--into <dir>] [--with-manage-bin] [--with-bootstrap-migration]
                       Scaffold a Django-shape app module.

EXAMPLES:
  cargo run --bin manage -- migrate
  cargo run --bin manage -- create-operator admin --password letmein
  cargo run --bin manage -- create-tenant acme --display-name 'ACME Corp'
  cargo run --bin manage -- create-user acme alice --password hunter2 --superuser
  cargo run --bin manage -- list-tenants

Run any verb with --help for verb-specific flags + details.
\"#;
"
                .to_owned()
        }
    }
}


// ---------------- Bootstrap migrations (tenant template) ----------------

/// Registry-scoped bootstrap migration shipped by `rustango::tenancy`.
/// Embedded as a static string so `cargo rustango new --template tenant`
/// drops a working `migrations/` dir into the new project — the very
/// first `cargo run --bin manage -- migrate` creates `rustango_orgs` /
/// `rustango_operators` without a separate `manage init-tenancy` step.
///
/// Regenerate by running `cargo test -p rustango --test dump_bootstrap
/// --features tenancy` and copying the output into
/// `crates/cargo-rustango/templates/`.
pub const BOOTSTRAP_REGISTRY_MIGRATION: &str =
    include_str!("../templates/0001_rustango_registry_initial.json");

/// Tenant-scoped bootstrap migration — same provenance as
/// [`BOOTSTRAP_REGISTRY_MIGRATION`]. Creates `rustango_users` inside
/// each tenant's schema/database when `manage migrate-tenants` runs.
pub const BOOTSTRAP_TENANT_MIGRATION: &str =
    include_str!("../templates/0001_rustango_tenant_initial.json");

