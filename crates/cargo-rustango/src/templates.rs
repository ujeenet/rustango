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

pub const MAIN_RS: &str = "//! Project entrypoint — boots the HTTP server.
//!
//! `manage.rs` (under src/bin/) handles migrations + scaffolding;
//! this binary is just the runtime web server.

mod models;
mod urls;
mod views;

use rustango::sql::sqlx::PgPool;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load .env (or any ancestor) so DATABASE_URL et al. are set
    // without re-exporting each session.
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(\"info\")),
        )
        .init();

    let url = std::env::var(\"DATABASE_URL\")?;
    let pool = PgPool::connect(&url).await?;
    let app = urls::router(pool);

    let bind = std::env::var(\"RUSTANGO_BIND\").unwrap_or_else(|_| \"127.0.0.1:8080\".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    eprintln!(\"server listening on http://{}\", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}
";

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
            // No admin import; just custom views.
            "//! Project URL routing (template: api — no admin).

use axum::routing::get;
use axum::Router;
use rustango::sql::sqlx::PgPool;

use crate::views;

pub fn router(_pool: PgPool) -> Router {
    Router::new()
        .route(\"/\", get(views::index))
        .route(\"/healthz\", get(views::healthz))
}
"
                .to_owned()
        }
        Template::Fullstack => {
            // ORM + auto-admin nested under /admin.
            "//! Project URL routing (template: fullstack — ORM + auto-admin).

use axum::routing::get;
use axum::Router;
use rustango::admin;
use rustango::sql::sqlx::PgPool;

use crate::views;

pub fn router(pool: PgPool) -> Router {
    let admin = admin::Builder::new(pool.clone()).build();
    Router::new()
        .route(\"/\", get(views::index))
        .route(\"/healthz\", get(views::healthz))
        .with_state(pool)
        .nest(\"/admin\", admin)
}
"
                .to_owned()
        }
        Template::Tenant => {
            // Multi-tenant — main.rs is unusual here because the
            // typical entrypoint is `manage run-server`, not a
            // bespoke router. We still ship a simple router for the
            // dev experience.
            "//! Project URL routing (template: tenant — multi-tenancy enabled).
//!
//! For production wiring, the recommended entrypoint is
//! `cargo run --bin manage -- run-server` which boots the operator
//! console at the apex + tenant admin at every subdomain via
//! host-based dispatch. This file is the simpler dev-server form.

use axum::routing::get;
use axum::Router;
use rustango::sql::sqlx::PgPool;

use crate::views;

pub fn router(_pool: PgPool) -> Router {
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

    let _ = dotenvy::dotenv();
    let pool = PgPool::connect(&std::env::var(\"DATABASE_URL\")?).await?;
    let dir: &std::path::Path = \"./migrations\".as_ref();
    rustango::migrate::manage::run(&pool, dir, std::env::args().skip(1)).await?;
    Ok(())
}

// `cargo run --bin manage` is a separate binary; pull the project's
// own crate root into scope as a dependency for the model imports
// above. Cargo gives every `[[bin]]` access to the lib via the crate
// name; for binary-only projects we use a path module instead.
mod models {
    include!(\"../models.rs\");
}
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
    let _ = dotenvy::dotenv();
    let registry_url = std::env::var(\"DATABASE_URL\")?;
    let pool = PgPool::connect(&registry_url).await?;
    let pools = TenantPools::new(pool);
    let dir: &std::path::Path = \"./migrations\".as_ref();
    rustango::tenancy::manage::run(&pools, &registry_url, dir, std::env::args().skip(1)).await?;
    Ok(())
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

