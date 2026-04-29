//! `tenancy_manage` — runnable CLI for the rustango-tenancy `manage`
//! subcommands. Shape mirrors the standard Django `manage.py` flow.
//!
//! ## Usage
//!
//! Bring up Postgres first:
//!
//! ```sh
//! docker compose up -d
//! ```
//!
//! Then run any tenancy verb. `DATABASE_URL` defaults to the docker
//! compose creds; `RUSTANGO_APEX_DOMAIN` is consulted by
//! `create-tenant` to default `host_pattern`.
//!
//! ```sh
//! # First run auto-bootstraps the registry — when `rustango_operators`
//! # is missing, the binary writes the packaged migrations into
//! # ./tenancy_manage_migrations and applies the registry-scoped one
//! # before dispatching whatever verb you typed. So running any verb
//! # against a fresh DB Just Works; you don't have to remember
//! # `init-tenancy && migrate` first. (Both verbs still exist for
//! # explicit control.)
//!
//! # Bootstrap an operator.
//! cargo run --example tenancy_manage -p rustango-tenancy -- \
//!     create-operator admin --password letmein
//!
//! # List operators.
//! cargo run --example tenancy_manage -p rustango-tenancy -- list-tenants
//!
//! # Provision a schema-mode tenant. host_pattern defaults to
//! # `<slug>.<RUSTANGO_APEX_DOMAIN>`. Without `--no-migrate` the
//! # tenant-scoped bootstrap migration runs against the new schema,
//! # so `rustango_users` is created automatically.
//! RUSTANGO_APEX_DOMAIN=localhost cargo run --example tenancy_manage \
//!     -p rustango-tenancy -- create-tenant acme --mode schema
//!
//! # Create a per-tenant user.
//! cargo run --example tenancy_manage -p rustango-tenancy -- \
//!     create-user acme alice --password hunter2 --superuser
//!
//! # Soft-delete a tenant (data preserved).
//! cargo run --example tenancy_manage -p rustango-tenancy -- \
//!     drop-tenant acme --confirm acme
//!
//! # Hard-delete a tenant (UNRECOVERABLE — drops schema CASCADE).
//! cargo run --example tenancy_manage -p rustango-tenancy -- \
//!     purge-tenant acme --confirm acme
//!
//! # Hard-delete a database-mode tenant (drops the dedicated DB).
//! cargo run --example tenancy_manage -p rustango-tenancy -- \
//!     purge-tenant acme --confirm acme --purge-database
//!
//! # Anything not a tenancy verb falls through to the standard
//! # rustango-migrate manage runner against the registry pool:
//! cargo run --example tenancy_manage -p rustango-tenancy -- showmigrations
//!
//! # Plain `migrate` is scope-aware: registry-scoped migrations
//! # apply to the registry pool, then tenant-scoped ones fan out
//! # across active orgs.
//! cargo run --example tenancy_manage -p rustango-tenancy -- migrate
//!
//! # Boot the operator console + tenant admin (Ctrl-C to stop):
//! cargo run --example tenancy_manage -p rustango-tenancy -- run-server
//! # Listens on RUSTANGO_BIND (default 0.0.0.0:8080) and routes apex →
//! # operator console, *.<RUSTANGO_APEX_DOMAIN> → tenant admin.
//! # `runserver` is an alias for `run-server` (matches Django muscle memory).
//! ```
//!
//! ## Bootstrap flow
//!
//! `rustango-tenancy` ships two packaged bootstrap migrations
//! ([`rustango::tenancy::bootstrap`]) — a registry-scoped one for
//! `rustango_orgs` + `rustango_operators`, and a tenant-scoped one
//! for `rustango_users`. `init-tenancy` writes them into the
//! migrations directory; `migrate` applies them. Re-running
//! `init-tenancy` is idempotent — existing files are left
//! untouched.

use rustango::sql::sqlx::PgPool;
use rustango::tenancy::TenantPools;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Auto-load `.env` (in the working dir or any ancestor) before
    // reading env vars, so users don't need to `source ./.env` or
    // re-export DATABASE_URL / RUSTANGO_APEX_DOMAIN / RUSTANGO_SESSION_SECRET
    // each session. `.env` is gitignored by convention; if it
    // doesn't exist we fall through silently.
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,sqlx=warn".into()),
        )
        .init();

    let registry_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://rustango:rustango@127.0.0.1:5432/rustango_test".into());
    let registry = PgPool::connect(&registry_url).await?;

    let pools = TenantPools::new(registry.clone());
    let dir = std::path::Path::new("./tenancy_manage_migrations");

    // First-run auto-bootstrap. If `rustango_operators` doesn't
    // exist yet, write the packaged tenancy migrations to `dir` and
    // apply the registry-scoped one so subcommands like
    // `create-operator` and `run-server` work out of the box.
    // Idempotent: subsequent runs find the table and skip both
    // steps. The tenant-scoped bootstrap is applied per-tenant by
    // `create-tenant` (without `--no-migrate`), so we only do the
    // registry phase here.
    if !registry_tables_exist(&registry).await? {
        eprintln!("==> first run: bootstrapping registry via init-tenancy + migrate-registry");
        rustango::tenancy::bootstrap::init_tenancy(dir)?;
        rustango::tenancy::migrate_registry(&pools, dir).await?;
    }

    let args = std::env::args().skip(1);
    rustango::tenancy::manage::run(&pools, &registry_url, dir, args).await?;
    Ok(())
}

async fn registry_tables_exist(pool: &PgPool) -> Result<bool, rustango::sql::sqlx::Error> {
    use rustango::sql::sqlx;
    let row: (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_name = 'rustango_operators')",
    )
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}
