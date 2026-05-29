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
# `rustango` re-exports every proc-macro (Model / Form / ViewSet /
# Serializer / embed_migrations / main), so you do NOT need to depend
# on `rustango-macros` directly. Use `rustango::Model` etc.
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

# The `#[derive(Model)]` FromRow / LoadRelated impls are
# `#[cfg(feature = "...")]`-gated on THIS crate's features, so a backend
# feature must be enabled here for them to compile. `default` picks the
# one `cargo run` uses; switch with e.g.
# `cargo run --no-default-features --features sqlite`.
[features]
default = ["postgres"]
postgres = ["rustango/postgres"]
sqlite = ["rustango/sqlite"]
mysql = ["rustango/mysql"]
"#
    )
}

// ---------------- .env.example ----------------

pub fn env_example(name: &str) -> String {
    format!(
        "# Copy this file to .env and edit the values for your environment.
# `dotenvy::dotenv()` in src/main.rs picks it up at startup.
#
# Defaults are Docker-friendly (`postgres` host, `0.0.0.0` bind) so
# `docker compose up -d` boots a working stack without any edits.
# If you run cargo on the host instead of in the rust container,
# change `postgres` -> `localhost` in DATABASE_URL.
#
# Database name matches docker-compose.yml's POSTGRES_DB ({name}_dev).
DATABASE_URL=postgres://rustango:rustango@postgres:5432/{name}_dev
RUSTANGO_BIND=0.0.0.0:8080

# Tenancy template only — apex domain + signing secret.
# Generate a real secret with: openssl rand -base64 32
RUSTANGO_APEX_DOMAIN=localhost
RUSTANGO_SESSION_SECRET=change-me-base64-encoded-32-bytes-or-more

# ---------------- Logging (ujeenet/rustango-cms#305) ----------------
# `#[rustango::main]` auto-installs a tracing_subscriber::fmt with
# env-filter; the default is `info,sqlx=warn`. Uncomment to turn on
# more verbose output without code changes. Standard `RUST_LOG`
# syntax — per-target filtering is the easiest knob.
#
#   `debug` — everything DEBUG+ across every crate (very noisy)
#   `info,my_app=debug` — INFO globally, DEBUG for one module
#   `info,sqlx=warn,hyper=warn` — quiet down noisy upstreams
#
# Production deployments override this in the orchestrator (k8s env,
# systemd unit, etc.) rather than editing this file.
# RUST_LOG=info,sqlx=warn,hyper=warn
"
    )
}

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

/// Bundle a working Postgres + Rust hot-reload dev stack out of the
/// box (#86). The `rust` service runs `cargo watch -x run` against
/// the bind-mounted source tree; the three named volumes
/// (`cargo-target`, `cargo-registry`, `cargo-git`) preserve
/// incremental build state across container restarts so a fresh
/// `docker compose up` doesn't trigger a full from-scratch rebuild.
///
/// Users who prefer running cargo on the host can simply ignore the
/// `rust` service and run the postgres service standalone with
/// `docker compose up -d postgres`.
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

  # Hot-reload Rust dev container. `cargo watch -x run` rebuilds and
  # restarts the binary on every source edit. Skip this service (run
  # cargo on the host) by passing `--no-deps postgres` to
  # `docker compose up`, or remove it entirely if you don't want the
  # Docker-based dev loop.
  rust:
    build: .
    depends_on:
      postgres:
        condition: service_healthy
    env_file:
      - .env
    volumes:
      - ./:/app
      - cargo-target:/app/target
      - cargo-registry:/usr/local/cargo/registry
      - cargo-git:/usr/local/cargo/git
    environment:
      CARGO_TARGET_DIR: /app/target
      CARGO_INCREMENTAL: "1"
    command: bash -c "cargo watch -x run"
    ports:
      - "8080:8080"

volumes:
  cargo-target:
  cargo-registry:
  cargo-git:
"#
    )
}

// ---------------- Dockerfile ----------------

/// Companion to [`docker_compose`] — a thin Rust dev image with
/// `cargo-watch` preinstalled. The image stays small because all
/// project sources land via the bind mount; this image only needs
/// the toolchain + cargo-watch installed.
///
/// The rust version is pinned to the same value as
/// `rust-toolchain.toml`'s `channel`. Override by editing both
/// files together if you bump the toolchain.
pub fn dockerfile() -> &'static str {
    "FROM rust:1.88\n\
     \n\
     WORKDIR /app\n\
     \n\
     RUN cargo install cargo-watch\n"
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

## Run locally — two paths

### A. All-in-Docker (default; hot-reload via cargo-watch)

```sh
cp .env.example .env
docker compose up -d                 # boots postgres + rust + cargo-watch
docker compose run --rm rust cargo run -- migrate
# server lives at http://localhost:8080 — edits to src/ trigger rebuild
```

The `rust` service runs `cargo watch -x run` against the bind-mounted
source tree. Three named volumes preserve incremental build state
across container restarts so a fresh `up` doesn't recompile from
scratch.

### B. Cargo on the host (Docker just for postgres)

```sh
cp .env.example .env                 # then change `postgres` -> `localhost` in DATABASE_URL
docker compose up -d postgres        # only the DB
cargo run -- migrate                 # apply pending migrations
cargo run                            # boot the HTTP server
cargo run -- --help                  # full verb list (makemigrations, startapp, etc.)
```

Either way: `cargo run` (no args) is `runserver`. Every other
Django-style verb flows through the same binary via
`rustango::manage::Cli` — see `src/main.rs`.

## Project layout

```text
src/
  main.rs         — Cli::new().api(urls::api()).run() boots both server + verbs
  models.rs       — every #[derive(Model)] lives here
  views.rs        — request handlers (Django-style "views")
  urls.rs         — pub fn api() -> Router aggregator

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
//!
//! Logging is auto-configured by `#[rustango::main]` —
//! `tracing_subscriber::fmt` with env-filter, default
//! `info,sqlx=warn`. Override with `RUST_LOG` (see `.env.example`)
//! or replace the macro with a hand-rolled subscriber in front of
//! `Cli::new()` for JSON / file-rotation / OTel export.

mod models;
mod urls;
mod views;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    rustango::manage::Cli::new()
        .api(urls::api())
        .with_welcome() // friendly `/` on first run; drop once you have a root handler
        .run()
        .await
}
";

const MAIN_RS_FULLSTACK: &str = "//! Project entrypoint — `Cli::run()` is the unified dispatcher
//! that handles `cargo run` (runserver) AND `cargo run -- migrate` /
//! `makemigrations` / `startapp` / etc. from one binary. No
//! `src/bin/manage.rs` needed. The auto-admin lives in
//! `urls::admin_router(pool)`; nest it under `/admin` when you want
//! it (see the getting-started guide).
//!
//! Logging is auto-configured by `#[rustango::main]` —
//! `tracing_subscriber::fmt` with env-filter, default
//! `info,sqlx=warn`. Override with `RUST_LOG` (see `.env.example`)
//! or replace the macro with a hand-rolled subscriber in front of
//! `Cli::new()` for JSON / file-rotation / OTel export.

mod models;
mod urls;
mod views;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    rustango::manage::Cli::new()
        .api(urls::api())
        .with_welcome() // friendly `/` on first run; drop once you have a root handler
        .with_health() // /health + /ready endpoints for load balancers
        .run()
        .await
}
";

const MAIN_RS_TENANT: &str = r##"//! Tenant project entrypoint — HTTP server serving both the operator
//! console and per-tenant apps via subdomain routing.
//!
//! Logging is auto-configured by `#[rustango::main]` —
//! `tracing_subscriber::fmt` with env-filter, default
//! `info,sqlx=warn`. Override with `RUST_LOG` (see `.env.example`)
//! or replace the macro with a hand-rolled subscriber in front of
//! `Cli::new()` for JSON / file-rotation / OTel export.

mod models;
mod urls;
mod views;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    rustango::manage::Cli::new()
        .tenancy()
        .api(urls::api())
        .with_welcome() // friendly `/` on first run; drop once you have a root handler
        .with_health() // /health + /ready hit the registry pool
        .run()
        .await
}
"##;

// MANAGE_RS_TENANT removed in v0.16 — tenant projects now use the
// same single-binary `Cli::new().tenancy().run()` shape as every
// other template; src/main.rs is the one entrypoint.

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
    // `admin_prefix` must match the path you nest the admin under in
    // `main.rs` (e.g. `.nest(\"/admin\", urls::admin_router(pool))`) so
    // the admin's own links + form actions resolve.
    admin::Builder::new(pool).admin_prefix(\"/admin\").build()
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
/// first `cargo run -- migrate` creates `rustango_orgs` /
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

// ---------------- Tiered settings files (#87) ----------------
//
// Every fresh project ships with four config files: `default.toml`
// for shared knobs + one `<env>_settings.toml` per tier (dev /
// staging / prod). The runtime picks the tier from `RUSTANGO_ENV`
// (default `dev`). Sensible-by-default values mean a freshly-
// scaffolded `cargo run` works without any TOML edits; production
// deploys override the prod tier with their own values.

/// `config/default.toml` — shared values that don't depend on tier.
/// Intentionally sparse — every section is optional and defaults to
/// the section's `Default` impl. Tier files override here.
pub fn config_default_toml(name: &str) -> String {
    format!(
        r##"# {name} — shared defaults across every tier
# (`config/default.toml` is loaded first; `config/<RUSTANGO_ENV>_settings.toml`
# overrides on top.) Every section is optional — uncomment + edit
# what you need.

# [database]
# url           = "postgres://localhost/{name}"
# pool_min_size = 2
# pool_max_size = 20

# [admin]
# allowed_tables   = []        # empty = every registered model
# read_only_tables = []

# [server]
# bind                  = "127.0.0.1:8080"
# request_timeout_secs  = 30
# max_body_bytes        = 2097152      # 2 MiB

# [auth]
# argon2_memory_kib  = 19456    # OWASP 2024 floor
# argon2_iterations  = 2
# lockout_threshold  = 5
# lockout_duration_secs = 900

# [auth.jwt]
# access_ttl_secs   = 900       # 15 min
# refresh_ttl_secs  = 604800    # 7 days
# issuer            = "{name}"

# [brand]
# name           = "{name}"
# tagline        = ""
# primary_color  = "#2c6fb0"
# theme_mode     = "auto"       # auto | light | dark

# [security]
# headers_preset       = "strict"
# hsts_max_age_secs    = 31536000     # 1 year
# cors_allowed_origins = []

# [routes]
# legacy_preset = false
# # Per-field overrides: login_url / admin_url / audit_url / static_url /
# # brand_url / change_password_url / impersonation_handoff_url

# [audit]
# retention_days = 90
"##
    )
}

/// `config/dev_settings.toml` — local development. Loose defaults:
/// short JWT TTLs (so token-rotation bugs surface early), no HSTS
/// (so http→https rebinds don't lock the browser), debug-friendly
/// settings.
pub fn config_dev_settings_toml(name: &str) -> String {
    format!(
        r##"# {name} — local development tier
# Loaded when RUSTANGO_ENV=dev (the default when unset).

[database]
url = "postgres://postgres:postgres@localhost:5432/{name}_dev"

[server]
bind = "127.0.0.1:8080"

[security]
# Drop strict headers in dev so http<->https rebinds don't lock the
# browser into HSTS.
headers_preset    = "dev"
hsts_max_age_secs = 0

[brand]
# Make the dev tier visually distinguishable from prod.
tagline = "(dev)"
"##
    )
}

/// `config/staging_settings.toml` — production-like but pointed at
/// a separate database, with shorter retention.
pub fn config_staging_settings_toml(name: &str) -> String {
    format!(
        r##"# {name} — staging tier
# Loaded when RUSTANGO_ENV=staging. Production-shape security
# headers, but pointed at a separate database with shorter retention
# so QA volume doesn't bleed into prod analytics.

# [database]
# url = "postgres://staging-host/{name}_staging"

[server]
bind = "0.0.0.0:8080"

[security]
headers_preset    = "strict"
hsts_max_age_secs = 31536000

[brand]
tagline = "(staging)"

[audit]
retention_days = 30
"##
    )
}

/// `config/prod_settings.toml` — production. Strict defaults; expects
/// real values (DATABASE_URL, secret_key, etc.) supplied via env
/// vars or out-of-band secret management. The TOML purposefully
/// leaves the database url commented — operators set it via
/// `RUSTANGO__DATABASE__URL` or a secrets manager.
pub fn config_prod_settings_toml(name: &str) -> String {
    format!(
        r##"# {name} — production tier
# Loaded when RUSTANGO_ENV=prod. Strict-by-default; sensitive values
# (database url, secret key) come from RUSTANGO__* env vars or your
# secrets manager — leaving them out of source control.

# [database]
# url = "set via RUSTANGO__DATABASE__URL or your secrets manager"
pool_min_size = 5
pool_max_size = 50

[server]
bind                 = "0.0.0.0:8080"
request_timeout_secs = 30

[security]
headers_preset    = "strict"
hsts_max_age_secs = 31536000

[audit]
retention_days = 365
"##
    )
}
