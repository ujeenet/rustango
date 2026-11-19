//! The `manage` command runner for rustango projects.
//!
//! [`run`] takes `argv` and dispatches to the right migration
//! function. Users drop a tiny `src/bin/manage.rs` binary into their
//! own project that imports their `#[derive(Model)]` structs (so
//! `inventory` registers them) and forwards argv to this runner:
//!
//! ```ignore
//! use rustango::sql::sqlx::PgPool;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Bring user models into this binary so inventory sees them.
//!     use my_app::models::*;
//!
//!     let pool = PgPool::connect(&std::env::var("DATABASE_URL")?).await?;
//!     let dir: &std::path::Path = "./migrations".as_ref();
//!     rustango::manage::run(&pool, dir, std::env::args().skip(1)).await?;
//!     Ok(())
//! }
//! ```
//!
//! UX: `cargo run -- migrate`,
//! `cargo run -- makemigrations [name]`, etc. The
//! framework owns the dispatcher; the user owns the entrypoint
//! (which must compile in their models). The split exists because
//! Rust links models per binary.
//!
//! ## Subcommands
//!
//! | command | what it does |
//! |---------|--------------|
//! | `makemigrations [name]` | Diff registry vs latest snapshot; write next file. |
//! | `makemigrations --empty <name>` | Write an empty scaffold for hand-authored data migrations. |
//! | `migrate` | Apply every pending migration. |
//! | `migrate <target>` | Forward-or-back to `<target>`. `zero` wipes everything. |
//! | `downgrade [N]` | Step back N applied migrations (default 1). |
//! | `showmigrations` / `status` | List migrations with `[X]`/`[ ]` applied marker. |
//! | `--help` / `-h` / `help` | Print usage. |

use std::io::Write;
use std::path::Path;

use crate::core::inventory;
use crate::sql::Pool;

use super::ensure::{bootstrap_tables, BootstrapHolder};
use super::error::MigrateError;
use super::file::{self, DataOp, Migration, Operation};
use super::runner;
use super::snapshot::SchemaSnapshot;
use crate::sql::connect_diagnosis::redact as redact_url;

#[cfg(feature = "cache")]
mod cache;
mod check;
mod db;
mod deploy_audit;
mod email;
mod fixtures;
mod flush;
mod generate;
mod info;
mod makemigrations;
mod migrate;
mod prune;
mod show;
mod showmigrations;
mod startapp;

#[cfg(feature = "cache")]
use cache::*;
pub(crate) use check::*;
use db::*;
pub use deploy_audit::*;
use email::*;
pub(crate) use fixtures::*;
pub(crate) use flush::*;
pub(in crate::migrate) use generate::*;
use info::*;
pub use makemigrations::*;
pub(crate) use migrate::*;
use prune::*;
use show::*;
pub(crate) use showmigrations::*;
use startapp::*;

/// Parse argv (no binary name) and dispatch to the right subcommand.
/// All output is written to stdout. Use [`run_with_writer`] when you
/// need to capture the output (tests, structured logging, custom UIs).
///
/// `dir` is the migrations directory (e.g. `./migrations`).
///
/// # Errors
/// Returns whatever the underlying migration function returns, plus
/// [`MigrateError::Validation`] for unknown subcommands or bad argv,
/// or [`MigrateError::Io`] if writing to stdout fails (broken pipe).
pub async fn run(
    pool: &Pool,
    dir: &Path,
    args: impl IntoIterator<Item = String>,
) -> Result<(), MigrateError> {
    let mut stdout = std::io::stdout();
    run_with_writer(pool, dir, args, &mut stdout).await
}

/// Dispatch the verbs that need **no database at all**, before any pool is
/// built. Returns `None` when `args` names a verb that does need one, so the
/// caller falls through to [`run`].
///
/// Exists because `Cli::dispatch` used to construct a pool for every verb,
/// including `--help`. `Pool::connect_lazy` doesn't connect, but it *does*
/// validate the URL scheme against the enabled backend features — so a project
/// built for SQLite with a stale `postgres://` in the environment failed to
/// print its own help text (#1216):
///
/// ```text
/// $ DATABASE_URL=postgres://… cargo run --features sqlite -- --help
/// Error: URL scheme `postgres` requires the `postgres` Cargo feature
/// ```
///
/// That bites exactly when help is most wanted — you changed backend and want
/// the verb list. None of these verbs read or write a database, so none of them
/// should care what `DATABASE_URL` says, or whether it is set at all.
///
/// # Errors
/// Whatever the dispatched verb returns.
pub fn run_pool_free<W: Write>(
    args: &[String],
    writer: &mut W,
) -> Option<Result<(), MigrateError>> {
    let cmd = args.first().map_or("", String::as_str);
    Some(match cmd {
        "" | "--help" | "-h" | "help" => print_help(writer).map_err(Into::into),
        "version" | "--version" => version_cmd(writer),
        "docs" => docs_cmd(writer),
        "db:info" => db_info_cmd(writer),
        // Code generators: they read the model registry and write files.
        "startapp" => startapp(&args[1..], writer),
        c if MAKE_VERBS.contains(&c) => make_cmd(c, &args[1..], writer, compiled_feature),
        "showurls" => showurls_cmd(&args[1..], writer),
        "showmodels" => showmodels_cmd(&args[1..], writer),
        "check" if args[1..].iter().any(|a| a == "--settings") => {
            check_settings_cmd(&args[1..], writer)
        }
        _ => return None,
    })
}

/// Same as [`run`] but writes user-facing output to `writer`. Useful
/// for tests (`Vec<u8>`), captured logs, or piping the dispatcher's
/// output through a custom formatter.
///
/// # Errors
/// As [`run`] — including [`MigrateError::Io`] from any failed
/// `writer.write` (the writer's surface).
pub async fn run_with_writer<W: Write + Send>(
    pool: &Pool,
    dir: &Path,
    args: impl IntoIterator<Item = String>,
    writer: &mut W,
) -> Result<(), MigrateError> {
    run_with_writer_for(
        pool,
        dir,
        args,
        writer,
        &[crate::cli_verbs::SINGLE_DB_VERBS],
    )
    .await
}

/// [`run_with_writer`], hinting an unknown verb from `verbs`: the tenancy
/// CLI falls through here with its own verbs too.
pub(crate) async fn run_with_writer_for<W: Write + Send>(
    pool: &Pool,
    dir: &Path,
    args: impl IntoIterator<Item = String>,
    writer: &mut W,
    verbs: &[&[&str]],
) -> Result<(), MigrateError> {
    let args: Vec<String> = args.into_iter().collect();
    let cmd = args.first().map_or("", String::as_str);

    match cmd {
        "" | "--help" | "-h" | "help" => {
            print_help(writer)?;
            Ok(())
        }
        "makemigrations" => makemigrations(dir, &args[1..], writer),
        "migrate" => migrate(pool, dir, &args[1..], writer).await,
        "downgrade" => downgrade(pool, dir, &args[1..], writer).await,
        "showmigrations" | "status" => showmigrations(pool, dir, writer).await,
        "sqlmigrate" => sqlmigrate_cmd(pool, dir, &args[1..], writer),
        "forget-pending" => forget_pending_cmd(pool, dir, &args[1..], writer).await,
        "startapp" => startapp(&args[1..], writer),
        "add-data-op" => add_data_op_cmd(dir, &args[1..], writer),
        c if MAKE_VERBS.contains(&c) => make_cmd(c, &args[1..], writer, compiled_feature),
        "about" => about_cmd(pool, writer).await,
        "check" => check_cmd(pool, dir, &args[1..], writer).await,
        "docs" => docs_cmd(writer),
        "version" | "--version" => version_cmd(writer),
        "db:dump" => db_dump_cmd(&args[1..]),
        "db:restore" => db_restore_cmd(&args[1..], writer),
        "db:info" => db_info_cmd(writer),
        "dumpdata" => dumpdata_cmd(pool, &args[1..], DataScope::All, writer).await,
        "loaddata" => loaddata_cmd(pool, &args[1..], DataScope::All, writer).await,
        "showurls" => showurls_cmd(&args[1..], writer),
        "showmodels" => showmodels_cmd(&args[1..], writer),
        // #253 slice B — bootstrap an AdminUser row for projects
        // using `admin::Builder::with_session_auth`. Only compiled
        // when the `admin` feature is on; without it, fall through
        // to the unknown-subcommand error.
        #[cfg(feature = "admin")]
        "create-admin" => crate::admin::create_admin_cmd(pool, &args[1..], writer).await,
        #[cfg(feature = "admin")]
        "reset-password" => crate::admin::reset_password_cmd(pool, &args[1..], writer).await,
        #[cfg(feature = "admin")]
        "change-password" => crate::admin::change_password_cmd(pool, &args[1..], writer).await,
        #[cfg(feature = "admin")]
        "set-admin-active" => crate::admin::set_admin_active_cmd(pool, &args[1..], writer).await,
        #[cfg(all(feature = "admin", feature = "totp"))]
        "reset-admin-totp" => crate::admin::reset_admin_totp_cmd(pool, &args[1..], writer).await,
        "flush" => flush_cmd(pool, &args[1..], FlushScope::All, writer).await,
        // #822 — bulk pruning of stale rows from `Prunable` models.
        "prune" => prune_cmd(pool, &args[1..], writer).await,
        "audit-cleanup" => audit_cleanup_cmd(pool, &args[1..], writer).await,
        // Purges expired entries from a DatabaseCache-backed
        // table. Works for any
        // table written by `cache::DatabaseCache`, including the
        // sessions-backend table.
        #[cfg(feature = "cache")]
        verb @ ("clear-cache" | "clearsessions") => {
            clear_cache_cmd(pool, verb, &args[1..], writer).await
        }
        // Idempotent `CREATE TABLE IF NOT EXISTS` for DatabaseCache.
        #[cfg(feature = "cache")]
        "createcachetable" | "create-cache-table" => {
            createcachetable_cmd(pool, &args[1..], writer).await
        }
        #[cfg(not(feature = "cache"))]
        verb @ ("clear-cache" | "clearsessions" | "createcachetable" | "create-cache-table") => {
            Err(MigrateError::Validation(format!(
                "`{verb}` needs the `cache` feature on rustango"
            )))
        }
        "sendtestemail" => sendtestemail_cmd(&args[1..], writer).await,
        // v0.38 — `inspectdb` is tri-dialect: PG + MySQL via
        // `information_schema`, SQLite via `PRAGMA table_info` +
        // `sqlite_master`. Dispatch happens inside `inspectdb_cmd`.
        "inspectdb" => super::inspectdb::inspectdb_cmd(pool, &args[1..], writer).await,
        other => Err(MigrateError::Validation(crate::cli_verbs::unknown_verb(
            other, verbs,
        ))),
    }
}

fn print_help<W: Write>(w: &mut W) -> std::io::Result<()> {
    writeln!(w, "rustango::manage — migration and admin command runner\n")?;
    writeln!(w, "USAGE:")?;
    writeln!(w, "  manage <COMMAND> [args]\n")?;
    writeln!(w, "COMMANDS:")?;
    writeln!(w, "  makemigrations [name]")?;
    writeln!(
        w,
        "      Diff the inventory registry against the latest snapshot"
    )?;
    writeln!(
        w,
        "      and write the next migration file. `name` overrides the"
    )?;
    writeln!(w, "      auto-derived suffix.\n")?;
    writeln!(w, "  makemigrations --empty <name>")?;
    writeln!(
        w,
        "      Write an empty migration scaffold (`forward: []`) for"
    )?;
    writeln!(
        w,
        "      hand-authored data migrations. Edit the JSON to add"
    )?;
    writeln!(w, "      `data` ops with sql + reverse_sql.\n")?;
    writeln!(w, "  migrate")?;
    writeln!(w, "      Apply every pending migration in lex order.\n")?;
    writeln!(w, "  migrate <target>")?;
    writeln!(
        w,
        "      Forward or back to <target>. `zero` unapplies every"
    )?;
    writeln!(w, "      applied migration.\n")?;
    writeln!(w, "  migrate --dry-run")?;
    writeln!(
        w,
        "      Print the SQL each pending migration would run; never"
    )?;
    writeln!(
        w,
        "      writes. Reads the ledger so the preview is accurate.\n"
    )?;
    writeln!(w, "  migrate --squash")?;
    writeln!(
        w,
        "      Delete every pending (un-applied) migration JSON and"
    )?;
    writeln!(
        w,
        "      regenerate a single fresh diff via makemigrations. Dev-"
    )?;
    writeln!(
        w,
        "      iteration escape hatch when an evolving model produces a"
    )?;
    writeln!(
        w,
        "      pending migration the validator rejects (e.g. AddColumn"
    )?;
    writeln!(
        w,
        "      NOT NULL with no default). Refuses to touch applied rows.\n"
    )?;
    writeln!(w, "  downgrade [N]")?;
    writeln!(w, "      Step back N applied migrations (default 1).\n")?;
    writeln!(w, "  showmigrations | status")?;
    writeln!(w, "      List migrations with [X]/[ ] applied marker.\n")?;
    writeln!(w, "  sqlmigrate <name>")?;
    writeln!(
        w,
        "      Print the SQL the named migration would emit when applied.\n"
    )?;
    writeln!(w, "  forget-pending <name>")?;
    writeln!(
        w,
        "      Delete a migration JSON that has NOT been applied yet,"
    )?;
    writeln!(
        w,
        "      so the next `makemigrations` regenerates the diff."
    )?;
    writeln!(
        w,
        "      Refuses if the migration is recorded in the ledger.\n"
    )?;
    writeln!(
        w,
        "  add-data-op --sql <SQL> [--reverse-sql <SQL>] [--name <name>] [--to <migration> | --scope <s>]"
    )?;
    writeln!(
        w,
        "      Add a data transformation op (up + optional down)."
    )?;
    writeln!(w, "      --sql        Forward SQL to run (required).")?;
    writeln!(
        w,
        "      --reverse-sql  Rollback SQL. Omit for irreversible ops."
    )?;
    writeln!(
        w,
        "      --name       Name suffix for the new migration file."
    )?;
    writeln!(
        w,
        "      --to         Append to an existing migration instead of creating one.\n"
    )?;
    writeln!(w, "  about")?;
    writeln!(w, "      Print framework version, registered models/apps,")?;
    writeln!(w, "      and detected backend configuration.\n")?;
    writeln!(w, "  check [--deploy | --settings]")?;
    writeln!(
        w,
        "      Run system audits — pending migrations, missing models, common"
    )?;
    writeln!(
        w,
        "      misconfigurations. With --deploy: production hardening checks."
    )?;
    writeln!(
        w,
        "      With --settings: where each setting comes from, reading config/\n      for the RUSTANGO_ENV tier (no database)."
    )?;
    writeln!(w, "      Exits non-zero on any error-level finding.\n")?;
    writeln!(w, "  flush [--yes] [--app <label>] [--model <name>]")?;
    writeln!(
        w,
        "      Wipe all rows from registered model tables. Schema + migrations"
    )?;
    writeln!(
        w,
        "      ledger stay intact. Without --yes, prints the planned"
    )?;
    writeln!(w, "      action and exits (no DB write).\n")?;
    writeln!(w, "  prune [--model <name>] [--except <name>] [--pretend]")?;
    writeln!(w, "      Bulk-delete stale rows from every Prunable model.")?;
    writeln!(
        w,
        "      --pretend counts matches without deleting; --model / --except"
    )?;
    writeln!(w, "      narrow the scope (repeatable).\n")?;
    #[cfg(feature = "admin")]
    {
        writeln!(
            w,
            "  create-admin <username> [--password-stdin | --generate] [--superuser]"
        )?;
        writeln!(
            w,
            "      Create an admin user; prompts for the password by default.\n"
        )?;
        writeln!(
            w,
            "  reset-password <username> | change-password <username>"
        )?;
        writeln!(
            w,
            "      Set an admin user's password; change-password checks the old one.\n"
        )?;
        writeln!(w, "  set-admin-active <username> --on|--off")?;
        writeln!(
            w,
            "      Turn an admin user off (ends their sessions) or back on.\n"
        )?;
    }
    #[cfg(all(feature = "admin", feature = "totp"))]
    {
        writeln!(w, "  reset-admin-totp <username>")?;
        writeln!(
            w,
            "      Remove an admin user's two-factor device (lost phone).\n"
        )?;
    }
    writeln!(w, "  audit-cleanup [--days <N> | --keep-last <N>] [--yes]")?;
    writeln!(
        w,
        "      Trim the audit log: entries older than N days, or all but the"
    )?;
    writeln!(
        w,
        "      N newest per row. N <= 0 deletes everything and needs --yes.\n"
    )?;
    #[cfg(feature = "cache")]
    {
        writeln!(w, "  clear-cache [--table <name>]")?;
        writeln!(w, "      (alias: clearsessions)")?;
        writeln!(
            w,
            "      Purge expired rows from a DatabaseCache-backed table."
        )?;
        writeln!(w, "      Default table: rustango_cache.\n")?;
        writeln!(w, "  createcachetable [--table <name>]")?;
        writeln!(w, "      (alias: create-cache-table)")?;
        writeln!(
            w,
            "      Idempotent CREATE TABLE IF NOT EXISTS for DatabaseCache."
        )?;
        writeln!(w, "      Default table: rustango_cache. Safe to re-run.\n")?;
    }
    writeln!(
        w,
        "  sendtestemail --to <addr> [--from <addr>] [--subject <text>]"
    )?;
    writeln!(
        w,
        "      Send a test message through the [mail] backend (console / memory /"
    )?;
    writeln!(
        w,
        "      null / smtp). Use to verify SMTP credentials before deploy.\n"
    )?;
    writeln!(w, "  showmodels [--format plain|json] [--app <label>]")?;
    writeln!(
        w,
        "      Print every model registered via #[derive(Model)] + inventory."
    )?;
    writeln!(
        w,
        "      Confirms registration / debugs missing-model surprises.\n"
    )?;
    writeln!(w, "  showurls [--format plain|json]")?;
    writeln!(
        w,
        "      Print every named URL pattern registered via `register_url!`."
    )?;
    writeln!(
        w,
        "      Useful for debugging routing + auditing `{{% url %}}` references.\n"
    )?;
    writeln!(w, "  loaddata <fixture.json> [--fail-fast]")?;
    writeln!(
        w,
        "      Insert every row in the JSON fixture. Default skips failing"
    )?;
    writeln!(
        w,
        "      rows with a warning; --fail-fast aborts on the first failure.\n"
    )?;
    writeln!(
        w,
        "  dumpdata [--model <name>] [--exclude <name>] [--indent <N>] [--output <file>]"
    )?;
    writeln!(w, "      Export every registered model's rows as a JSON")?;
    writeln!(
        w,
        "      fixture (`[{{\"model\": \"app.Model\", \"pk\": N, \"fields\": {{...}}}}]`)."
    )?;
    writeln!(
        w,
        "      --model limits to a single model; pass multiple times for a set.\n"
    )?;
    writeln!(w, "  docs")?;
    writeln!(
        w,
        "      Open docs.rs/rustango in the default browser. Set \
         RUSTANGO_NO_BROWSER to just print the URL.\n"
    )?;
    writeln!(w, "  version | --version")?;
    writeln!(w, "      Print the rustango framework version.\n")?;
    writeln!(
        w,
        "  (To bootstrap a new project from scratch, install + run"
    )?;
    writeln!(
        w,
        "  `cargo install cargo-rustango` then `cargo rustango new <name>`.)\n"
    )?;
    writeln!(
        w,
        "  make:viewset <Name> [--model <Model>] [--tenant | --no-tenant]"
    )?;
    writeln!(
        w,
        "    --tenant emits a `ViewSet::for_model(...).tenant_router(...)`"
    )?;
    writeln!(
        w,
        "    shape that resolves a per-request connection via `Tenant`"
    )?;
    writeln!(
        w,
        "    instead of baking a pool at mount time (required for tenancy)."
    )?;
    writeln!(
        w,
        "    Auto-detected from Cargo.toml when the rustango dep enables"
    )?;
    writeln!(
        w,
        "    `tenancy` — pass `--no-tenant` to override the auto-detection."
    )?;
    writeln!(w, "  make:api_routes <app> [--tenant]")?;
    writeln!(
        w,
        "    Scaffold src/<app>/api_routes.rs — the per-app composer that"
    )?;
    writeln!(
        w,
        "    merges every viewset's router into a single Router<()>."
    )?;
    writeln!(w, "  make:serializer <Name> [--model <Model>]")?;
    writeln!(w, "  make:form <Name>")?;
    writeln!(w, "  make:job <Name>")?;
    writeln!(w, "  make:scheduled <Name>")?;
    writeln!(w, "  make:worker <Name>")?;
    writeln!(w, "  make:notification <Name>")?;
    writeln!(w, "  make:middleware <Name>")?;
    writeln!(w, "  make:test <Name>")?;
    writeln!(
        w,
        "      Scaffold a single source file with the chosen shape."
    )?;
    writeln!(
        w,
        "      Writes to src/<snake_name>.rs (skips if exists).\n"
    )?;
    writeln!(
        w,
        "  db:dump [--out <file>] [--data-only|--schema-only] [--no-owner]"
    )?;
    writeln!(
        w,
        "      Run pg_dump against $DATABASE_URL. Default: prints SQL to"
    )?;
    writeln!(
        w,
        "      stdout (omit --out to pipe). --data-only / --schema-only"
    )?;
    writeln!(
        w,
        "      mirror pg_dump's flags. --no-owner skips OWNER lines.\n"
    )?;
    writeln!(w, "  db:restore <file> [--clean --yes]")?;
    writeln!(
        w,
        "      Run psql against $DATABASE_URL with `\\i <file>`, in one transaction. With"
    )?;
    writeln!(
        w,
        "      --clean, prepend a `DROP SCHEMA public CASCADE; CREATE SCHEMA public;`"
    )?;
    writeln!(
        w,
        "      so the restore lands on a clean database; --yes confirms the drop.\n"
    )?;
    writeln!(w, "  db:info")?;
    writeln!(
        w,
        "      Print the resolved DB URL (password redacted), detected"
    )?;
    writeln!(w, "      backend, and which backend Cargo features are")?;
    writeln!(w, "      compiled in. Read-only — does not connect.\n")?;
    writeln!(w, "  inspectdb [--schema <name>] [--table <name>]")?;
    writeln!(
        w,
        "      Connect to DATABASE_URL and emit `#[derive(Model)]`"
    )?;
    writeln!(
        w,
        "      source for every base table in `--schema` (default `public`)."
    )?;
    writeln!(w, "      Pipe to a file the user reviews + edits. Adopts")?;
    writeln!(w, "      rustango against an existing database")?;
    writeln!(w, "      without rewriting it.\n")?;
    writeln!(w, "  startapp <name> [--with-manage-bin]")?;
    writeln!(w, "      Scaffold an app module under src/<name>/")?;
    writeln!(
        w,
        "      (models.rs + views.rs + urls.rs + mod.rs). Idempotent;"
    )?;
    writeln!(
        w,
        "      existing files are left untouched. With --with-manage-bin,"
    )?;
    writeln!(w, "      also writes src/bin/manage.rs.")?;
    Ok(())
}

/// A flag this command does not know.
fn unknown_flag(flag: &str) -> MigrateError {
    MigrateError::Validation(format!("unknown flag: {flag}"))
}

/// A positional argument this command does not take.
fn unexpected_positional(arg: &str) -> MigrateError {
    MigrateError::Validation(format!("unexpected positional argument: {arg}"))
}

/// A flag given as the last argument, without its value.
fn missing_value(flag: &str) -> MigrateError {
    MigrateError::Validation(format!("{flag} requires a value"))
}

/// Scaffolders never overwrite a file the user may have edited.
fn refuse_overwrite(path: &Path) -> Result<(), MigrateError> {
    if path.exists() {
        return Err(MigrateError::Validation(format!(
            "{} already exists — refusing to overwrite",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod gen_tests;
