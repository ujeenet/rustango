//! Tenant provisioning + management subcommands.
//!
//! Composes with `crate::migrate::manage::run` — recognizes the
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
//!     let pools = crate::tenancy::TenantPools::new(pool);
//!     let dir = std::path::Path::new("./migrations");
//!     crate::tenancy::manage::run(
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
//!   [`super::server::run`]).
//! - [`args`] — shared positional / flag parsing helpers.

mod args;
mod audit;
mod migrations;
mod scaffold;
mod server;
mod tenants;
mod users;

/// Typed Rust API for tenancy provisioning — `create_tenant_if_missing`,
/// `create_operator_if_missing`, `create_user_if_missing`, `find_org`.
/// Use these from `Builder::seed_with` closures and other in-process
/// callers; the verb dispatcher [`run_with_writer`] is the CLI surface.
pub mod api;

use std::io::{self, Write};
use std::path::Path;

use super::error::TenancyError;
use super::pools::TenantPools;

/// Dispatch entrypoint. Recognizes tenancy verbs and delegates the
/// rest to `crate::migrate::manage::run`.
///
/// `dir` is the migrations directory (same as the underlying
/// rustango-migrate runner). `registry_url` is needed for schema-
/// mode tenant migrations + tenant pool building; supply the same
/// value the registry pool was built from.
///
/// # Errors
/// Either a [`TenancyError`] from a tenancy verb, or a wrapped
/// `crate::migrate::MigrateError` from the delegated call.
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
        "" | "help" | "--help" | "-h" => {
            write_help(writer)?;
            Ok(())
        }
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
        // Intercepted before fall-through: tenancy ships its own
        // manage.rs template in `--with-manage-bin`, wiring
        // `crate::tenancy::manage::run` instead of the single-tenant
        // dispatcher. Plain models/views/urls files are identical
        // across both crates, so the heavy lifting still runs through
        // `rustango::migrate::scaffold::startapp`.
        "audit-cleanup" => audit::audit_cleanup_cmd(pools, &args[1..], writer).await,
        "startapp" => scaffold::startapp_cmd(&args[1..], writer),
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
        // …) is registry-scoped and delegates to the standard
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

/// Render the curated help text for `cargo run --bin manage` (or
/// equivalent). Covers every tenancy verb with a brief description +
/// the most common flags, then the registry-scoped verbs that fall
/// through to `rustango::migrate::manage`.
///
/// Public so generated `manage.rs` binaries can short-circuit the
/// `help` / `--help` / `-h` / no-args case **before** connecting to
/// Postgres — otherwise `cargo run --bin manage -- help` would error
/// with "missing DATABASE_URL" and the user couldn't see the help.
///
/// # Errors
/// Returns [`TenancyError::Io`] for a failed write — should not happen
/// against `std::io::stdout()`.
pub fn write_help<W: Write>(w: &mut W) -> Result<(), TenancyError> {
    writeln!(w, "rustango manage CLI — tenancy-aware dispatcher")?;
    writeln!(w)?;
    writeln!(w, "USAGE:")?;
    writeln!(w, "  cargo run --bin manage -- <verb> [args]")?;
    writeln!(w)?;
    writeln!(w, "TENANT MANAGEMENT:")?;
    writeln!(
        w,
        "  create-tenant <slug> [--display-name <s>] [--mode schema|database]"
    )?;
    writeln!(
        w,
        "                       [--host-pattern <s>] [--database-url <s>] [--no-migrate]"
    )?;
    writeln!(
        w,
        "                       Provision a new tenant. Schema mode (default) gives the"
    )?;
    writeln!(
        w,
        "                       tenant its own Postgres schema; database mode points at"
    )?;
    writeln!(
        w,
        "                       a fully separate DB via --database-url."
    )?;
    writeln!(
        w,
        "  drop-tenant   <slug> [--confirm <slug>]"
    )?;
    writeln!(
        w,
        "                       Soft-delete (active=false). Data preserved."
    )?;
    writeln!(
        w,
        "  purge-tenant  <slug> [--confirm <slug>] [--purge-database]"
    )?;
    writeln!(
        w,
        "                       HARD-delete: drops schema (or DB with --purge-database)."
    )?;
    writeln!(w, "  list-tenants         Print every Org row in the registry.")?;
    writeln!(w)?;
    writeln!(w, "USER / OPERATOR MANAGEMENT:")?;
    writeln!(
        w,
        "  create-operator <username> --password <p>"
    )?;
    writeln!(
        w,
        "                       Operator-level account; signs into the apex /login."
    )?;
    writeln!(
        w,
        "  create-user <slug> <username> --password <p> [--superuser]"
    )?;
    writeln!(
        w,
        "                       Tenant-scoped user; signs into <slug>.<apex>/__login."
    )?;
    writeln!(w)?;
    writeln!(w, "MIGRATIONS:")?;
    writeln!(
        w,
        "  init-tenancy         Materialize bootstrap migrations into ./migrations/."
    )?;
    writeln!(
        w,
        "  makemigrations       Diff models against latest snapshot, emit a new JSON file."
    )?;
    writeln!(
        w,
        "  migrate              Apply registry-scoped, then tenant-scoped, migrations."
    )?;
    writeln!(
        w,
        "  migrate-registry     Apply registry-scoped migrations only."
    )?;
    writeln!(
        w,
        "  migrate-tenants      Apply tenant-scoped migrations to every active org."
    )?;
    writeln!(
        w,
        "  showmigrations       List which migrations are applied / pending."
    )?;
    writeln!(
        w,
        "  downgrade            Roll back the most recent migration."
    )?;
    writeln!(w)?;
    writeln!(w, "AUDIT:")?;
    writeln!(
        w,
        "  audit-cleanup --days <N>     Delete entries older than N days (all active tenants)."
    )?;
    writeln!(
        w,
        "  audit-cleanup --keep-last <N> Keep N most recent entries per row."
    )?;
    writeln!(
        w,
        "                [--tenant <s>]  Scope to one tenant instead of all."
    )?;
    writeln!(w)?;
    writeln!(w, "SERVER:")?;
    writeln!(
        w,
        "  run-server [--bind <addr>]"
    )?;
    writeln!(
        w,
        "                       Boot the HTTP server with admin + operator console."
    )?;
    writeln!(w)?;
    writeln!(w, "SCAFFOLDING:")?;
    writeln!(
        w,
        "  startapp <name> [--into <dir>] [--with-manage-bin] [--with-bootstrap-migration]"
    )?;
    writeln!(
        w,
        "                       Scaffold a Django-shape app module."
    )?;
    writeln!(w)?;
    writeln!(w, "EXAMPLES:")?;
    writeln!(w, "  cargo run --bin manage -- migrate")?;
    writeln!(w, "  cargo run --bin manage -- create-operator admin --password letmein")?;
    writeln!(w, "  cargo run --bin manage -- create-tenant acme --display-name 'ACME Corp'")?;
    writeln!(w, "  cargo run --bin manage -- create-user acme alice --password hunter2 --superuser")?;
    writeln!(w, "  cargo run --bin manage -- list-tenants")?;
    writeln!(w, "  cargo run --bin manage -- audit-cleanup --days 90")?;
    writeln!(w, "  cargo run --bin manage -- audit-cleanup --keep-last 50 --tenant acme")?;
    writeln!(w)?;
    writeln!(w, "Run any verb with --help for verb-specific flags + details.")?;
    Ok(())
}
