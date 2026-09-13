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

// Agent / skill verbs are MCP-only (see `super::agents`, gated the same way).
#[cfg(feature = "mcp")]
mod agents;
mod args;
mod audit;
#[cfg(feature = "postgres")]
mod migrate_storage;
mod migrations;
mod roles;
mod scaffold;
mod server;
mod tenants;
mod users;
mod wizard;

/// Typed Rust API for tenancy provisioning — `create_tenant_if_missing`,
/// `create_operator_if_missing`, `create_user_if_missing`, `find_org`.
/// Use these from `Builder::seed_with` closures and other in-process
/// callers; the verb dispatcher [`run_with_writer`] is the CLI surface.
pub mod api;

use std::io::{self, Write};
use std::path::Path;

use super::error::TenancyError;
use super::pools::TenantPools;

/// Function pointer for the bootstrap initializer used by the
/// `init-tenancy` verb. Defaults to
/// [`crate::tenancy::bootstrap::init_tenancy`]; override via
/// [`run_with_init`] / [`run_with_writer_and_init`] to swap in a
/// custom [`crate::tenancy::TenantUserModel`] (typically threaded
/// from [`crate::manage::Cli::user_model`]).
pub type InitTenancyFn = fn(&Path) -> Result<super::bootstrap::InitTenancyReport, TenancyError>;

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
pub async fn run<DB: sqlx::Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    dir: &Path,
    args: impl IntoIterator<Item = String>,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let mut stdout = io::stdout();
    run_with_writer(pools, registry_url, dir, args, &mut stdout).await
}

/// Like [`run`] but with a custom bootstrap initializer — wires a
/// [`crate::tenancy::TenantUserModel`] override through to the
/// `init-tenancy` verb. Other verbs ignore `init_fn`.
///
/// # Errors
/// As [`run`].
pub async fn run_with_init<DB: sqlx::Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    dir: &Path,
    args: impl IntoIterator<Item = String>,
    init_fn: InitTenancyFn,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let mut stdout = io::stdout();
    run_with_writer_and_init(pools, registry_url, dir, args, &mut stdout, init_fn).await
}

/// Same as [`run`] but writes user-facing output to `writer` —
/// useful for tests.
///
/// # Errors
/// As [`run`].
pub async fn run_with_writer<W: Write + Send, DB: sqlx::Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    dir: &Path,
    args: impl IntoIterator<Item = String>,
    writer: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    run_with_writer_and_init(
        pools,
        registry_url,
        dir,
        args,
        writer,
        super::bootstrap::init_tenancy,
    )
    .await
}

/// Full-bore variant: custom writer **and** custom bootstrap
/// initializer. The other variants are ergonomic wrappers around
/// this one.
///
/// # Errors
/// As [`run`].
pub async fn run_with_writer_and_init<W: Write + Send, DB: sqlx::Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    dir: &Path,
    args: impl IntoIterator<Item = String>,
    writer: &mut W,
    init_fn: InitTenancyFn,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
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
        "prewarm-pools" => {
            // v0.27.7 (#60) — eagerly build pools for every active
            // database-mode tenant. Useful as a post-deploy hook
            // after credential rotation, or to validate that all
            // tenants are reachable before flipping a load balancer.
            let report = pools.prewarm_database_tenants().await?;
            writeln!(
                writer,
                "prewarm: {} active tenants — warmed {}, failed {}, skipped (cap) {}",
                report.total_active, report.warmed, report.failed, report.skipped_cap
            )?;
            Ok(())
        }
        "migrate-tenants" => {
            migrations::migrate_tenants_cmd(pools, registry_url, dir, writer).await
        }
        "migrate-registry" => migrations::migrate_registry_cmd(pools, dir, writer).await,
        #[cfg(feature = "postgres")]
        "migrate-tenant-storage" => {
            // PG-only: uses pg_dump | psql + schema-mode dispatch.
            // Downcast through `Any` so the generic dispatcher path
            // can still compile on multi-backend builds.
            let pg_pools = (pools as &dyn std::any::Any)
                .downcast_ref::<TenantPools<sqlx::Postgres>>()
                .ok_or_else(|| {
                    TenancyError::Validation(
                        "migrate-tenant-storage is Postgres-only (uses pg_dump | psql + \
                         schema-mode dispatch). Re-run with a postgres:// DATABASE_URL."
                            .into(),
                    )
                })?;
            migrate_storage::migrate_tenant_storage_cmd(pg_pools, registry_url, &args[1..], writer)
                .await
        }
        #[cfg(not(feature = "postgres"))]
        "migrate-tenant-storage" => Err(TenancyError::Validation(
            "migrate-tenant-storage requires the `postgres` feature (uses pg_dump | psql + \
             schema-mode is PG-only)"
                .into(),
        )),
        "create-operator" => users::create_operator_cmd(pools, &args[1..], writer).await,
        "create-user" => users::create_user_cmd(pools, registry_url, &args[1..], writer).await,
        "create-superuser" => {
            users::create_superuser_cmd(pools, registry_url, &args[1..], writer).await
        }
        "set-superuser" => users::set_superuser_cmd(pools, registry_url, &args[1..], writer).await,
        "reset-password" => {
            users::reset_password_cmd(pools, registry_url, &args[1..], writer).await
        }
        "reset-operator-password" => {
            users::reset_operator_password_cmd(pools, &args[1..], writer).await
        }
        "change-password" => {
            users::change_password_cmd(pools, registry_url, &args[1..], writer).await
        }
        "change-operator-password" => {
            users::change_operator_password_cmd(pools, &args[1..], writer).await
        }
        "run-server" | "runserver" => {
            server::run_server_cmd(pools, registry_url, &args[1..], writer).await
        }
        "init-tenancy" => migrations::init_tenancy_cmd_with(dir, writer, init_fn),
        "wizard" | "init" => {
            // Interactive setup wizard (roadmap #2, v0.30.14).
            // Reads from stdin, writes to the same writer the
            // dispatcher uses. `init` is an alias the muscle
            // memory of `cargo init` users will reach for.
            let stdin = std::io::stdin();
            let mut reader = stdin.lock();
            wizard::wizard_cmd(pools, registry_url, dir, init_fn, &mut reader, writer).await
        }
        // Intercepted before fall-through: tenancy ships its own
        // manage.rs template in `--with-manage-bin`, wiring
        // `crate::tenancy::manage::run` instead of the single-tenant
        // dispatcher. Plain models/views/urls files are identical
        // across both crates, so the heavy lifting still runs through
        // `rustango::migrate::scaffold::startapp`.
        "audit-cleanup" => audit::audit_cleanup_cmd(pools, &args[1..], writer).await,
        "create-role" => roles::create_role_cmd(pools, &args[1..], writer).await,
        "list-roles" => roles::list_roles_cmd(pools, &args[1..], writer).await,
        "assign-role" => roles::assign_role_cmd(pools, &args[1..], writer).await,
        "revoke-role" => roles::revoke_role_cmd(pools, &args[1..], writer).await,
        "grant-perm" => roles::grant_perm_cmd(pools, &args[1..], writer).await,
        "revoke-perm" => roles::revoke_perm_cmd(pools, &args[1..], writer).await,
        "create-api-key" => roles::create_api_key_cmd(pools, &args[1..], writer).await,
        #[cfg(feature = "mcp")]
        "create-agent" => agents::create_agent_cmd(pools, registry_url, &args[1..], writer).await,
        #[cfg(feature = "mcp")]
        "rotate-agent-secret" => {
            agents::rotate_agent_secret_cmd(pools, registry_url, &args[1..], writer).await
        }
        #[cfg(feature = "mcp")]
        "list-agents" => agents::list_agents_cmd(pools, registry_url, &args[1..], writer).await,
        #[cfg(feature = "mcp")]
        "create-skill" => agents::create_skill_cmd(pools, registry_url, &args[1..], writer).await,
        #[cfg(feature = "mcp")]
        "grant-skill" => agents::grant_skill_cmd(pools, registry_url, &args[1..], writer).await,
        #[cfg(feature = "mcp")]
        "revoke-skill" => agents::revoke_skill_cmd(pools, registry_url, &args[1..], writer).await,
        #[cfg(feature = "mcp")]
        "list-skills" => agents::list_skills_cmd(pools, registry_url, &args[1..], writer).await,
        #[cfg(feature = "mcp")]
        "create-user-key" => {
            agents::create_user_key_cmd(pools, registry_url, &args[1..], writer).await
        }
        #[cfg(feature = "mcp")]
        "list-user-keys" => {
            agents::list_user_keys_cmd(pools, registry_url, &args[1..], writer).await
        }
        #[cfg(feature = "mcp")]
        "revoke-user-key" => {
            agents::revoke_user_key_cmd(pools, registry_url, &args[1..], writer).await
        }
        #[cfg(feature = "mcp")]
        "map-skill-permission" => {
            agents::map_skill_permission_cmd(pools, registry_url, &args[1..], writer).await
        }
        #[cfg(feature = "mcp")]
        "unmap-skill-permission" => {
            agents::unmap_skill_permission_cmd(pools, registry_url, &args[1..], writer).await
        }
        "seed-permissions" => roles::seed_permissions_cmd(pools, &args[1..], writer).await,
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
        // v0.38 — `manage::run_with_writer` now accepts `&crate::sql::Pool`
        // (instead of `&PgPool`), so we route through the tri-dialect
        // `registry_pool()` accessor that returns the backend-agnostic
        // enum form.
        _ => rustango::migrate::manage::run_with_writer(&pools.registry_pool(), dir, args, writer)
            .await
            .map_err(TenancyError::Migrate),
    }
}

/// Render the curated help text for `cargo run` (or
/// equivalent). Covers every tenancy verb with a brief description +
/// the most common flags, then the registry-scoped verbs that fall
/// through to `rustango::migrate::manage`.
///
/// Public so generated `manage.rs` binaries can short-circuit the
/// `help` / `--help` / `-h` / no-args case **before** connecting to
/// Postgres — otherwise `cargo run -- help` would error
/// with "missing DATABASE_URL" and the user couldn't see the help.
///
/// # Errors
/// Returns [`TenancyError::Io`] for a failed write — should not happen
/// against `std::io::stdout()`.
pub fn write_help<W: Write>(w: &mut W) -> Result<(), TenancyError> {
    writeln!(w, "rustango manage CLI — tenancy-aware dispatcher")?;
    writeln!(w)?;
    writeln!(w, "USAGE:")?;
    writeln!(w, "  cargo run -- <verb> [args]")?;
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
    writeln!(w, "  drop-tenant   <slug> [--confirm <slug>]")?;
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
    writeln!(
        w,
        "  list-tenants         Print every Org row in the registry."
    )?;
    writeln!(w)?;
    writeln!(w, "USER / OPERATOR MANAGEMENT:")?;
    writeln!(
        w,
        "  create-operator <username> [--password <p> | --generate]"
    )?;
    writeln!(
        w,
        "                       Operator-level account; signs into the apex /login."
    )?;
    writeln!(
        w,
        "  create-user <slug> <username> [--password <p> | --generate] [--superuser]"
    )?;
    writeln!(
        w,
        "                       Tenant-scoped user; signs into <slug>.<apex>/__login."
    )?;
    writeln!(w, "  create-superuser <slug> <username> [--password <p>]")?;
    writeln!(
        w,
        "                       Tenant-scoped superuser (alias for create-user --superuser)."
    )?;
    writeln!(w, "  set-superuser <slug> <username> [--on|--off]")?;
    writeln!(
        w,
        "                       Promote / demote an existing tenant user."
    )?;
    writeln!(
        w,
        "  reset-password <slug> <username> [--password <p> | --generate]"
    )?;
    writeln!(
        w,
        "                       Reset a tenant user's password without their old one."
    )?;
    writeln!(
        w,
        "  reset-operator-password <username> [--password <p> | --generate]"
    )?;
    writeln!(
        w,
        "                       Reset an operator's password without their old one."
    )?;
    writeln!(
        w,
        "  change-password <slug> <username> [--current <p>] [--password <p> | --generate]"
    )?;
    writeln!(
        w,
        "                       Rotate a tenant user's password; verifies current first."
    )?;
    writeln!(
        w,
        "  change-operator-password <username> [--current <p>] [--password <p> | --generate]"
    )?;
    writeln!(
        w,
        "                       Rotate an operator's password; verifies current first."
    )?;
    writeln!(w)?;
    writeln!(w, "QUICK START:")?;
    writeln!(
        w,
        "  wizard | init        Interactive setup — walks you from a fresh project to"
    )?;
    writeln!(
        w,
        "                       a working tenant + operator + tenant superuser through"
    )?;
    writeln!(
        w,
        "                       prompts. Each step is opt-in (press n to skip)."
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
        "  migrate --fake <name>  Insert <name> into registry ledger WITHOUT running its SQL"
    )?;
    writeln!(
        w,
        "                         (recovery: \"relation X already exists\" 42P07 errors)."
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
        "  prewarm-pools        Eagerly build pools for every active database-mode tenant"
    )?;
    writeln!(
        w,
        "                       (drops first-request latency on the hot path)."
    )?;
    writeln!(
        w,
        "  migrate-tenant-storage <slug> --to schema|database [--database-url <s>] [--schema-name <s>] [--dry-run]"
    )?;
    writeln!(
        w,
        "                       Move a tenant between storage modes via pg_dump → psql."
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
    writeln!(w, "ROLES & PERMISSIONS:")?;
    writeln!(w, "  create-role <slug> <name> [--description <s>]")?;
    writeln!(w, "                       Create a named role on a tenant.")?;
    writeln!(
        w,
        "  list-roles  <slug>   List roles and permission counts."
    )?;
    writeln!(w, "  assign-role <slug> <username> <role>")?;
    writeln!(w, "                       Add a user to a role.")?;
    writeln!(w, "  revoke-role <slug> <username> <role>")?;
    writeln!(w, "                       Remove a user from a role.")?;
    writeln!(w, "  grant-perm  <slug> <name> <codename> [--role]")?;
    writeln!(
        w,
        "                       Grant a codename to a user (default) or role (--role)."
    )?;
    writeln!(w, "  revoke-perm <slug> <name> <codename> [--role]")?;
    writeln!(
        w,
        "                       Revoke a codename from a user or role."
    )?;
    writeln!(
        w,
        "  create-api-key <slug> <username> [--label <s>] [--expires-days <N>]"
    )?;
    writeln!(
        w,
        "                       Issue a Bearer API key for a tenant user."
    )?;
    // MCP agent/skill verbs only exist when the `mcp` feature is compiled in.
    #[cfg(feature = "mcp")]
    {
        writeln!(w, "  create-agent <slug> <name>")?;
        writeln!(
            w,
            "                       Provision an MCP agent (prints its secret once)."
        )?;
        writeln!(w, "  rotate-agent-secret <slug> <name>")?;
        writeln!(
            w,
            "                       Issue a fresh secret for an MCP agent."
        )?;
        writeln!(w, "  list-agents <slug>   List a tenant's MCP agents.")?;
        writeln!(
            w,
            "  create-skill <slug> <codename> [--name ..] [--tools a,b] [--instructions ..]"
        )?;
        writeln!(
            w,
            "                       Define an MCP skill (a bundle of tools)."
        )?;
        writeln!(w, "  grant-skill <slug> <agent> <skill>")?;
        writeln!(w, "                       Grant a skill to an agent.")?;
        writeln!(w, "  revoke-skill <slug> <agent> <skill>")?;
        writeln!(w, "                       Revoke a skill from an agent.")?;
        writeln!(w, "  list-skills <slug>   List a tenant's MCP skills.")?;
        writeln!(w, "  create-user-key <slug> <username> [label]")?;
        writeln!(
            w,
            "                       Issue a personal, user-owned MCP key (prints its token once)."
        )?;
        writeln!(w, "  list-user-keys <slug> <username>")?;
        writeln!(w, "                       List a user's personal MCP keys.")?;
        writeln!(w, "  revoke-user-key <slug> <username> <key_id>")?;
        writeln!(
            w,
            "                       Revoke one of a user's personal keys by id."
        )?;
        writeln!(w, "  map-skill-permission <slug> <skill> <permission>")?;
        writeln!(
            w,
            "                       Grant a skill to any user holding <permission> (user keys)."
        )?;
        writeln!(w, "  unmap-skill-permission <slug> <skill> <permission>")?;
        writeln!(
            w,
            "                       Remove a skill↔permission mapping."
        )?;
    }
    writeln!(w, "  seed-permissions [--slug <s>]")?;
    writeln!(
        w,
        "                       Re-seed `rustango_permissions` for one (--slug)"
    )?;
    writeln!(
        w,
        "                       or every active tenant. Idempotent — safe to"
    )?;
    writeln!(
        w,
        "                       run after adding `#[rustango(permissions)]` to"
    )?;
    writeln!(
        w,
        "                       a model without a fresh migrate cycle."
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
    writeln!(w, "  run-server [--bind <addr>]")?;
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
    writeln!(w, "  cargo run -- migrate")?;
    writeln!(w, "  cargo run -- create-operator admin --password letmein")?;
    writeln!(
        w,
        "  cargo run -- create-tenant acme --display-name 'ACME Corp'"
    )?;
    writeln!(
        w,
        "  cargo run -- create-user acme alice --password hunter2 --superuser"
    )?;
    writeln!(w, "  cargo run -- list-tenants")?;
    writeln!(w, "  cargo run -- audit-cleanup --days 90")?;
    writeln!(
        w,
        "  cargo run -- audit-cleanup --keep-last 50 --tenant acme"
    )?;
    writeln!(w)?;
    writeln!(
        w,
        "Run any verb with --help for verb-specific flags + details."
    )?;
    Ok(())
}
