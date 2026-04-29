//! Tenant provisioning + management subcommands.
//!
//! Composes with `rustango_migrate::manage::run` — recognizes the
//! tenancy-specific verbs (`create-tenant`, `drop-tenant`,
//! `purge-tenant`, `list-tenants`, `migrate-tenants`,
//! `migrate-registry`, `init-tenancy`, `create-operator`,
//! `create-user`, `run-server`, scope-aware `migrate`) and delegates
//! everything else to the standard single-tenant runner using the
//! registry pool.
//!
//! ## User wiring
//!
//! ```ignore
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let registry_url = std::env::var("DATABASE_URL")?;
//!     let pool = rustango::sql::sqlx::PgPool::connect(&registry_url).await?;
//!     let pools = rustango_tenancy::TenantPools::new(pool);
//!     let dir = std::path::Path::new("./migrations");
//!     rustango_tenancy::manage::run(
//!         &pools,
//!         &registry_url,
//!         dir,
//!         std::env::args().skip(1),
//!     ).await?;
//!     Ok(())
//! }
//! ```
//!
//! ## Crate-internal layout (Django-shape, slice 6)
//!
//! - [`tenants`] — `create-tenant`, `drop-tenant`, `purge-tenant`,
//!   `list-tenants` plus the database-mode admin-DROP helper.
//! - [`users`] — `create-operator`, `create-user`, plus the
//!   schema-scoped pool builder used to land tenant inserts.
//! - [`migrations`] — `init-tenancy`, `migrate`, `migrate-registry`,
//!   `migrate-tenants` and the tenant migration report formatter.
//! - [`server`] — `run-server` (thin wrapper around
//!   [`crate::server::run`]).
//! - [`args`] — shared positional / flag parsing helpers.

mod args;
mod migrations;
mod server;
mod tenants;
mod users;

use std::io::{self, Write};
use std::path::Path;

use crate::error::TenancyError;
use crate::pools::TenantPools;

/// Dispatch entrypoint. Recognizes tenancy verbs and delegates the
/// rest to `rustango_migrate::manage::run`.
///
/// `dir` is the migrations directory (same as the underlying
/// rustango-migrate runner). `registry_url` is needed for schema-
/// mode tenant migrations + tenant pool building; supply the same
/// value the registry pool was built from.
///
/// # Errors
/// Either a [`TenancyError`] from a tenancy verb, or a wrapped
/// `rustango_migrate::MigrateError` from the delegated call.
pub async fn run(
    pools: &TenantPools,
    registry_url: &str,
    dir: &Path,
    args: impl IntoIterator<Item = String>,
) -> Result<(), TenancyError> {
    let mut stdout = io::stdout();
    run_with_writer(pools, registry_url, dir, args, &mut stdout).await
}

/// Same as [`run`] but writes user-facing output to `writer` —
/// useful for tests.
///
/// # Errors
/// As [`run`].
pub async fn run_with_writer<W: Write + Send>(
    pools: &TenantPools,
    registry_url: &str,
    dir: &Path,
    args: impl IntoIterator<Item = String>,
    writer: &mut W,
) -> Result<(), TenancyError> {
    let args: Vec<String> = args.into_iter().collect();
    let cmd = args.first().map_or("", String::as_str);

    match cmd {
        "create-tenant" => {
            tenants::create_tenant(pools, registry_url, dir, &args[1..], writer).await
        }
        "drop-tenant" => tenants::drop_tenant(pools, &args[1..], writer).await,
        "purge-tenant" => tenants::purge_tenant(pools, &args[1..], writer).await,
        "list-tenants" => tenants::list_tenants(pools, writer).await,
        "migrate-tenants" => {
            migrations::migrate_tenants_cmd(pools, registry_url, dir, writer).await
        }
        "migrate-registry" => migrations::migrate_registry_cmd(pools, dir, writer).await,
        "create-operator" => users::create_operator_cmd(pools, &args[1..], writer).await,
        "create-user" => users::create_user_cmd(pools, registry_url, &args[1..], writer).await,
        "run-server" | "runserver" => {
            server::run_server_cmd(pools, registry_url, &args[1..], writer).await
        }
        "init-tenancy" => migrations::init_tenancy_cmd(dir, writer),
        // Plain `migrate` is scope-aware here — registry-scoped
        // migrations apply to the registry pool first, then tenant-
        // scoped ones fan out across active orgs. Direct fall-through
        // to `rustango::migrate::manage` (which is scope-blind) would
        // apply tenant migrations to the registry pool, a real
        // footgun. `migrate-registry` / `migrate-tenants` stay
        // available for fine-grained control.
        "migrate" => {
            migrations::migrate_all_cmd(pools, registry_url, dir, &args[1..], writer).await
        }
        // Everything else (makemigrations, showmigrations, downgrade,
        // help, …) is registry-scoped and delegates to the standard
        // single-tenant runner.
        _ => rustango::migrate::manage::run_with_writer(
            pools.registry(),
            dir,
            args,
            writer,
        )
        .await
        .map_err(TenancyError::Migrate),
    }
}
