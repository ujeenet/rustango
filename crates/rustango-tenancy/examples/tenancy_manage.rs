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
//! # Bootstrap an operator (first run also creates the registry tables).
//! cargo run --example tenancy_manage -p rustango-tenancy -- \
//!     create-operator admin --password letmein
//!
//! # List operators.
//! cargo run --example tenancy_manage -p rustango-tenancy -- list-tenants
//!
//! # Provision a schema-mode tenant. host_pattern defaults to
//! # `<slug>.<RUSTANGO_APEX_DOMAIN>`.
//! RUSTANGO_APEX_DOMAIN=localhost cargo run --example tenancy_manage \
//!     -p rustango-tenancy -- create-tenant acme --mode schema --no-migrate
//!
//! # Create a per-tenant user.
//! cargo run --example tenancy_manage -p rustango-tenancy -- \
//!     create-user acme alice --password hunter2 --superuser
//!
//! # Soft-delete a tenant (data preserved).
//! cargo run --example tenancy_manage -p rustango-tenancy -- \
//!     drop-tenant acme --confirm acme
//!
//! # Anything not a tenancy verb falls through to the standard
//! # rustango-migrate manage runner against the registry pool:
//! cargo run --example tenancy_manage -p rustango-tenancy -- showmigrations
//! cargo run --example tenancy_manage -p rustango-tenancy -- migrate
//! ```
//!
//! ## What it bootstraps on first run
//!
//! `rustango_tenancy` registers `Org`, `Operator`, and `User` in the
//! inventory but does **not** ship packaged bootstrap migrations
//! yet (deferred to a later v0.6 slice). On first run this binary
//! issues `CREATE TABLE IF NOT EXISTS` for `rustango_orgs` and
//! `rustango_operators` against the registry so subcommands work
//! without you having to also run `multitenant_demo` first.
//!
//! Per-tenant `rustango_users` is NOT created here — `create-tenant`
//! creates the tenant's schema, but the `rustango_users` table
//! inside it currently has to be hand-rolled (see `multitenant_demo`
//! for a working example) until packaged tenant migrations land.

use rustango::core::Model as _;
use rustango::migrate;
use rustango::sql::sqlx::PgPool;
use rustango_tenancy::TenantPools;

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

    // First-run bootstrap. Idempotent (CREATE TABLE IF NOT EXISTS).
    // Only the two registry-scoped tables; per-tenant rustango_users
    // is the operator's responsibility for now.
    let orgs_sql = migrate::ddl::create_table_if_not_exists_sql(
        rustango_tenancy::Org::SCHEMA,
    );
    let operators_sql = migrate::ddl::create_table_if_not_exists_sql(
        rustango_tenancy::Operator::SCHEMA,
    );
    rustango::sql::sqlx::query(&orgs_sql).execute(&registry).await?;
    rustango::sql::sqlx::query(&operators_sql).execute(&registry).await?;

    let pools = TenantPools::new(registry.clone());
    let dir = std::path::Path::new("./tenancy_manage_migrations");
    let args = std::env::args().skip(1);

    rustango_tenancy::manage::run(&pools, &registry_url, dir, args).await?;
    Ok(())
}
