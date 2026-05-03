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

const MAIN_RS_API: &str = "//! Project entrypoint — `Cli::run()` is the unified dispatcher
//! that handles `cargo run` (runserver) AND `cargo run -- migrate` /
//! `makemigrations` / `startapp` / etc. from one binary. No
//! `src/bin/manage.rs` needed.

mod models;
mod urls;
mod views;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    rustango::manage::Cli::new().api(urls::api()).run().await
}
";

const MAIN_RS_FULLSTACK: &str = "//! Project entrypoint — `Cli::run()` is the unified dispatcher
//! that handles `cargo run` (runserver) AND `cargo run -- migrate` /
//! `makemigrations` / `startapp` / etc. from one binary. No
//! `src/bin/manage.rs` needed. Auto-admin is mounted via
//! `urls::api()` which nests `admin_router(pool)` itself.

mod models;
mod urls;
mod views;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    rustango::manage::Cli::new().api(urls::api()).run().await
}
";

const MAIN_RS_TENANT: &str = r##"//! Tenant project entrypoint — `Cli::tenancy().run()` is the unified
//! dispatcher. It owns the apex/subdomain host split, operator console
//! mount, registry + tenant migrations, and `cargo run -- create-tenant`
//! / `migrate-tenants` / `create-operator` / `create-user` verbs from
//! one binary. No `src/bin/manage.rs` needed.

mod models;
mod urls;
mod views;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    rustango::manage::Cli::new()
        .tenancy()
        .api(urls::api())
        .run().await
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

