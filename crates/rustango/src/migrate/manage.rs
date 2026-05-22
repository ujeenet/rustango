//! Django-style `manage.py` analog for rustango projects.
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
//! (which must compile in their models). Same factoring as Django's
//! `manage.py` adapted for Rust's link-by-binary model.
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

use super::error::MigrateError;
use super::file::{self, DataOp, Migration, Operation};
use super::make::{make_migrations, make_migrations_for_app};
use super::runner;
use super::snapshot::SchemaSnapshot;

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
        "sqlmigrate" => sqlmigrate_cmd(dir, &args[1..], writer),
        "forget-pending" => forget_pending_cmd(pool, dir, &args[1..], writer).await,
        "startapp" => startapp(&args[1..], writer),
        "add-data-op" => add_data_op_cmd(dir, &args[1..], writer),
        "make:viewset" => make_viewset_cmd(&args[1..], writer),
        "make:api_routes" => make_api_routes_cmd(&args[1..], writer),
        "make:serializer" => make_serializer_cmd(&args[1..], writer),
        "make:form" => make_form_cmd(&args[1..], writer),
        "make:job" => make_job_cmd(&args[1..], writer),
        "make:notification" => make_notification_cmd(&args[1..], writer),
        "make:middleware" => make_middleware_cmd(&args[1..], writer),
        "make:test" => make_test_cmd(&args[1..], writer),
        "about" => about_cmd(pool, writer).await,
        "check" => check_cmd(pool, dir, &args[1..], writer).await,
        "docs" => docs_cmd(writer),
        "version" | "--version" => version_cmd(writer),
        "db:dump" => db_dump_cmd(&args[1..], writer),
        "db:restore" => db_restore_cmd(&args[1..], writer),
        "db:info" => db_info_cmd(writer),
        "dumpdata" => dumpdata_cmd(pool, &args[1..], writer).await,
        "loaddata" => loaddata_cmd(pool, &args[1..], writer).await,
        "showurls" => showurls_cmd(&args[1..], writer),
        "showmodels" => showmodels_cmd(&args[1..], writer),
        // #253 slice B — bootstrap an AdminUser row for projects
        // using `admin::Builder::with_session_auth`. Only compiled
        // when the `admin` feature is on; without it, fall through
        // to the unknown-subcommand error.
        #[cfg(feature = "admin")]
        "create-admin" => crate::admin::create_admin_cmd(pool, &args[1..], writer).await,
        "flush" => flush_cmd(pool, &args[1..], writer).await,
        "sendtestemail" => sendtestemail_cmd(&args[1..], writer).await,
        // v0.38 — `inspectdb` is tri-dialect: PG + MySQL via
        // `information_schema`, SQLite via `PRAGMA table_info` +
        // `sqlite_master`. Dispatch happens inside `inspectdb_cmd`.
        "inspectdb" => super::inspectdb::inspectdb_cmd(pool, &args[1..], writer).await,
        other => Err(MigrateError::Validation(format!(
            "unknown subcommand: `{other}` (run with --help for usage)"
        ))),
    }
}

fn print_help<W: Write>(w: &mut W) -> std::io::Result<()> {
    writeln!(w, "rustango::manage — Django-style migration runner\n")?;
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
        "  add-data-op --sql <SQL> [--reverse-sql <SQL>] [--name <name>] [--to <migration>]"
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
    writeln!(w, "  check [--deploy]")?;
    writeln!(
        w,
        "      Run system audits — pending migrations, missing models, common"
    )?;
    writeln!(
        w,
        "      misconfigurations. With --deploy: production hardening checks."
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
    writeln!(w, "  dumpdata [--model <name>] [--indent <N>]")?;
    writeln!(
        w,
        "      Export every registered model's rows as a Django-shape JSON"
    )?;
    writeln!(
        w,
        "      fixture (`[{{\"model\": \"app.Model\", \"pk\": N, \"fields\": {{...}}}}]`)."
    )?;
    writeln!(
        w,
        "      --model limits to a single model; pass multiple times for a set.\n"
    )?;
    writeln!(w, "  docs")?;
    writeln!(w, "      Open docs.rs/rustango in the default browser.\n")?;
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
    writeln!(w, "  db:restore <file> [--clean]")?;
    writeln!(
        w,
        "      Run psql against $DATABASE_URL with `\\i <file>`. With"
    )?;
    writeln!(
        w,
        "      --clean, prepend a `DROP SCHEMA public CASCADE; CREATE SCHEMA public;`"
    )?;
    writeln!(w, "      so the restore lands on a clean database.\n")?;
    writeln!(w, "  db:info")?;
    writeln!(
        w,
        "      Print the resolved DB URL (password redacted), detected"
    )?;
    writeln!(
        w,
        "      backend, and which `postgres`/`mysql` Cargo features are"
    )?;
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
    writeln!(
        w,
        "      Pipe to a file the user reviews + edits. Mirrors Django's"
    )?;
    writeln!(
        w,
        "      `inspectdb` shape — adopts rustango against an existing DB"
    )?;
    writeln!(w, "      without rewriting it.\n")?;
    writeln!(w, "  startapp <name> [--with-manage-bin]")?;
    writeln!(
        w,
        "      Scaffold a Django-shape app module under src/<name>/"
    )?;
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

fn makemigrations<W: Write>(dir: &Path, args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let mut empty = false;
    let mut name: Option<String> = None;
    let mut app: Option<String> = None;
    let mut scope_override: Option<crate::core::ModelScope> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--empty" => empty = true,
            "--app" => {
                app = Some(iter.next().cloned().ok_or_else(|| {
                    MigrateError::Validation("--app requires an app name".into())
                })?);
            }
            "--scope" => {
                let raw = iter.next().cloned().ok_or_else(|| {
                    MigrateError::Validation("--scope requires \"registry\" or \"tenant\"".into())
                })?;
                scope_override =
                    Some(crate::core::ModelScope::from_str(&raw).ok_or_else(|| {
                        MigrateError::Validation(format!(
                            "--scope must be \"registry\" or \"tenant\", got {raw:?}"
                        ))
                    })?);
            }
            "--help" | "-h" => {
                writeln!(
                    w,
                    "makemigrations [name]                  diff registry, write next file in <dir>\n\
                     makemigrations --app <app> [name]      diff one app, write to <project_root>/<app>/migrations/\n\
                     makemigrations --scope <s> [name]      <s> = registry|tenant; one file with that MigrationScope\n\
                     makemigrations --empty <name>          empty scaffold for data ops\n\
                     \n\
                     In tenancy projects (any registered model with scope = \"registry\"),\n\
                     a flagless makemigrations splits the diff into TWO files — one for\n\
                     registry-scoped models, one for tenant-scoped — so framework tables\n\
                     don't bleed across scopes when migrate-tenants fans out."
                )?;
                return Ok(());
            }
            other if other.starts_with('-') => {
                return Err(MigrateError::Validation(format!("unknown flag: {other}")));
            }
            other => {
                if name.is_some() {
                    return Err(MigrateError::Validation(format!(
                        "unexpected positional argument: {other}"
                    )));
                }
                name = Some(other.to_owned());
            }
        }
    }

    if empty {
        let Some(n) = name else {
            return Err(MigrateError::Validation(
                "makemigrations --empty requires a name".into(),
            ));
        };
        let mig = make_empty(dir, &n)?;
        writeln!(
            w,
            "wrote {} (empty scaffold — fill in `forward` with data ops)",
            file_path(dir, &mig.name).display()
        )?;
        return Ok(());
    }

    // Per-app makemigrations (slice 9.0g): filter inventory to models
    // whose `resolved_app_label()` matches `<app>` and write the
    // result under `<project_root>/<app>/migrations/`. The `dir`
    // argument's parent is the project root so existing manage CLI
    // wiring (which passes the flat `migrations/` dir) keeps working.
    if let Some(app_name) = app {
        let project_root = dir.parent().unwrap_or(dir);
        match make_migrations_for_app(project_root, &app_name, name.as_deref())? {
            Some(mig) => {
                let app_dir = project_root.join(&app_name).join("migrations");
                writeln!(w, "wrote {}", file_path(&app_dir, &mig.name).display())?;
                for op in &mig.forward {
                    writeln!(w, "    + {}", describe_op(op))?;
                }
            }
            None => writeln!(
                w,
                "app `{app_name}`: no changes — models match latest snapshot (or no models with this app_label)"
            )?,
        }
        return Ok(());
    }

    // Explicit scope flag — emit exactly one file in that scope.
    if let Some(scope) = scope_override {
        return write_scoped_migration(dir, scope, name.as_deref(), w);
    }

    // Auto-detect: if any registered model is `scope = "registry"`,
    // we're in a tenancy project. Run BOTH scopes so framework
    // registry tables get their own file and user's tenant models
    // get theirs. Each scope is a no-op when nothing in that scope
    // changed; the user sees one or two "wrote ..." lines.
    let has_registry_scoped = inventory::iter::<crate::core::ModelEntry>
        .into_iter()
        .any(|e| e.schema.scope == crate::core::ModelScope::Registry);
    if has_registry_scoped {
        let mut wrote_any = false;
        for scope in [
            crate::core::ModelScope::Registry,
            crate::core::ModelScope::Tenant,
        ] {
            let mig = crate::migrate::make::make_migrations_for_scope(dir, scope, name.as_deref())?;
            match mig {
                Some(m) => {
                    writeln!(
                        w,
                        "wrote {} ({} scope)",
                        file_path(dir, &m.name).display(),
                        scope.as_str(),
                    )?;
                    for op in &m.forward {
                        writeln!(w, "    + {}", describe_op(op))?;
                    }
                    wrote_any = true;
                }
                None => writeln!(w, "no changes for {} scope", scope.as_str(),)?,
            }
        }
        if !wrote_any {
            // Fall through silently — both scope-clean messages already printed.
        }
        return Ok(());
    }

    // Single-tenant (no registry-scoped models): one migration covering
    // every registered model — the v0.24.x behavior.
    match make_migrations(dir, name.as_deref())? {
        Some(mig) => {
            writeln!(w, "wrote {}", file_path(dir, &mig.name).display())?;
            for op in &mig.forward {
                writeln!(w, "    + {}", describe_op(op))?;
            }
        }
        None => writeln!(w, "no changes — registry matches latest snapshot")?,
    }
    Ok(())
}

fn write_scoped_migration<W: Write>(
    dir: &Path,
    scope: crate::core::ModelScope,
    name: Option<&str>,
    w: &mut W,
) -> Result<(), MigrateError> {
    match crate::migrate::make::make_migrations_for_scope(dir, scope, name)? {
        Some(mig) => {
            writeln!(
                w,
                "wrote {} ({} scope)",
                file_path(dir, &mig.name).display(),
                scope.as_str(),
            )?;
            for op in &mig.forward {
                writeln!(w, "    + {}", describe_op(op))?;
            }
        }
        None => writeln!(
            w,
            "no changes — {} models match latest snapshot",
            scope.as_str(),
        )?,
    }
    Ok(())
}

async fn migrate<W: Write>(
    pool: &Pool,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let mut dry_run = false;
    let mut squash = false;
    let mut positional: Option<&str> = None;
    for arg in args {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--squash" => squash = true,
            "--help" | "-h" => {
                writeln!(
                    w,
                    "migrate                    apply pending migrations\n\
                     migrate <target>           forward or back to <target> (`zero` wipes)\n\
                     migrate --dry-run          preview the SQL without writing\n\
                     migrate --squash           delete every pending (un-applied) migration JSON\n\
                                                and regenerate a single fresh diff. Dev-iteration\n\
                                                escape hatch — refuses to touch applied rows."
                )?;
                return Ok(());
            }
            other if other.starts_with('-') => {
                return Err(MigrateError::Validation(format!("unknown flag: {other}")));
            }
            other => {
                if positional.is_some() {
                    return Err(MigrateError::Validation(format!(
                        "unexpected positional argument: {other}"
                    )));
                }
                positional = Some(other);
            }
        }
    }

    if squash {
        if dry_run || positional.is_some() {
            return Err(MigrateError::Validation(
                "`migrate --squash` does not combine with `--dry-run` or a positional target"
                    .into(),
            ));
        }
        return migrate_squash(pool, dir, w).await;
    }

    if dry_run {
        if positional.is_some() {
            return Err(MigrateError::Validation(
                "`migrate <target> --dry-run` is not supported in v0.4 — use plain `--dry-run` to preview pending forward migrations".into(),
            ));
        }
        let preview = runner::migrate_dry_run_pool(pool, dir).await?;
        if preview.is_empty() {
            writeln!(w, "nothing to migrate (already up to date)")?;
        } else {
            writeln!(
                w,
                "-- DRY RUN: {} pending migration(s); no SQL will be executed",
                preview.len()
            )?;
            for p in &preview {
                writeln!(w)?;
                writeln!(
                    w,
                    "-- {} ({})",
                    p.name,
                    if p.atomic { "atomic" } else { "non-atomic" }
                )?;
                for stmt in &p.statements {
                    writeln!(w, "{stmt};")?;
                }
            }
        }
        return Ok(());
    }

    if let Some(target) = positional {
        let touched = runner::migrate_to_pool(pool, dir, target).await?;
        if touched.is_empty() {
            writeln!(w, "already at {target}")?;
        } else {
            for m in &touched {
                writeln!(w, "  touched {}", m.name)?;
            }
        }
        return Ok(());
    }

    let applied = runner::migrate_pool(pool, dir).await?;
    if applied.is_empty() {
        writeln!(w, "nothing to migrate (already up to date)")?;
    } else {
        for m in &applied {
            writeln!(w, "  applied {}", m.name)?;
        }
    }
    Ok(())
}

/// `migrate --squash` (#84a) — dev-iteration escape hatch.
///
/// Deletes every pending (un-applied) migration JSON in `dir`, then
/// re-runs `makemigrations` so the diff regenerates as a single
/// fresh file against the current model registry. Recovers the
/// "scaffolder shape → real shape" iteration cycle that hits the
/// `AddColumn NOT NULL no default` validator rejection without
/// touching the database.
///
/// Refuses if any pending migration is somehow already applied
/// (impossible by definition — "pending" means absent from the
/// ledger — but the check is cheap and guards against a future
/// caller passing a stale `applied_set`). Refuses if there are zero
/// pending migrations (nothing to do) or only one (`forget-pending`
/// is the right verb for the single-file case).
async fn migrate_squash<W: Write>(pool: &Pool, dir: &Path, w: &mut W) -> Result<(), MigrateError> {
    runner::ensure_ledger_pool(pool).await?;
    let all = file::list_dir(dir)?;
    let applied = runner::applied_set_pool(pool).await?;

    let pending: Vec<&Migration> = all.iter().filter(|m| !applied.contains(&m.name)).collect();
    if pending.is_empty() {
        writeln!(
            w,
            "no pending migrations to squash (every JSON is in the ledger)"
        )?;
        return Ok(());
    }
    if pending.len() == 1 {
        return Err(MigrateError::Validation(format!(
            "only one pending migration (`{}`) — use `forget-pending {}` instead of `--squash`",
            pending[0].name, pending[0].name,
        )));
    }

    // Defensive double-check: the filter above already guarantees
    // none of these are applied. If a future refactor breaks that
    // invariant we still fail loudly instead of clobbering the
    // ledger.
    for m in &pending {
        if applied.contains(&m.name) {
            return Err(MigrateError::Validation(format!(
                "migrate --squash: refused — migration `{}` appears applied. \
                 Use `migrate <prev>` or `downgrade` to unapply it first.",
                m.name,
            )));
        }
    }

    writeln!(
        w,
        "squashing {} pending migration(s) into a fresh diff:",
        pending.len()
    )?;
    for m in &pending {
        let path = file_path(dir, &m.name);
        std::fs::remove_file(&path).map_err(|e| {
            MigrateError::Validation(format!("migrate --squash: rm {}: {e}", path.display()))
        })?;
        writeln!(w, "  removed {}", path.display())?;
    }

    writeln!(w, "regenerating diff against current model registry...")?;
    // Empty args means "default makemigrations behavior" — splits
    // registry vs tenant in tenancy projects, single file otherwise.
    // Pass through to the existing entry point so any future flags
    // gain consistent behavior automatically.
    makemigrations(dir, &[], w)
}

async fn downgrade<W: Write>(
    pool: &Pool,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let steps: usize = if let Some(arg) = args.first() {
        arg.parse().map_err(|_| {
            MigrateError::Validation(format!(
                "invalid step count: {arg} (expected a non-negative integer)"
            ))
        })?
    } else {
        1
    };
    let touched = runner::downgrade_pool(pool, dir, steps).await?;
    if touched.is_empty() {
        writeln!(w, "nothing to downgrade")?;
    } else {
        for m in &touched {
            writeln!(w, "  rolled back {}", m.name)?;
        }
    }
    Ok(())
}

/// Django-shape `sqlmigrate <name>` — print the SQL that would run
/// when the named migration is applied, without touching the database.
/// Issue #345.
///
/// Output format mirrors `migrate --dry-run` per-migration: a comment
/// header (`-- <name> (atomic|non-atomic)`) followed by every emitted
/// statement, semicolon-terminated, one per line.
fn sqlmigrate_cmd<W: Write>(dir: &Path, args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let mut positional: Option<&str> = None;
    for arg in args {
        match arg.as_str() {
            "--help" | "-h" => {
                writeln!(
                    w,
                    "sqlmigrate <name>          print the SQL the named migration would emit\n\
                                                without applying it"
                )?;
                return Ok(());
            }
            other if other.starts_with('-') => {
                return Err(MigrateError::Validation(format!("unknown flag: {other}")));
            }
            other => {
                if positional.is_some() {
                    return Err(MigrateError::Validation(format!(
                        "unexpected positional argument: {other}"
                    )));
                }
                positional = Some(other);
            }
        }
    }
    let name = positional
        .ok_or_else(|| MigrateError::Validation("sqlmigrate requires a migration name".into()))?;
    let preview = runner::sqlmigrate_one(dir, name)?;
    writeln!(
        w,
        "-- {} ({})",
        preview.name,
        if preview.atomic {
            "atomic"
        } else {
            "non-atomic"
        }
    )?;
    for stmt in &preview.statements {
        writeln!(w, "{stmt};")?;
    }
    Ok(())
}

async fn showmigrations<W: Write>(pool: &Pool, dir: &Path, w: &mut W) -> Result<(), MigrateError> {
    runner::ensure_ledger_pool(pool).await?;
    let all = file::list_dir(dir)?;
    let applied = runner::applied_set_pool(pool).await?;

    if all.is_empty() {
        writeln!(w, "(no migrations in {})", dir.display())?;
        return Ok(());
    }
    writeln!(w, "Migrations in {}:", dir.display())?;
    for m in &all {
        let mark = if applied.contains(&m.name) {
            "[X]"
        } else {
            "[ ]"
        };
        writeln!(w, "  {mark} {}", m.name)?;
    }
    Ok(())
}

/// `manage forget-pending <name>` — delete a migration JSON that
/// hasn't been applied yet, so the next `makemigrations` regenerates
/// the diff from the current model registry. The actionable
/// companion to the dev-iteration recovery path documented in the
/// `AddColumn NOT NULL no default` validator error (#84).
///
/// Refuses if:
///   - the migration name doesn't match any file in `dir`
///   - the migration is recorded in `__rustango_migrations__` (i.e.
///     it's been applied — removing the JSON without unapplying
///     would orphan the ledger row and break further migrate runs)
///   - more than one migration matches a partial-name query (use
///     the full name to disambiguate)
///
/// On success: deletes the file, prints a one-line confirmation,
/// and suggests `makemigrations` as the follow-up.
async fn forget_pending_cmd<W: Write>(
    pool: &Pool,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    // --help / no args
    let target = match args.first().map(String::as_str) {
        Some("--help") | Some("-h") | None => {
            writeln!(w, "forget-pending <name>")?;
            writeln!(
                w,
                "  Delete a migration JSON that has NOT been applied yet, so the"
            )?;
            writeln!(
                w,
                "  next `makemigrations` regenerates the diff against current"
            )?;
            writeln!(
                w,
                "  models. Refuses to delete an already-applied migration (would"
            )?;
            writeln!(
                w,
                "  orphan the `{}` ledger row and break later runs).",
                runner::LEDGER_TABLE
            )?;
            writeln!(w)?;
            writeln!(
                w,
                "  <name> can be the full migration name (e.g. `0003_auto`)"
            )?;
            writeln!(w, "  or a unique substring; ambiguous matches error.")?;
            return Ok(());
        }
        Some(s) if s.starts_with('-') => {
            return Err(MigrateError::Validation(format!(
                "forget-pending: expected positional <name> first, got flag `{s}`"
            )));
        }
        Some(s) => s,
    };

    runner::ensure_ledger_pool(pool).await?;
    let all = file::list_dir(dir)?;
    let applied = runner::applied_set_pool(pool).await?;

    // Resolve <name> against the file list — accept exact match OR
    // unique substring (matches the "0003" use case).
    let exact: Vec<&Migration> = all.iter().filter(|m| m.name == target).collect();
    let candidates: Vec<&Migration> = if exact.len() == 1 {
        exact
    } else {
        all.iter().filter(|m| m.name.contains(target)).collect()
    };
    let migration = match candidates.len() {
        0 => {
            return Err(MigrateError::Validation(format!(
                "forget-pending: no migration matches `{target}` in {}",
                dir.display()
            )));
        }
        1 => candidates[0],
        n => {
            let names: Vec<&str> = candidates.iter().map(|m| m.name.as_str()).collect();
            return Err(MigrateError::Validation(format!(
                "forget-pending: `{target}` is ambiguous ({n} matches: {}); pass the full migration name",
                names.join(", ")
            )));
        }
    };

    // Refuse if the migration has been applied — removing it would
    // orphan the ledger row.
    if applied.contains(&migration.name) {
        return Err(MigrateError::Validation(format!(
            "forget-pending: migration `{}` is already applied (recorded in `{}`). \
             Use `migrate <prev>` or `downgrade` to unapply it first, then \
             `forget-pending` to drop the JSON.",
            migration.name,
            runner::LEDGER_TABLE,
        )));
    }

    let path = file_path(dir, &migration.name);
    std::fs::remove_file(&path).map_err(|e| {
        MigrateError::Validation(format!("forget-pending: rm {}: {e}", path.display()))
    })?;
    writeln!(w, "deleted {}", path.display())?;
    writeln!(w, "  next: `cargo run -- makemigrations` will regenerate")?;
    writeln!(w, "  the diff against the current model registry.")?;
    Ok(())
}

/// Write an empty migration scaffold (`forward: []`) carrying the
/// predecessor's snapshot so a subsequent `make_migrations` doesn't
/// re-emit the same diff. The user fills in `forward` by hand for
/// data migrations.
///
/// Public so binaries that want a programmatic equivalent of
/// `makemigrations --empty` can call it directly.
///
/// # Errors
/// As [`super::file::list_dir`] / [`super::file::write`].
pub fn make_empty(dir: &Path, name: &str) -> Result<Migration, MigrateError> {
    let prior = file::list_dir(dir)?;
    let prev_snapshot = prior.last().map_or_else(
        || SchemaSnapshot {
            tables: vec![],
            m2m_tables: vec![],
            indexes: vec![],
            checks: vec![],
        },
        |m| m.snapshot.clone(),
    );
    let prev_name = prior.last().map(|m| m.name.clone());
    let next_index = prior
        .last()
        .and_then(|m| file::extract_index(&m.name))
        .map_or(1, |n| n + 1);

    let full_name = format!("{next_index:04}_{name}");
    let mig = Migration {
        name: full_name.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        prev: prev_name,
        atomic: true,
        scope: super::MigrationScope::default(),
        snapshot: prev_snapshot,
        forward: vec![],
    };
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
    }
    file::write(&file_path(dir, &mig.name), &mig)?;
    Ok(mig)
}

fn file_path(dir: &Path, name: &str) -> std::path::PathBuf {
    dir.join(format!("{name}.json"))
}

// ------------------------------------------------------------------ add-data-op

/// Create a new migration containing a single data operation.
///
/// `name` is the migration name suffix (e.g. `"backfill_slugs"`); the index
/// prefix is derived automatically from the migration chain in `dir`.
///
/// If `reverse_sql` is `Some`, the op is marked `reversible = true` and
/// `unapply` / `downgrade` will run the reverse SQL. If `None`, the op is
/// irreversible and rollback will fail fast.
///
/// # Errors
/// [`MigrateError::Io`] / [`MigrateError::Json`] if the file can't be written.
pub fn make_data_migration(
    dir: &Path,
    name: &str,
    sql: &str,
    reverse_sql: Option<&str>,
) -> Result<Migration, MigrateError> {
    let prior = file::list_dir(dir)?;
    let prev_snapshot = prior
        .last()
        .map_or_else(|| SchemaSnapshot::default(), |m| m.snapshot.clone());
    let prev_name = prior.last().map(|m| m.name.clone());
    let next_index = prior
        .last()
        .and_then(|m| file::extract_index(&m.name))
        .map_or(1, |n| n + 1);

    let full_name = format!("{next_index:04}_{name}");
    let op = Operation::Data(DataOp {
        sql: sql.to_owned(),
        reverse_sql: reverse_sql.map(str::to_owned),
        reversible: reverse_sql.is_some(),
    });
    let mig = Migration {
        name: full_name.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        prev: prev_name,
        atomic: true,
        scope: super::MigrationScope::default(),
        snapshot: prev_snapshot,
        forward: vec![op],
    };
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
    }
    file::write(&file_path(dir, &mig.name), &mig)?;
    Ok(mig)
}

/// Append a data operation to an existing migration file.
///
/// `migration_name` is the full migration name (e.g. `"0002_add_slug"`).
/// The op is appended to the end of `forward`. If `reverse_sql` is `Some`
/// the op is reversible.
///
/// # Errors
/// [`MigrateError::Validation`] if `migration_name` not found in `dir`.
/// [`MigrateError::Io`] / [`MigrateError::Json`] for file I/O failures.
pub fn append_data_op(
    dir: &Path,
    migration_name: &str,
    sql: &str,
    reverse_sql: Option<&str>,
) -> Result<(), MigrateError> {
    let path = file_path(dir, migration_name);
    let mut mig = file::load(&path).map_err(|_| {
        MigrateError::Validation(format!(
            "migration `{migration_name}` not found at {}",
            path.display()
        ))
    })?;
    mig.forward.push(Operation::Data(DataOp {
        sql: sql.to_owned(),
        reverse_sql: reverse_sql.map(str::to_owned),
        reversible: reverse_sql.is_some(),
    }));
    file::write(&path, &mig)?;
    Ok(())
}

/// `add-data-op` subcommand handler.
fn add_data_op_cmd<W: Write>(dir: &Path, args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let mut sql: Option<String> = None;
    let mut reverse_sql: Option<String> = None;
    let mut name: Option<String> = None;
    let mut to: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--sql" => {
                sql =
                    Some(iter.next().cloned().ok_or_else(|| {
                        MigrateError::Validation("--sql requires a value".into())
                    })?);
            }
            "--reverse-sql" => {
                reverse_sql = Some(iter.next().cloned().ok_or_else(|| {
                    MigrateError::Validation("--reverse-sql requires a value".into())
                })?);
            }
            "--name" => {
                name =
                    Some(iter.next().cloned().ok_or_else(|| {
                        MigrateError::Validation("--name requires a value".into())
                    })?);
            }
            "--to" => {
                to = Some(iter.next().cloned().ok_or_else(|| {
                    MigrateError::Validation("--to requires a migration name".into())
                })?);
            }
            "--help" | "-h" => {
                writeln!(
                    w,
                    "add-data-op --sql <SQL> [--reverse-sql <SQL>] [--name <name>] [--to <migration>]"
                )?;
                return Ok(());
            }
            other if other.starts_with('-') => {
                return Err(MigrateError::Validation(format!("unknown flag: {other}")));
            }
            other => {
                return Err(MigrateError::Validation(format!(
                    "unexpected argument: `{other}` — use --sql, --reverse-sql, --name, --to"
                )));
            }
        }
    }

    let sql = sql.ok_or_else(|| MigrateError::Validation("--sql is required".into()))?;

    if let Some(migration_name) = to {
        append_data_op(dir, &migration_name, &sql, reverse_sql.as_deref())?;
        writeln!(w, "appended data op to {migration_name}.json")?;
    } else {
        let name = name.unwrap_or_else(|| "data_op".to_owned());
        let mig = make_data_migration(dir, &name, &sql, reverse_sql.as_deref())?;
        let rev_note = if reverse_sql.is_some() {
            " (reversible)"
        } else {
            " (irreversible)"
        };
        writeln!(w, "wrote {}{rev_note}", file_path(dir, &mig.name).display())?;
    }
    Ok(())
}

fn describe_op(op: &Operation) -> String {
    match op {
        Operation::Schema(c) => format!("{c:?}"),
        Operation::Data(d) => {
            let head: String = d.sql.chars().take(60).collect();
            let ellipsis = if d.sql.chars().count() > 60 {
                "…"
            } else {
                ""
            };
            format!("data: {head}{ellipsis}")
        }
    }
}

/// `startapp <name> [--with-manage-bin]` — scaffold a Django-shape app
/// module under `src/<name>/` (`models.rs` + `views.rs` + `urls.rs` +
/// `mod.rs`). Idempotent — files that already exist are reported as
/// skipped. With `--with-manage-bin`, also writes `src/bin/manage.rs`
/// with the standard single-tenant dispatcher boilerplate.
fn startapp<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let mut iter = args.iter();
    let app_name = iter
        .next()
        .cloned()
        .ok_or_else(|| MigrateError::Validation(usage()))?;
    let mut with_manage_bin = false;
    let mut into: Option<String> = None;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--with-manage-bin" => with_manage_bin = true,
            "--into" => {
                into = Some(iter.next().cloned().ok_or_else(|| {
                    MigrateError::Validation("--into requires a directory argument".into())
                })?);
            }
            "--help" | "-h" => {
                writeln!(w, "{}", usage())?;
                return Ok(());
            }
            other => {
                return Err(MigrateError::Validation(format!(
                    "startapp: unknown argument `{other}` (run --help for usage)"
                )));
            }
        }
    }
    let base_label = into.clone().unwrap_or_else(|| "src".into());
    let opts = super::scaffold::StartAppOptions {
        app_name: app_name.clone(),
        manage_bin: with_manage_bin.then_some(super::scaffold::SINGLE_TENANT_MANAGE_BIN),
        base_dir: into.map(std::path::PathBuf::from),
    };
    // Project root = current working directory. Most users run
    // `cargo run -- startapp …` from the project root,
    // which is exactly where Cargo.toml lives. Document this in the
    // help string so non-default invocations are an explicit `cd`.
    let cwd = std::env::current_dir()?;
    let report = super::scaffold::startapp(&cwd, &opts)?;
    write_startapp_report(w, &app_name, &base_label, &report)
}

fn write_startapp_report<W: Write>(
    w: &mut W,
    app_name: &str,
    base_label: &str,
    report: &super::scaffold::StartAppReport,
) -> Result<(), MigrateError> {
    if report.written.is_empty() && report.skipped.is_empty() {
        writeln!(w, "startapp: nothing to do")?;
        return Ok(());
    }
    writeln!(w, "startapp `{app_name}`")?;
    for path in &report.written {
        writeln!(w, "  + wrote {path}")?;
    }
    for path in &report.skipped {
        writeln!(w, "  · {path} already exists — left untouched")?;
    }
    for path in &report.patched {
        writeln!(w, "  ~ patched {path} (auto-mounted new app)")?;
    }
    for hint in &report.manual_steps {
        writeln!(w, "  ! manual: {hint}")?;
    }
    if !report.written.is_empty() {
        writeln!(w, "next:")?;
        writeln!(
            w,
            "  add `mod {app_name};` to {base_label}/main.rs (or {base_label}/lib.rs)"
        )?;
        writeln!(
            w,
            "  so the derive macros' `inventory` registrations are pulled in."
        )?;
    }
    Ok(())
}

fn usage() -> String {
    "startapp <name> [--with-manage-bin]\n  \
     Scaffold a Django-shape app module under src/<name>/ (mod.rs +\n  \
     models.rs + views.rs + urls.rs). Idempotent: existing files\n  \
     are left untouched. <name> must be a valid Rust identifier.\n\n  \
     --with-manage-bin\n  \
     Also write src/bin/manage.rs with the single-tenant dispatcher\n  \
     boilerplate. Skipped if the file already exists."
        .to_owned()
}

// ============================================================ about / check / docs / version

/// `manage about` — env summary for support tickets / debugging.
async fn about_cmd<W: Write>(pool: &Pool, w: &mut W) -> Result<(), MigrateError> {
    let registered_models = crate::core::inventory::iter::<crate::core::ModelEntry>
        .into_iter()
        .count();
    let mut apps: std::collections::BTreeSet<&'static str> = std::collections::BTreeSet::new();
    for entry in crate::core::inventory::iter::<crate::core::ModelEntry> {
        if let Some(app) = entry.resolved_app_label() {
            apps.insert(app);
        }
    }

    writeln!(w, "rustango")?;
    writeln!(w, "  version:        {}", env!("CARGO_PKG_VERSION"))?;
    writeln!(w, "  models:         {registered_models} registered")?;
    writeln!(
        w,
        "  apps:           {} ({})",
        apps.len(),
        if apps.is_empty() {
            "none".to_owned()
        } else {
            apps.iter().copied().collect::<Vec<_>>().join(", ")
        }
    )?;
    let env_label = std::env::var("RUSTANGO_ENV").unwrap_or_else(|_| "(unset)".into());
    writeln!(w, "  RUSTANGO_ENV:   {env_label}")?;
    let db_url = std::env::var("DATABASE_URL").map_or("(unset)".into(), |s| {
        // Redact password component
        if let Some(at) = s.rfind('@') {
            if let Some(scheme_end) = s.find("://") {
                let prefix = &s[..scheme_end + 3];
                let rest = &s[at..];
                return format!("{prefix}***{rest}");
            }
        }
        s
    });
    writeln!(w, "  DATABASE_URL:   {db_url}")?;

    // DB connectivity — tri-dialect: SELECT 1 is universal across PG/MySQL/SQLite.
    write!(w, "  db_connect:     ")?;
    let ok = crate::sql::raw_execute_pool(pool, "SELECT 1", Vec::new())
        .await
        .is_ok();
    writeln!(w, "{}", if ok { "ok" } else { "FAILED" })?;

    Ok(())
}

/// `manage check [--deploy]` — run system audits.
async fn check_cmd<W: Write>(
    pool: &Pool,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let deploy = args.iter().any(|a| a == "--deploy");
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut info: Vec<String> = Vec::new();

    writeln!(
        w,
        "running rustango system check{}...",
        if deploy { " (deploy mode)" } else { "" }
    )?;

    // Always-on checks
    let model_count = crate::core::inventory::iter::<crate::core::ModelEntry>
        .into_iter()
        .count();
    if model_count == 0 {
        errors.push("no models registered — every #[derive(Model)] struct must be `pub use`d through the binary's crate root".into());
    } else {
        info.push(format!("{model_count} models registered via inventory"));
    }

    // DB connectivity — tri-dialect via raw_execute_pool.
    if crate::sql::raw_execute_pool(pool, "SELECT 1", Vec::new())
        .await
        .is_err()
    {
        errors.push("cannot connect to database — verify DATABASE_URL is reachable".into());
    } else {
        info.push("database reachable".into());
    }

    // Pending migrations
    if dir.exists() {
        let prior = file::list_dir(dir)?;
        if prior.is_empty() && model_count > 0 {
            warnings.push(
                "models registered but no migrations on disk — run `manage makemigrations`".into(),
            );
        } else {
            info.push(format!("{} migration(s) on disk", prior.len()));
        }
    }

    // Deploy checks
    if deploy {
        let mut audit = DeployAuditFindings::default();
        run_deploy_audit(&deploy_audit_env(), &mut audit);
        // Settings audit (#87 slice 4) — flags dev-defaults left in
        // prod (e.g. `headers_preset = "dev"` in `prod_settings.toml`).
        // Gated by the `config` feature; no-op without it.
        #[cfg(feature = "config")]
        run_settings_audit(&mut audit);
        info.extend(audit.info);
        warnings.extend(audit.warnings);
        errors.extend(audit.errors);
    }

    // Render
    for msg in &info {
        writeln!(w, "  [info]    {msg}")?;
    }
    for msg in &warnings {
        writeln!(w, "  [warning] {msg}")?;
    }
    for msg in &errors {
        writeln!(w, "  [error]   {msg}")?;
    }

    if !errors.is_empty() {
        return Err(MigrateError::Validation(format!(
            "{} system check(s) failed",
            errors.len()
        )));
    }
    if warnings.is_empty() && errors.is_empty() {
        writeln!(w, "all checks passed")?;
    }
    Ok(())
}

/// `manage docs` — try to open https://docs.rs/rustango in the user's browser.
fn docs_cmd<W: Write>(w: &mut W) -> Result<(), MigrateError> {
    let url = "https://docs.rs/rustango";
    writeln!(w, "{url}")?;
    // Best-effort — don't fail if the OS has no `open` / `xdg-open` / `start`
    let opener = if cfg!(target_os = "macos") {
        Some(("open", url))
    } else if cfg!(target_os = "linux") {
        Some(("xdg-open", url))
    } else if cfg!(target_os = "windows") {
        Some(("cmd", "/C start"))
    } else {
        None
    };
    if let Some((cmd, _)) = opener {
        let _ = std::process::Command::new(cmd).arg(url).spawn();
    }
    Ok(())
}

/// `manage version` — print the rustango framework version.
fn version_cmd<W: Write>(w: &mut W) -> Result<(), MigrateError> {
    writeln!(w, "rustango {}", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}

// ============================================================ make:* generators

fn parse_name_and_model(args: &[String]) -> Result<(String, Option<String>), MigrateError> {
    let mut name: Option<String> = None;
    let mut model: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--model" => {
                model =
                    Some(iter.next().cloned().ok_or_else(|| {
                        MigrateError::Validation("--model requires a value".into())
                    })?);
            }
            other if other.starts_with('-') => {
                return Err(MigrateError::Validation(format!("unknown flag `{other}`")));
            }
            other => {
                if name.is_some() {
                    return Err(MigrateError::Validation(format!(
                        "unexpected positional `{other}`"
                    )));
                }
                name = Some(other.to_owned());
            }
        }
    }
    let name = name.ok_or_else(|| {
        MigrateError::Validation("name is required (e.g. `manage make:viewset PostViewSet`)".into())
    })?;
    if !is_valid_type_name(&name) {
        return Err(MigrateError::Validation(format!(
            "`{name}` is not a valid Rust type name (PascalCase, alphanumeric + underscore)"
        )));
    }
    Ok((name, model))
}

fn is_valid_type_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes[0].is_ascii_uppercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

fn pascal_to_snake(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

fn write_generated<W: Write>(
    w: &mut W,
    file_name: &str,
    contents: String,
) -> Result<(), MigrateError> {
    let path = std::path::PathBuf::from("src").join(file_name);
    if path.exists() {
        return Err(MigrateError::Validation(format!(
            "{} already exists — refusing to overwrite",
            path.display()
        )));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, contents)?;
    writeln!(w, "wrote {}", path.display())?;
    writeln!(
        w,
        "  add `mod {};` to src/main.rs (or `pub mod ...;` to src/lib.rs)",
        file_name.trim_end_matches(".rs")
    )?;
    Ok(())
}

fn make_viewset_cmd<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    // Tenancy projects need a different scaffold shape — the
    // single-pool `#[derive(ViewSet)]` is fatal for them (#80) — so
    // this command toggles between two templates. Resolution order:
    //
    //   1. `--no-tenant` flag → pool template (escape hatch when
    //      auto-detection guesses wrong, or when a tenancy project
    //      hand-rolls a single-pool viewset for some reason)
    //   2. `--tenant` flag → tenant template (explicit)
    //   3. Cargo.toml has `features = [..., "tenancy", ...]` on the
    //      `rustango` dep → tenant template (auto-detected)
    //   4. Otherwise → pool template
    //
    // The auto-detect path keeps Django-shape "you don't need a flag
    // for the obvious thing" ergonomics: tenancy projects get
    // `tenant_router` without the user having to remember `--tenant`.
    let mut explicit_tenant = false;
    let mut explicit_no_tenant = false;
    let mut filtered: Vec<String> = Vec::with_capacity(args.len());
    for a in args {
        if a == "--tenant" || a == "--tenant-aware" {
            explicit_tenant = true;
        } else if a == "--no-tenant" {
            explicit_no_tenant = true;
        } else {
            filtered.push(a.clone());
        }
    }
    let (tenant_aware, echoed_auto_detect) =
        resolve_viewset_tenant_mode(explicit_tenant, explicit_no_tenant, project_uses_tenancy());
    // Echo when auto-detection picked tenant mode — silent
    // auto-config surprises users; one informational line points
    // them at the override flag without being noisy.
    if echoed_auto_detect {
        writeln!(
            w,
            "make:viewset: auto-detected tenancy mode from Cargo.toml (pass `--no-tenant` to override)"
        )?;
    }
    let (name, model) = parse_name_and_model(&filtered)?;
    let snake = pascal_to_snake(&name);
    let model = model.unwrap_or_else(|| "Post".into());
    let body = if tenant_aware {
        viewset_template_tenant(&name, &model, &snake)
    } else {
        viewset_template_pool(&name, &model, &snake)
    };
    write_generated(w, &format!("{snake}.rs"), body)
}

/// Template emitted by `make:viewset <Name>` (default). Uses the
/// `#[derive(ViewSet)]` shape with a mount-time `PgPool` —
/// appropriate for single-tenant projects (api / fullstack
/// templates).
fn viewset_template_pool(name: &str, model: &str, snake: &str) -> String {
    format!(
        r#"//! Auto-scaffolded by `manage make:viewset {name}`.

use rustango::ViewSet;

#[derive(ViewSet)]
#[viewset(
    model        = {model},
    fields       = "id, ",
    filter_fields = "",
    search_fields = "",
    page_size    = 20,
)]
pub struct {name};

// Mount in your urls.rs:
//
//   .merge({name}::router("/api/{snake}", pool.clone()))
"#
    )
}

/// Template emitted by `make:viewset <Name> --tenant`. Uses the
/// runtime-built `ViewSet::for_model(...).tenant_router(...)` shape
/// (#80) so each request resolves the per-tenant connection via the
/// `Tenant` extractor instead of capturing a single pool at mount
/// time. Required for tenancy projects.
///
/// Since v0.30, `tenant_router` carries the full static-router builder
/// chain (filter / search / ordering / pagination / permissions) so
/// the scaffold demonstrates each knob — same shape Django's class-
/// based admin generators emit, just with `// uncomment to enable`
/// markers next to each one.
fn viewset_template_tenant(name: &str, model: &str, snake: &str) -> String {
    format!(
        r#"//! Auto-scaffolded by `manage make:viewset {name} --tenant`.
//!
//! Tenant-aware viewset: each request resolves the connection via
//! `rustango::extractors::Tenant`, so the same `router()` serves
//! every tenant under their own subdomain / schema / database.
//!
//! Since v0.30 (#80), `tenant_router` carries the full static-router
//! builder chain — filter_fields / search_fields / ordering /
//! page_size / permissions_for_model all work in tenant mode too.

use axum::Router;
use rustango::core::Model as _;
use rustango::viewset::ViewSet;

use crate::models::{model};

pub fn router() -> Router<()> {{
    ViewSet::for_model({model}::SCHEMA)
        // .fields(&["id", "name", "created_at"])    // restrict response shape
        // .filter_fields(&["status", "owner_id"])    // ?status=draft&owner_id=42
        // .search_fields(&["name", "description"])   // ?search=foo (ILIKE)
        // .ordering(&[("created_at", true)])         // default ORDER BY
        // .ordering_fields(&["name", "created_at"])  // ?ordering=-name allowlist
        // .page_size(20)
        // .permissions_for_model::<{model}>()        // CRUD codenames
        // .read_only()                               // GET only
        .tenant_router("/api/{snake}")
}}

// Mount in your urls.rs:
//
//   .merge(crate::viewsets::{snake}::router())
"#
    )
}

/// Detect whether the project the scaffolder is running inside enables
/// the `tenancy` feature on its `rustango` dep — used to default
/// `make:viewset` to `--tenant` when no explicit flag is given.
/// Reads `./Cargo.toml` from the current working directory; returns
/// `false` (single-tenant default) on any read/parse failure so the
/// scaffolder doesn't break in odd environments.
///
/// Heuristic: look for a `[dependencies.rustango]` table with
/// `features = [..., "tenancy", ...]` OR an inline-table dep
/// (`rustango = {{ version = "...", features = [..., "tenancy"] }}`).
/// Cargo.toml syntax is well-defined; this is a substring check
/// rather than a full TOML parse to keep the binary light, but it's
/// strict enough — a `# tenancy` comment elsewhere wouldn't trigger.
/// Decide whether `make:viewset` should emit the tenant template
/// and whether to echo a one-line auto-detect notice. Pure
/// function — no I/O, no `cargo` introspection — so tests can
/// exercise every branch without chdir tricks. The actual Cargo.toml
/// inspection happens once in [`project_uses_tenancy`] and gets
/// passed in here.
///
/// Returns `(tenant_aware, echo_auto_detect)`.
fn resolve_viewset_tenant_mode(
    explicit_tenant: bool,
    explicit_no_tenant: bool,
    project_tenancy: bool,
) -> (bool, bool) {
    if explicit_no_tenant {
        return (false, false);
    }
    if explicit_tenant {
        return (true, false);
    }
    // Auto-detect path. Echo only when we actually picked tenant.
    (project_tenancy, project_tenancy)
}

fn project_uses_tenancy() -> bool {
    let Ok(s) = std::fs::read_to_string("Cargo.toml") else {
        return false;
    };
    // Find the rustango dep block. Either a bare key
    // `rustango = "..."` (no features at all → not tenancy) or a
    // structured form (`rustango = { features = [...] }`) or a
    // dedicated table `[dependencies.rustango]`. We scan the whole
    // file for both shapes since the line carrying the features may
    // be on its own.
    let lower = s.to_ascii_lowercase();
    let has_inline = lower.contains("rustango")
        && lower
            .lines()
            .any(|line| line.contains("rustango") && line.contains("\"tenancy\""));
    let has_table_block =
        lower.contains("[dependencies.rustango]") && lower.contains("\"tenancy\"");
    has_inline || has_table_block
}

/// `manage make:api_routes <app> [--tenant]` — emit
/// `src/<app>/api_routes.rs`, the per-app composer that merges
/// every viewset's router into a single `Router<()>` (#82).
///
/// Mirrors the working pattern in tango/src/regions/api_routes.rs:
/// each per-model file under `viewsets/` exposes a router fn, and
/// `api_routes::api()` composes them with `.merge(...)`. The
/// scaffold is intentionally minimal — placeholder comments mark
/// where to add `.merge(...)` lines as new viewsets are written.
///
/// Refuses to overwrite an existing file. Errors clearly when the
/// `src/<app>/` directory is missing — tells the user to run
/// `manage startapp <app>` first.
fn make_api_routes_cmd<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let mut tenant_aware = false;
    let mut filtered: Vec<String> = Vec::with_capacity(args.len());
    for a in args {
        if a == "--tenant" || a == "--tenant-aware" {
            tenant_aware = true;
        } else if a == "--help" || a == "-h" {
            writeln!(w, "make:api_routes <app> [--tenant]")?;
            writeln!(
                w,
                "  Scaffold src/<app>/api_routes.rs — the per-app router composer."
            )?;
            writeln!(
                w,
                "  Use --tenant for tenancy projects (no PgPool argument; each"
            )?;
            writeln!(
                w,
                "  viewset resolves its own per-request connection via the Tenant"
            )?;
            writeln!(w, "  extractor).")?;
            return Ok(());
        } else if a.starts_with('-') {
            return Err(MigrateError::Validation(format!(
                "make:api_routes: unrecognized flag `{a}`"
            )));
        } else {
            filtered.push(a.clone());
        }
    }
    let app = filtered.first().ok_or_else(|| {
        MigrateError::Validation(
            "app name is required (e.g. `manage make:api_routes regions`)".into(),
        )
    })?;
    if filtered.len() > 1 {
        return Err(MigrateError::Validation(format!(
            "make:api_routes: expected one app name, got {} ({:?})",
            filtered.len(),
            filtered
        )));
    }
    if !is_valid_app_name(app) {
        return Err(MigrateError::Validation(format!(
            "make:api_routes: app name `{app}` must match `[a-z_][a-z0-9_]*`"
        )));
    }

    let app_dir = std::path::PathBuf::from("src").join(app);
    if !app_dir.exists() {
        return Err(MigrateError::Validation(format!(
            "make:api_routes: src/{app}/ does not exist. Run `manage startapp {app}` first."
        )));
    }

    let path = app_dir.join("api_routes.rs");
    if path.exists() {
        return Err(MigrateError::Validation(format!(
            "{} already exists — refusing to overwrite",
            path.display()
        )));
    }

    let body = if tenant_aware {
        api_routes_template_tenant(app)
    } else {
        api_routes_template_pool(app)
    };
    std::fs::write(&path, body)?;
    writeln!(w, "wrote {}", path.display())?;
    writeln!(
        w,
        "  add `pub mod api_routes;` to src/{app}/mod.rs (or `mod ...;`),"
    )?;
    writeln!(
        w,
        "  then `.merge({app}::api_routes::api())` from your top-level urls.rs."
    )?;
    Ok(())
}

fn is_valid_app_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// `--tenant` template — uses each viewset's `tenant_router(...)`,
/// no `PgPool` threaded through `api()` since the per-tenant
/// connection is resolved per-request via the `Tenant` extractor.
fn api_routes_template_tenant(app: &str) -> String {
    format!(
        r#"//! Auto-scaffolded by `manage make:api_routes {app} --tenant`.
//!
//! API routing for the `{app}` app. Per-model viewsets live under
//! `viewsets/` and each exposes a `pub fn viewset() -> ViewSet`;
//! this file composes them into a single `Router<()>`.
//!
//! Adding a resource:
//!   1. Drop a new file under `viewsets/` exposing
//!      `pub fn viewset() -> ViewSet`.
//!   2. Declare it in `viewsets/mod.rs`.
//!   3. Add one `.merge(...)` line below.

use axum::Router;

pub fn api() -> Router<()> {{
    Router::new()
        // .merge(super::viewsets::<model>::viewset().tenant_router("/api/<model>"))
}}
"#
    )
}

/// Default template (no `--tenant`) — single-pool projects. The
/// `pool` argument is threaded into per-model `router(prefix, pool)`
/// calls; the macro-derived ViewSet captures it at mount time.
fn api_routes_template_pool(app: &str) -> String {
    format!(
        r#"//! Auto-scaffolded by `manage make:api_routes {app}`.
//!
//! API routing for the `{app}` app. Composes per-model viewsets
//! into a single `Router<()>`. Each viewset captures the supplied
//! `PgPool` at mount time.
//!
//! Adding a resource:
//!   1. Run `manage make:viewset <Name> --model <Model>`.
//!   2. Add one `.merge(...)` line below.

use axum::Router;
use rustango::sql::sqlx::PgPool;

pub fn api(pool: PgPool) -> Router<()> {{
    let _pool = pool;
    Router::new()
        // .merge(super::viewsets::<snake>::router("/api/<snake>", _pool.clone()))
}}
"#
    )
}

fn make_serializer_cmd<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let (name, model) = parse_name_and_model(args)?;
    let snake = pascal_to_snake(&name);
    let model = model.unwrap_or_else(|| "Post".into());
    let body = format!(
        r#"//! Auto-scaffolded by `manage make:serializer {name}`.

use rustango::Serializer;

#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = {model})]
pub struct {name} {{
    pub id: i64,
    // pub title: String,
    // #[serializer(read_only)]
    // pub created_at: chrono::DateTime<chrono::Utc>,
}}
"#
    );
    write_generated(w, &format!("{snake}.rs"), body)
}

fn make_form_cmd<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let (name, _) = parse_name_and_model(args)?;
    let snake = pascal_to_snake(&name);
    let body = format!(
        r#"//! Auto-scaffolded by `manage make:form {name}`.

use rustango::forms::Form;
use rustango::Form as DeriveForm;

#[derive(DeriveForm)]
pub struct {name} {{
    #[form(min_length = 1, max_length = 200)]
    pub title: String,
    pub body: Option<String>,
}}
"#
    );
    write_generated(w, &format!("{snake}.rs"), body)
}

fn make_job_cmd<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let (name, _) = parse_name_and_model(args)?;
    let snake = pascal_to_snake(&name);
    let body = format!(
        r#"//! Auto-scaffolded by `manage make:job {name}`.
//!
//! Background job — run async work outside the request lifecycle.
//! Pair with `rustango::scheduler::Scheduler` (cron-shape) or your queue layer.

use std::sync::Arc;
use rustango::sql::sqlx::PgPool;

pub struct {name} {{
    pub pool: PgPool,
}}

impl {name} {{
    pub async fn run(self: Arc<Self>) {{
        // TODO: implement
        let _ = self.pool.acquire().await;
    }}
}}

// Wire up in main.rs:
//
//   let job = Arc::new({name} {{ pool: pool.clone() }});
//   scheduler.every("{snake}", Duration::from_secs(60), move || {{
//       let job = job.clone();
//       async move {{ job.run().await }}
//   }});
"#
    );
    write_generated(w, &format!("{snake}.rs"), body)
}

fn make_notification_cmd<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let (name, _) = parse_name_and_model(args)?;
    let snake = pascal_to_snake(&name);
    let body = format!(
        r#"//! Auto-scaffolded by `manage make:notification {name}`.
//!
//! User-facing notification. For now this just builds an Email; once the
//! `rustango::notifications` layer ships you'll add `via()` for multi-channel.

use rustango::email::Email;

pub struct {name} {{
    pub user_email: String,
    pub subject: String,
}}

impl {name} {{
    pub fn build_email(&self) -> Email {{
        Email::new()
            .to(&self.user_email)
            .from("noreply@example.com")
            .subject(&self.subject)
            .body("Hello — this notification was generated by {name}.")
    }}
}}
"#
    );
    write_generated(w, &format!("{snake}.rs"), body)
}

fn make_middleware_cmd<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let (name, _) = parse_name_and_model(args)?;
    let snake = pascal_to_snake(&name);
    let body = format!(
        r#"//! Auto-scaffolded by `manage make:middleware {name}`.

use axum::body::Body;
use axum::http::{{Request, Response}};
use axum::middleware::Next;

pub async fn {snake}(req: Request<Body>, next: Next) -> Response<Body> {{
    // TODO: pre-handler logic
    let response = next.run(req).await;
    // TODO: post-handler logic
    response
}}

// Apply with:
//   router.layer(axum::middleware::from_fn({snake}))
"#
    );
    write_generated(w, &format!("{snake}.rs"), body)
}

fn make_test_cmd<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let (name, _) = parse_name_and_model(args)?;
    let snake = pascal_to_snake(&name);
    let body = format!(
        r#"//! Auto-scaffolded by `manage make:test {name}`.
//!
//! Integration test. Run with `cargo test --test {snake}`.

use rustango::test_client::TestClient;
use axum::Router;
use axum::routing::get;

fn app() -> Router {{
    Router::new().route("/hello", get(|| async {{ "hi" }}))
}}

#[tokio::test]
async fn {snake}_smoke() {{
    let client = TestClient::new(app());
    let r = client.get("/hello").send().await;
    assert_eq!(r.status, 200);
    assert_eq!(r.text(), "hi");
}}
"#
    );
    // Tests live in tests/ not src/
    let path = std::path::PathBuf::from("tests").join(format!("{snake}.rs"));
    if path.exists() {
        return Err(MigrateError::Validation(format!(
            "{} already exists — refusing to overwrite",
            path.display()
        )));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, body)?;
    writeln!(w, "wrote {}", path.display())?;
    Ok(())
}

// =====================================================================
// db:dump / db:restore — shell out to pg_dump / psql
// =====================================================================

#[derive(Debug, PartialEq)]
struct DbDumpArgs {
    out: Option<String>,
    data_only: bool,
    schema_only: bool,
    no_owner: bool,
}

fn parse_db_dump_args(args: &[String]) -> Result<DbDumpArgs, MigrateError> {
    let mut out: Option<String> = None;
    let mut data_only = false;
    let mut schema_only = false;
    let mut no_owner = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--out" | "-o" => {
                out = Some(
                    iter.next()
                        .cloned()
                        .ok_or_else(|| MigrateError::Validation("--out requires a path".into()))?,
                );
            }
            "--data-only" => data_only = true,
            "--schema-only" => schema_only = true,
            "--no-owner" => no_owner = true,
            other => {
                return Err(MigrateError::Validation(format!("unknown flag: {other}")));
            }
        }
    }
    if data_only && schema_only {
        return Err(MigrateError::Validation(
            "--data-only and --schema-only are mutually exclusive".into(),
        ));
    }
    Ok(DbDumpArgs {
        out,
        data_only,
        schema_only,
        no_owner,
    })
}

/// Build the argument vector for pg_dump given parsed args + database
/// URL. Pure function — easy to test.
fn build_pg_dump_argv(parsed: &DbDumpArgs, database_url: &str) -> Vec<String> {
    let mut argv = vec![database_url.to_owned()];
    if parsed.data_only {
        argv.push("--data-only".into());
    }
    if parsed.schema_only {
        argv.push("--schema-only".into());
    }
    if parsed.no_owner {
        argv.push("--no-owner".into());
    }
    if let Some(out) = &parsed.out {
        argv.push("--file".into());
        argv.push(out.clone());
    }
    argv
}

fn db_dump_cmd<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let parsed = parse_db_dump_args(args)?;
    let url = std::env::var("DATABASE_URL").map_err(|_| {
        MigrateError::Validation(
            "DATABASE_URL must be set for db:dump (e.g. \
             postgres://user:pass@host:5432/db)"
                .into(),
        )
    })?;
    let argv = build_pg_dump_argv(&parsed, &url);
    writeln!(w, "running: pg_dump {}", redact(&argv).join(" "))?;
    let status = std::process::Command::new("pg_dump")
        .args(&argv)
        .status()
        .map_err(|e| {
            MigrateError::Validation(format!("could not run pg_dump (is it on PATH?): {e}"))
        })?;
    if !status.success() {
        return Err(MigrateError::Validation(format!(
            "pg_dump exited with status {status}"
        )));
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
struct DbRestoreArgs {
    file: String,
    clean: bool,
}

fn parse_db_restore_args(args: &[String]) -> Result<DbRestoreArgs, MigrateError> {
    let mut file: Option<String> = None;
    let mut clean = false;
    for arg in args {
        match arg.as_str() {
            "--clean" => clean = true,
            other if other.starts_with('-') => {
                return Err(MigrateError::Validation(format!("unknown flag: {other}")));
            }
            other => {
                if file.is_some() {
                    return Err(MigrateError::Validation(format!(
                        "unexpected argument: {other}"
                    )));
                }
                file = Some(other.to_owned());
            }
        }
    }
    let file = file.ok_or_else(|| {
        MigrateError::Validation("db:restore <file> requires a dump file path".into())
    })?;
    Ok(DbRestoreArgs { file, clean })
}

/// Build the psql argv given parsed args + URL. Pure function — easy
/// to test.
fn build_psql_argv(parsed: &DbRestoreArgs, database_url: &str) -> Vec<String> {
    let mut argv = vec![database_url.to_owned()];
    // -v ON_ERROR_STOP=1 makes psql exit non-zero on the first SQL
    // error, instead of plowing through and "succeeding" with garbage.
    argv.push("-v".into());
    argv.push("ON_ERROR_STOP=1".into());
    if parsed.clean {
        argv.push("-c".into());
        argv.push("DROP SCHEMA IF EXISTS public CASCADE; CREATE SCHEMA public;".into());
    }
    argv.push("-f".into());
    argv.push(parsed.file.clone());
    argv
}

fn db_restore_cmd<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let parsed = parse_db_restore_args(args)?;
    let url = std::env::var("DATABASE_URL").map_err(|_| {
        MigrateError::Validation(
            "DATABASE_URL must be set for db:restore (e.g. \
             postgres://user:pass@host:5432/db)"
                .into(),
        )
    })?;
    let argv = build_psql_argv(&parsed, &url);
    writeln!(w, "running: psql {}", redact(&argv).join(" "))?;
    let status = std::process::Command::new("psql")
        .args(&argv)
        .status()
        .map_err(|e| {
            MigrateError::Validation(format!("could not run psql (is it on PATH?): {e}"))
        })?;
    if !status.success() {
        return Err(MigrateError::Validation(format!(
            "psql exited with status {status}"
        )));
    }
    Ok(())
}

/// `db:info` — read-only summary of the DB configuration this build
/// would use. Shows the resolved URL (password redacted), the detected
/// backend, and which `postgres`/`mysql` Cargo features are compiled
/// in. Does not connect — handy in CI / containers where DB might not
/// be reachable yet but you still want to confirm the runtime sees
/// the expected backend.
fn db_info_cmd<W: Write>(w: &mut W) -> Result<(), MigrateError> {
    writeln!(w, "rustango db:info")?;
    writeln!(w, "  framework version:  {}", env!("CARGO_PKG_VERSION"))?;

    let pg_enabled = cfg!(feature = "postgres");
    let mysql_enabled = cfg!(feature = "mysql");
    writeln!(
        w,
        "  postgres feature:   {}",
        if pg_enabled { "enabled" } else { "disabled" }
    )?;
    writeln!(
        w,
        "  mysql feature:      {} (impl lands in v0.23.0-batch2)",
        if mysql_enabled { "enabled" } else { "disabled" }
    )?;

    match crate::env::database_url_from_env() {
        Ok(url) => {
            let scheme = url.split("://").next().unwrap_or("(unknown)");
            writeln!(w, "  resolved URL:       {}", redact_url(&url))?;
            writeln!(w, "  detected backend:   {scheme}")?;
            // Soft warning when scheme + feature don't line up — caught
            // at runtime by Pool::connect, but this surfaces it before
            // the operator tries to start the server.
            match scheme {
                "postgres" | "postgresql" if !pg_enabled => {
                    writeln!(
                        w,
                        "  ! warning: URL is postgres but the `postgres` feature is disabled — \
                         add `features = [\"postgres\"]` to rustango"
                    )?;
                }
                "mysql" if !mysql_enabled => {
                    writeln!(
                        w,
                        "  ! warning: URL is mysql but the `mysql` feature is disabled — \
                         add `features = [\"mysql\"]` to rustango"
                    )?;
                }
                "mysql" if mysql_enabled => {
                    writeln!(
                        w,
                        "  ! note: MySql connections will fail in v0.23.0-batch1 \
                         (MySqlDialect lands in batch2)"
                    )?;
                }
                _ => {}
            }
        }
        Err(e) => {
            writeln!(w, "  resolved URL:       (none — {e})")?;
            writeln!(
                w,
                "  hint:               set DATABASE_URL or DB_USER+DB_NAME (+optional \
                 DB_HOST/DB_PORT/DB_PASSWORD/DB_DRIVER/DB_PARAMS)"
            )?;
        }
    }
    Ok(())
}

/// `manage dumpdata [--model app.Name] [--indent N]` — fixture
/// export. Iterates every model registered in `inventory` and
/// emits a Django-shape JSON array:
///
/// ```json
/// [
///   {"model": "blog.Article", "pk": 1, "fields": {"title": "...", ...}},
///   {"model": "blog.Article", "pk": 2, "fields": {"title": "...", ...}}
/// ]
/// ```
///
/// `model` defaults to the resolved app label + schema name. Pass
/// `--model <name>` to limit to a single model (full `app.Model`
/// name OR bare model name); pass multiple times to limit to a set.
/// `--indent N` controls JSON formatting (default `2`).
///
/// Output goes to stdout so users can pipe to a file:
/// `cargo run manage dumpdata > fixtures/seed.json`.
///
/// `loaddata` is the companion verb that re-applies a fixture
/// export — queued as a follow-up.
#[derive(Debug, Default, PartialEq)]
struct DumpdataArgs {
    /// Limit to these `app.Model` or `Model` names. Empty = every model.
    model_filters: Vec<String>,
    /// JSON indent. `0` = compact single-line; otherwise pretty (2-space).
    indent: usize,
    /// `true` when the user passed `--help`; cmd short-circuits to help.
    help: bool,
}

fn parse_dumpdata_args(args: &[String]) -> Result<DumpdataArgs, MigrateError> {
    let mut out = DumpdataArgs {
        indent: 2,
        ..Default::default()
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                out.help = true;
                return Ok(out);
            }
            "--model" => {
                let v = iter
                    .next()
                    .ok_or_else(|| MigrateError::Validation("--model expects a value".into()))?;
                out.model_filters.push(v.clone());
            }
            "--indent" => {
                let v = iter
                    .next()
                    .ok_or_else(|| MigrateError::Validation("--indent expects a value".into()))?;
                out.indent = v.parse().map_err(|e| {
                    MigrateError::Validation(format!("--indent: not an integer ({e})"))
                })?;
            }
            other if other.starts_with('-') => {
                return Err(MigrateError::Validation(format!("unknown flag: {other}")));
            }
            other => {
                return Err(MigrateError::Validation(format!(
                    "unexpected positional argument: {other}"
                )));
            }
        }
    }
    Ok(out)
}

async fn dumpdata_cmd<W: Write>(
    pool: &Pool,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let parsed = parse_dumpdata_args(args)?;
    if parsed.help {
        writeln!(w, "dumpdata [--model app.Name] [--indent N]")?;
        writeln!(w)?;
        writeln!(
            w,
            "  Export every registered model's rows as JSON in Django fixture"
        )?;
        writeln!(
            w,
            "  shape: `[{{\"model\": \"app.Model\", \"pk\": N, \"fields\": {{...}}}}]`."
        )?;
        writeln!(w)?;
        writeln!(
            w,
            "  --model <name>   Limit to a single model. Accepts either the full"
        )?;
        writeln!(
            w,
            "                   `app.Model` shape or the bare model name. Pass"
        )?;
        writeln!(
            w,
            "                   the flag multiple times to limit to a set."
        )?;
        writeln!(
            w,
            "  --indent <N>     JSON indent (default 2; 0 emits compact single-line)."
        )?;
        return Ok(());
    }
    let model_filters = &parsed.model_filters;
    let indent = parsed.indent;

    // Walk every registered model, fetching rows from each.
    let mut out: Vec<serde_json::Value> = Vec::new();
    for entry in inventory::iter::<crate::core::ModelEntry>() {
        let schema = entry.schema;
        let app_label = entry.resolved_app_label().unwrap_or("");
        let dotted_name = if app_label.is_empty() {
            schema.name.to_owned()
        } else {
            format!("{app_label}.{}", schema.name)
        };

        // Filter: if the user specified --model, only emit models
        // whose `app.Model` or bare `Model` matches.
        if !model_filters.is_empty()
            && !model_filters
                .iter()
                .any(|f| f == &dotted_name || f == schema.name)
        {
            continue;
        }

        // Identify the PK column for fixture `pk` extraction.
        let pk_field = schema.primary_key();

        // Build a minimal "select every column" SelectQuery.
        let fields: Vec<&'static crate::core::FieldSchema> = schema.scalar_fields().collect();
        let query = crate::core::SelectQuery {
            model: schema,
            where_clause: crate::core::WhereExpr::And(vec![]),
            search: None,
            joins: vec![],
            order_by: vec![],
            limit: None,
            offset: None,
            lock_mode: None,
            compound: vec![],
            projection: None,
            distinct: None,
        };

        let rows = crate::sql::select_rows_as_json(pool, &query, &fields)
            .await
            .map_err(|e| {
                MigrateError::Validation(format!(
                    "dumpdata: select from `{}` failed: {e}",
                    schema.table
                ))
            })?;

        for mut row in rows {
            // Pop the PK column off `fields` into the outer fixture
            // entry's `pk` slot — Django fixtures separate identity
            // from payload.
            let pk_value = match pk_field {
                Some(pk) => row
                    .as_object_mut()
                    .and_then(|m| m.remove(pk.name))
                    .unwrap_or(serde_json::Value::Null),
                None => serde_json::Value::Null,
            };
            out.push(serde_json::json!({
                "model": dotted_name,
                "pk": pk_value,
                "fields": row,
            }));
        }
    }

    let rendered = if indent == 0 {
        serde_json::to_string(&out)
    } else {
        serde_json::to_string_pretty(&out)
    }
    .map_err(|e| MigrateError::Validation(format!("dumpdata: serialize JSON: {e}")))?;
    writeln!(w, "{rendered}")?;
    Ok(())
}

#[derive(Debug, PartialEq)]
struct LoaddataArgs {
    /// Path to the fixture JSON file.
    file: String,
    /// Stop on the first error (`true`) vs. log + continue (`false`,
    /// default).
    fail_fast: bool,
    /// Help short-circuit.
    help: bool,
}

fn parse_loaddata_args(args: &[String]) -> Result<LoaddataArgs, MigrateError> {
    let mut file: Option<String> = None;
    let mut fail_fast = false;
    let mut help = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                help = true;
                break;
            }
            "--fail-fast" => fail_fast = true,
            other if other.starts_with('-') => {
                return Err(MigrateError::Validation(format!("unknown flag: {other}")));
            }
            other => {
                if file.is_some() {
                    return Err(MigrateError::Validation(format!(
                        "unexpected positional argument: {other}"
                    )));
                }
                file = Some(other.to_owned());
            }
        }
    }
    if help {
        return Ok(LoaddataArgs {
            file: String::new(),
            fail_fast: false,
            help: true,
        });
    }
    let file = file.ok_or_else(|| {
        MigrateError::Validation("loaddata: missing <fixture.json> argument".into())
    })?;
    Ok(LoaddataArgs {
        file,
        fail_fast,
        help: false,
    })
}

/// `manage loaddata <fixture.json> [--fail-fast]` — companion to
/// `dumpdata`. Reads a Django-shape fixture array and inserts each
/// row via [`crate::sql::insert_pool`]. Models are resolved by
/// `inventory` lookup against the `"app.Model"` name in the fixture.
///
/// Default behaviour: log + skip rows that fail (unknown model,
/// JSON shape mismatch, INSERT error). `--fail-fast` aborts on the
/// first failure.
///
/// JSON values map onto rustango's `SqlValue` types by field kind
/// declared in the target model's schema:
/// - integer / float JSON → numeric `SqlValue` per `FieldType`
/// - boolean → `SqlValue::Bool`
/// - string → parsed per `FieldType` (Decimal / Date / DateTime /
///   Time / Uuid / Binary [hex] / String)
/// - null → `SqlValue::Null`
/// - object / array → `SqlValue::Json`
async fn loaddata_cmd<W: Write>(
    pool: &Pool,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let parsed = parse_loaddata_args(args)?;
    if parsed.help {
        writeln!(w, "loaddata <fixture.json> [--fail-fast]")?;
        writeln!(w)?;
        writeln!(
            w,
            "  Insert every row in `fixture.json` via the registered model schemas."
        )?;
        writeln!(
            w,
            "  Fixture shape: `[{{\"model\": \"app.Model\", \"pk\": N, \"fields\": {{...}}}}]`"
        )?;
        writeln!(w, "  — the same shape `manage dumpdata` produces.")?;
        writeln!(w)?;
        writeln!(
            w,
            "  --fail-fast   Abort on the first error instead of skipping the row."
        )?;
        return Ok(());
    }

    let raw = std::fs::read_to_string(&parsed.file)
        .map_err(|e| MigrateError::Validation(format!("loaddata: read `{}`: {e}", parsed.file)))?;
    let entries: Vec<serde_json::Value> = serde_json::from_str(&raw).map_err(|e| {
        MigrateError::Validation(format!("loaddata: parse `{}` as JSON: {e}", parsed.file))
    })?;

    // Build a lookup map: "app.Model" + bare "Model" → &'static ModelSchema.
    let mut schemas: std::collections::HashMap<String, &'static crate::core::ModelSchema> =
        std::collections::HashMap::new();
    for entry in inventory::iter::<crate::core::ModelEntry>() {
        let schema = entry.schema;
        let app = entry.resolved_app_label().unwrap_or("");
        if !app.is_empty() {
            schemas.insert(format!("{app}.{}", schema.name), schema);
        }
        schemas.insert(schema.name.to_owned(), schema);
    }

    let mut loaded = 0_usize;
    let mut skipped = 0_usize;
    for (idx, entry) in entries.into_iter().enumerate() {
        let line = idx + 1;
        let model_name = entry.get("model").and_then(|v| v.as_str()).unwrap_or("");
        let schema = match schemas.get(model_name) {
            Some(s) => *s,
            None => {
                let msg = format!(
                    "loaddata: entry #{line}: unknown model `{model_name}` (registered: {} models)",
                    schemas.len() / 2, // dotted + bare keys
                );
                if parsed.fail_fast {
                    return Err(MigrateError::Validation(msg));
                }
                tracing::warn!("{msg}");
                skipped += 1;
                continue;
            }
        };

        // Build column/value vectors. PK rides at the top level of
        // the fixture entry, so we re-glue it into the fields map.
        let mut fields_obj: serde_json::Map<String, serde_json::Value> = entry
            .get("fields")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        if let Some(pk_value) = entry.get("pk").cloned() {
            if let Some(pk_field) = schema.primary_key() {
                fields_obj.insert(pk_field.name.to_owned(), pk_value);
            }
        }

        let mut columns: Vec<&'static str> = Vec::new();
        let mut values: Vec<crate::core::SqlValue> = Vec::new();
        let mut row_err: Option<String> = None;
        for f in schema.scalar_fields() {
            let raw = fields_obj
                .get(f.name)
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            match json_to_sql_value(&raw, f) {
                Ok(v) => {
                    columns.push(f.column);
                    values.push(v);
                }
                Err(e) => {
                    row_err = Some(format!("field `{}`: {e}", f.name));
                    break;
                }
            }
        }
        if let Some(e) = row_err {
            let msg = format!("loaddata: entry #{line} (`{model_name}`): {e}");
            if parsed.fail_fast {
                return Err(MigrateError::Validation(msg));
            }
            tracing::warn!("{msg}");
            skipped += 1;
            continue;
        }

        let query = crate::core::InsertQuery {
            model: schema,
            columns,
            values,
            returning: vec![],
            on_conflict: None,
        };
        match crate::sql::insert_pool(pool, &query).await {
            Ok(()) => loaded += 1,
            Err(e) => {
                let msg = format!("loaddata: entry #{line} (`{model_name}`) insert: {e}");
                if parsed.fail_fast {
                    return Err(MigrateError::Validation(msg));
                }
                tracing::warn!("{msg}");
                skipped += 1;
            }
        }
    }

    writeln!(w, "loaddata: {loaded} loaded, {skipped} skipped")?;
    Ok(())
}

/// Map a JSON value onto an [`crate::core::SqlValue`] using the target
/// field's declared [`crate::core::FieldType`].
///
/// String inputs are reparsed for typed fields:
/// - Decimal: `rust_decimal::Decimal::from_str`
/// - Date: `chrono::NaiveDate::parse_from_str("%Y-%m-%d")`
/// - DateTime: RFC 3339 / `%Y-%m-%dT%H:%M:%S`
/// - Time: `%H:%M:%S` then `%H:%M`
/// - Uuid: `Uuid::parse_str`
/// - Binary: lowercase hex
///
/// Object / Array JSON nodes always land as `SqlValue::Json`.
fn json_to_sql_value(
    v: &serde_json::Value,
    field: &crate::core::FieldSchema,
) -> Result<crate::core::SqlValue, String> {
    use crate::core::{FieldType, SqlValue};
    if v.is_null() {
        return Ok(SqlValue::Null);
    }
    match field.ty {
        FieldType::I16 => v
            .as_i64()
            .and_then(|n| i16::try_from(n).ok())
            .map(SqlValue::I16)
            .ok_or_else(|| format!("expected i16, got {v}")),
        FieldType::I32 => v
            .as_i64()
            .and_then(|n| i32::try_from(n).ok())
            .map(SqlValue::I32)
            .ok_or_else(|| format!("expected i32, got {v}")),
        FieldType::I64 => v.as_i64().map(SqlValue::I64).ok_or_else(|| {
            // Accept integer-shaped strings too (e.g. SQLite NUMERIC).
            v.as_str()
                .and_then(|s| s.parse::<i64>().ok())
                .map(SqlValue::I64)
                .map(|_| format!("expected i64, got {v}"))
                .unwrap_or_else(|| format!("expected i64, got {v}"))
        }),
        FieldType::F32 => v
            .as_f64()
            .map(|n| SqlValue::F32(n as f32))
            .ok_or_else(|| format!("expected f32, got {v}")),
        FieldType::F64 => v
            .as_f64()
            .map(SqlValue::F64)
            .ok_or_else(|| format!("expected f64, got {v}")),
        FieldType::Bool => v
            .as_bool()
            .map(SqlValue::Bool)
            .ok_or_else(|| format!("expected bool, got {v}")),
        FieldType::String => v
            .as_str()
            .map(|s| SqlValue::String(s.to_owned()))
            .ok_or_else(|| format!("expected string, got {v}")),
        FieldType::DateTime => {
            let s = v
                .as_str()
                .ok_or_else(|| format!("expected string for DateTime, got {v}"))?;
            chrono::DateTime::parse_from_rfc3339(s)
                .map(|d| SqlValue::DateTime(d.with_timezone(&chrono::Utc)))
                .or_else(|_| {
                    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
                        .map(|ndt| SqlValue::DateTime(ndt.and_utc()))
                })
                .map_err(|e| format!("DateTime parse: {e}"))
        }
        FieldType::Date => {
            let s = v
                .as_str()
                .ok_or_else(|| format!("expected string for Date, got {v}"))?;
            chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map(SqlValue::Date)
                .map_err(|e| format!("Date parse: {e}"))
        }
        FieldType::Time => {
            let s = v
                .as_str()
                .ok_or_else(|| format!("expected string for Time, got {v}"))?;
            chrono::NaiveTime::parse_from_str(s, "%H:%M:%S")
                .or_else(|_| chrono::NaiveTime::parse_from_str(s, "%H:%M"))
                .map(SqlValue::Time)
                .map_err(|e| format!("Time parse: {e}"))
        }
        FieldType::Uuid => {
            let s = v
                .as_str()
                .ok_or_else(|| format!("expected string for Uuid, got {v}"))?;
            uuid::Uuid::parse_str(s)
                .map(SqlValue::Uuid)
                .map_err(|e| format!("Uuid parse: {e}"))
        }
        FieldType::Json => Ok(SqlValue::Json(v.clone())),
        FieldType::Decimal => {
            let s = v
                .as_str()
                .map(str::to_owned)
                .or_else(|| v.as_f64().map(|n| n.to_string()))
                .or_else(|| v.as_i64().map(|n| n.to_string()))
                .ok_or_else(|| format!("expected number/string for Decimal, got {v}"))?;
            s.parse::<rust_decimal::Decimal>()
                .map(SqlValue::Decimal)
                .map_err(|e| format!("Decimal parse: {e}"))
        }
        FieldType::Binary => {
            let s = v
                .as_str()
                .ok_or_else(|| format!("expected lowercase-hex string for Binary, got {v}"))?;
            if s.len() % 2 != 0 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("Binary: not a valid lowercase-hex string".into());
            }
            let bytes: Vec<u8> = s
                .as_bytes()
                .chunks_exact(2)
                .map(|c| {
                    let h = (c[0] as char).to_digit(16).unwrap_or(0) as u8;
                    let l = (c[1] as char).to_digit(16).unwrap_or(0) as u8;
                    (h << 4) | l
                })
                .collect();
            Ok(SqlValue::Binary(bytes))
        }
    }
}

/// `manage showurls [--format <plain|json>]` — print every named
/// URL pattern registered via `register_url!`. Django parity verb.
///
/// Defaults to plain two-column output (name, pattern). `--format
/// json` emits a JSON array of `{"name": "...", "pattern": "..."}`
/// for machine consumption.
///
/// Sorted by name for deterministic output.
fn showurls_cmd<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let mut format = "plain";
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                writeln!(w, "showurls [--format <plain|json>]")?;
                writeln!(w)?;
                writeln!(
                    w,
                    "  Print every named URL pattern registered via `register_url!`."
                )?;
                writeln!(w, "  --format plain (default) | json")?;
                return Ok(());
            }
            "--format" => {
                let v = iter
                    .next()
                    .ok_or_else(|| MigrateError::Validation("--format expects a value".into()))?;
                match v.as_str() {
                    "plain" | "json" => {
                        format = match v.as_str() {
                            "plain" => "plain",
                            "json" => "json",
                            _ => unreachable!(),
                        }
                    }
                    other => {
                        return Err(MigrateError::Validation(format!(
                            "--format: unknown value `{other}` (expected `plain` or `json`)"
                        )));
                    }
                }
            }
            other if other.starts_with('-') => {
                return Err(MigrateError::Validation(format!("unknown flag: {other}")));
            }
            other => {
                return Err(MigrateError::Validation(format!(
                    "unexpected positional argument: {other}"
                )));
            }
        }
    }

    let mut routes: Vec<&'static crate::urls::NamedRoute> =
        inventory::iter::<crate::urls::NamedRoute>().collect();
    routes.sort_by_key(|r| r.name);

    match format {
        "json" => {
            let items: Vec<serde_json::Value> = routes
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "name": r.name,
                        "pattern": r.pattern,
                    })
                })
                .collect();
            let rendered = serde_json::to_string_pretty(&items)
                .map_err(|e| MigrateError::Validation(format!("showurls: serialize JSON: {e}")))?;
            writeln!(w, "{rendered}")?;
        }
        _ => {
            // plain
            if routes.is_empty() {
                writeln!(w, "(no named URLs registered)")?;
                return Ok(());
            }
            let max_name = routes.iter().map(|r| r.name.len()).max().unwrap_or(0);
            for r in &routes {
                writeln!(w, "  {:<width$}  {}", r.name, r.pattern, width = max_name)?;
            }
        }
    }
    Ok(())
}

/// `manage showmodels [--format plain|json] [--app <label>]` —
/// print every model registered via `#[derive(Model)]` + `inventory`.
/// Useful for confirming model registration in CI / debugging
/// "where did my model go?" / cross-checking the admin sidebar
/// against the inventory.
///
/// - `--format plain` (default): one row per model. Columns are
///   app, model name, table, field count.
/// - `--format json`: JSON array of `{"app", "name", "table",
///   "fields"}` for piping to `jq`.
/// - `--app <label>`: filter to one app's models. Useful when the
///   binary registers many apps.
///
/// Sorted by `(app, name)` for deterministic output.
fn showmodels_cmd<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let mut format = "plain";
    let mut app_filter: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                writeln!(w, "showmodels [--format plain|json] [--app <label>]")?;
                writeln!(w)?;
                writeln!(
                    w,
                    "  Print every model registered via #[derive(Model)] + inventory."
                )?;
                writeln!(w, "  --format plain (default) | json")?;
                writeln!(w, "  --app <label>          Limit to a single app.")?;
                return Ok(());
            }
            "--format" => {
                let v = iter
                    .next()
                    .ok_or_else(|| MigrateError::Validation("--format expects a value".into()))?;
                match v.as_str() {
                    "plain" => format = "plain",
                    "json" => format = "json",
                    other => {
                        return Err(MigrateError::Validation(format!(
                            "--format: unknown value `{other}` (expected `plain` or `json`)"
                        )));
                    }
                }
            }
            "--app" => {
                let v = iter
                    .next()
                    .ok_or_else(|| MigrateError::Validation("--app expects a value".into()))?;
                app_filter = Some(v.clone());
            }
            other if other.starts_with('-') => {
                return Err(MigrateError::Validation(format!("unknown flag: {other}")));
            }
            other => {
                return Err(MigrateError::Validation(format!(
                    "unexpected positional argument: {other}"
                )));
            }
        }
    }

    // Collect + filter + sort.
    #[derive(Debug)]
    struct Row {
        app: String,
        name: &'static str,
        table: &'static str,
        fields: usize,
    }
    let mut rows: Vec<Row> = inventory::iter::<crate::core::ModelEntry>()
        .map(|entry| Row {
            app: entry.resolved_app_label().unwrap_or("").to_owned(),
            name: entry.schema.name,
            table: entry.schema.table,
            fields: entry.schema.fields.len(),
        })
        .filter(|r| app_filter.as_deref().is_none_or(|f| r.app == f))
        .collect();
    rows.sort_by(|a, b| (a.app.as_str(), a.name).cmp(&(b.app.as_str(), b.name)));

    match format {
        "json" => {
            let items: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "app": r.app,
                        "name": r.name,
                        "table": r.table,
                        "fields": r.fields,
                    })
                })
                .collect();
            let rendered = serde_json::to_string_pretty(&items).map_err(|e| {
                MigrateError::Validation(format!("showmodels: serialize JSON: {e}"))
            })?;
            writeln!(w, "{rendered}")?;
        }
        _ => {
            if rows.is_empty() {
                let suffix = app_filter
                    .as_deref()
                    .map(|f| format!(" for app `{f}`"))
                    .unwrap_or_default();
                writeln!(w, "(no models registered{suffix})")?;
                return Ok(());
            }
            let max_app = rows.iter().map(|r| r.app.len()).max().unwrap_or(0).max(3); // "app"
            let max_name = rows.iter().map(|r| r.name.len()).max().unwrap_or(0).max(5);
            let max_table = rows.iter().map(|r| r.table.len()).max().unwrap_or(0).max(5);
            for r in &rows {
                let app = if r.app.is_empty() {
                    "-"
                } else {
                    r.app.as_str()
                };
                writeln!(
                    w,
                    "  {:<aw$}  {:<nw$}  {:<tw$}  {} fields",
                    app,
                    r.name,
                    r.table,
                    r.fields,
                    aw = max_app,
                    nw = max_name,
                    tw = max_table,
                )?;
            }
        }
    }
    Ok(())
}

#[derive(Debug, Default, PartialEq)]
struct FlushArgs {
    /// `--yes` confirms data destruction. Without it, the command
    /// prints what would happen (dry-run) and exits.
    yes: bool,
    /// Limit to a specific app or set of apps.
    apps: Vec<String>,
    /// Limit to a specific model or set of models.
    models: Vec<String>,
    help: bool,
}

fn parse_flush_args(args: &[String]) -> Result<FlushArgs, MigrateError> {
    let mut out = FlushArgs::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                out.help = true;
                return Ok(out);
            }
            "--yes" => out.yes = true,
            "--app" => {
                let v = iter
                    .next()
                    .ok_or_else(|| MigrateError::Validation("--app expects a value".into()))?;
                out.apps.push(v.clone());
            }
            "--model" => {
                let v = iter
                    .next()
                    .ok_or_else(|| MigrateError::Validation("--model expects a value".into()))?;
                out.models.push(v.clone());
            }
            other if other.starts_with('-') => {
                return Err(MigrateError::Validation(format!("unknown flag: {other}")));
            }
            other => {
                return Err(MigrateError::Validation(format!(
                    "unexpected positional argument: {other}"
                )));
            }
        }
    }
    Ok(out)
}

/// `manage flush [--yes] [--app <label>] [--model <name>]` — wipe
/// all rows from registered model tables. Django parity verb.
/// Without `--yes`, prints what would happen and exits without
/// touching the database (dry-run by default — a hand-typed
/// `manage flush` doesn't accidentally nuke production).
///
/// On PG, emits `TRUNCATE table1, table2, ... RESTART IDENTITY
/// CASCADE` in a single statement so FK constraints resolve and
/// sequences reset. On MySQL/SQLite, emits per-table `DELETE FROM
/// <table>` in registration order; sequences are NOT reset
/// (caller can `DROP SEQUENCE` + `CREATE SEQUENCE` manually if
/// they need that). The migrations ledger is left untouched —
/// flush wipes data, not schema or schema history.
///
/// `--app <label>` / `--model <name>` filters narrow the wipe.
/// Pass either flag multiple times to limit to a set.
async fn flush_cmd<W: Write>(pool: &Pool, args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let parsed = parse_flush_args(args)?;
    if parsed.help {
        writeln!(w, "flush [--yes] [--app <label>] [--model <name>]")?;
        writeln!(w)?;
        writeln!(
            w,
            "  Wipe all rows from registered model tables. Schema + migrations ledger"
        )?;
        writeln!(w, "  stay intact.")?;
        writeln!(w)?;
        writeln!(
            w,
            "  --yes              Confirm. Without this, prints the planned action"
        )?;
        writeln!(w, "                     and exits without touching the DB.")?;
        writeln!(w, "  --app <label>      Limit to one app (repeatable).")?;
        writeln!(w, "  --model <name>     Limit to one model (repeatable).")?;
        return Ok(());
    }

    // Collect target tables in inventory order.
    let mut targets: Vec<&'static str> = Vec::new();
    for entry in inventory::iter::<crate::core::ModelEntry>() {
        let schema = entry.schema;
        let app = entry.resolved_app_label().unwrap_or("");
        let dotted = if app.is_empty() {
            schema.name.to_owned()
        } else {
            format!("{app}.{}", schema.name)
        };
        if !parsed.apps.is_empty() && !parsed.apps.iter().any(|a| a == app) {
            continue;
        }
        if !parsed.models.is_empty()
            && !parsed
                .models
                .iter()
                .any(|m| m == &dotted || m == schema.name)
        {
            continue;
        }
        targets.push(schema.table);
    }
    if targets.is_empty() {
        writeln!(w, "flush: no tables match the filter (nothing to do)")?;
        return Ok(());
    }

    if !parsed.yes {
        writeln!(
            w,
            "flush: would clear {} table(s) (run with --yes to execute):",
            targets.len()
        )?;
        for t in &targets {
            writeln!(w, "  - {t}")?;
        }
        return Ok(());
    }

    // Execute. Per-dialect strategy:
    let dialect = pool.dialect().name();
    let mut cleared = 0_usize;
    let mut failures: Vec<(String, String)> = Vec::new();
    if dialect == "postgres" {
        // One big TRUNCATE — atomic, FK-aware, sequence-resetting.
        let quoted: Vec<String> = targets
            .iter()
            .map(|t| format!(r#""{}""#, t.replace('"', r#""""#)))
            .collect();
        let sql = format!(
            "TRUNCATE TABLE {} RESTART IDENTITY CASCADE",
            quoted.join(", "),
        );
        match crate::sql::raw_execute_pool(pool, &sql, Vec::new()).await {
            Ok(_) => cleared = targets.len(),
            Err(e) => failures.push(("TRUNCATE".to_owned(), e.to_string())),
        }
    } else {
        // MySQL / SQLite: per-table DELETE in registration order.
        // FK constraints from referencing tables may error; caller
        // can scope with --app / --model.
        for table in &targets {
            let sql = format!(r#"DELETE FROM "{}""#, table.replace('"', r#""""#));
            match crate::sql::raw_execute_pool(pool, &sql, Vec::new()).await {
                Ok(_) => cleared += 1,
                Err(e) => failures.push(((*table).to_owned(), e.to_string())),
            }
        }
    }

    writeln!(w, "flush: cleared {cleared} table(s)")?;
    if !failures.is_empty() {
        writeln!(w, "flush: {} table(s) failed:", failures.len())?;
        for (t, e) in &failures {
            writeln!(w, "  - {t}: {e}")?;
        }
        // Surface as an error so the caller's exit code reflects partial failure.
        return Err(MigrateError::Validation(format!(
            "flush completed with {} failure(s)",
            failures.len()
        )));
    }
    Ok(())
}

/// Parsed `manage sendtestemail` arguments.
///
/// Only the `feature = "config"` build of `sendtestemail_cmd` uses
/// this; the `not(feature = "config")` stub short-circuits with a
/// friendly error, so the parser is unreachable there.
#[cfg(feature = "config")]
#[derive(Debug, Default, PartialEq, Eq)]
struct SendTestEmailArgs {
    to: Option<String>,
    from: Option<String>,
    subject: Option<String>,
    help: bool,
}

#[cfg(feature = "config")]
fn parse_sendtestemail_args(args: &[String]) -> Result<SendTestEmailArgs, MigrateError> {
    let mut out = SendTestEmailArgs::default();
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--help" | "-h" => out.help = true,
            "--to" => {
                let v = iter
                    .next()
                    .ok_or_else(|| MigrateError::Validation("--to requires a value".to_owned()))?;
                out.to = Some(v.clone());
            }
            "--from" => {
                let v = iter.next().ok_or_else(|| {
                    MigrateError::Validation("--from requires a value".to_owned())
                })?;
                out.from = Some(v.clone());
            }
            "--subject" => {
                let v = iter.next().ok_or_else(|| {
                    MigrateError::Validation("--subject requires a value".to_owned())
                })?;
                out.subject = Some(v.clone());
            }
            other if other.starts_with("--") => {
                return Err(MigrateError::Validation(format!("unknown flag: {other}")));
            }
            other => {
                return Err(MigrateError::Validation(format!(
                    "unexpected positional argument: {other}"
                )));
            }
        }
    }
    Ok(out)
}

/// `manage sendtestemail --to <addr>` — send a fixed test email
/// through the mail backend configured in `[mail]` settings. Django
/// parity verb for verifying SMTP credentials / mail wiring without
/// digging into a REPL.
///
/// Requires the `config` feature so settings can be loaded. Without
/// `--to`, errors with a usage hint. `--from` defaults to the
/// `[mail].from_address` setting (and errors if neither is set).
/// `--subject` defaults to `"rustango: test email"`.
///
/// Successful send prints `sendtestemail: ok` plus the resolved
/// backend name. Send failures surface the underlying `MailError`
/// as a [`MigrateError::Validation`].
#[cfg(feature = "config")]
async fn sendtestemail_cmd<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let parsed = parse_sendtestemail_args(args)?;
    if parsed.help {
        writeln!(
            w,
            "sendtestemail --to <addr> [--from <addr>] [--subject <text>]"
        )?;
        writeln!(w)?;
        writeln!(
            w,
            "  Send a test message through the [mail] backend (console / memory /"
        )?;
        writeln!(
            w,
            "  null / smtp). Use to verify SMTP credentials or mail wiring."
        )?;
        writeln!(w)?;
        writeln!(w, "  --to       Recipient address (REQUIRED).")?;
        writeln!(
            w,
            "  --from     From address. Defaults to [mail].from_address."
        )?;
        writeln!(
            w,
            "  --subject  Subject line. Defaults to `rustango: test email`."
        )?;
        return Ok(());
    }

    let to = parsed.to.as_deref().ok_or_else(|| {
        MigrateError::Validation(
            "sendtestemail: --to <addr> is required (run with --help for usage)".to_owned(),
        )
    })?;

    let settings = crate::config::Settings::load_from_env().map_err(|e| {
        MigrateError::Validation(format!(
            "sendtestemail: failed to load settings: {e} — check config/{{default,$tier}}.toml",
        ))
    })?;

    let from = parsed
        .from
        .clone()
        .or_else(|| settings.mail.from_address.clone())
        .ok_or_else(|| {
            MigrateError::Validation(
                "sendtestemail: no --from supplied and [mail].from_address is unset".to_owned(),
            )
        })?;

    let subject = parsed
        .subject
        .clone()
        .unwrap_or_else(|| "rustango: test email".to_owned());
    let body = "If you're reading this in a real inbox, your mail backend is wired correctly.\n\n\
         Sent by `manage sendtestemail`."
        .to_owned();

    let mailer = crate::email::from_settings(&settings.mail);
    let backend = settings.mail.backend.as_deref().unwrap_or("console");

    let email = crate::email::Email::new()
        .to(to)
        .from(&from)
        .subject(subject)
        .body(body);

    match mailer.send(&email).await {
        Ok(()) => {
            writeln!(
                w,
                "sendtestemail: ok (backend = {backend}, to = {to}, from = {from})"
            )?;
            Ok(())
        }
        Err(e) => Err(MigrateError::Validation(format!(
            "sendtestemail: mailer.send failed (backend = {backend}): {e}"
        ))),
    }
}

#[cfg(not(feature = "config"))]
async fn sendtestemail_cmd<W: Write>(_args: &[String], _w: &mut W) -> Result<(), MigrateError> {
    Err(MigrateError::Validation(
        "sendtestemail: this build was compiled without the `config` feature — settings \
         can't be loaded. Rebuild with `--features config`."
            .to_owned(),
    ))
}

/// Mask the password in a `postgres://user:pass@host/db` connection
/// URL so it doesn't leak into log output.
fn redact(argv: &[String]) -> Vec<String> {
    argv.iter().map(|a| redact_url(a)).collect()
}

fn redact_url(s: &str) -> String {
    // Match `<scheme>://<user>:<password>@<rest>` and replace `<password>`
    // with `***`. Anything that doesn't look like a URL passes through.
    let Some(scheme_end) = s.find("://") else {
        return s.to_owned();
    };
    let rest = &s[scheme_end + 3..];
    let Some(at) = rest.find('@') else {
        return s.to_owned();
    };
    let creds = &rest[..at];
    let Some(colon) = creds.find(':') else {
        return s.to_owned();
    };
    let user = &creds[..colon];
    let after_at = &rest[at..];
    format!("{}://{user}:***{after_at}", &s[..scheme_end])
}

/// Snapshot of the env vars `manage check --deploy` cares about.
/// Lifted out so the audit logic is pure (testable without
/// `unsafe { env::set_var }` race conditions across `cargo test`'s
/// parallel runners).
#[derive(Debug, Default, Clone)]
pub(crate) struct DeployAuditEnv {
    pub rustango_env: Option<String>,
    pub session_secret: Option<String>,
    pub database_url: Option<String>,
    pub apex_domain: Option<String>,
    pub bind: Option<String>,
}

fn deploy_audit_env() -> DeployAuditEnv {
    DeployAuditEnv {
        rustango_env: std::env::var("RUSTANGO_ENV").ok(),
        session_secret: std::env::var("RUSTANGO_SESSION_SECRET").ok(),
        database_url: std::env::var("DATABASE_URL").ok(),
        apex_domain: std::env::var("RUSTANGO_APEX_DOMAIN").ok(),
        bind: std::env::var("RUSTANGO_BIND").ok(),
    }
}

#[derive(Debug, Default)]
pub(crate) struct DeployAuditFindings {
    pub info: Vec<String>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

/// Run the `manage check --deploy` audit checks against `env`.
/// Pure function so callers can test it with arbitrary env
/// snapshots without poking actual process env vars.
///
/// Modernized from the v0.27 shape that checked the wrong env
/// var (`SECRET_KEY` — never read by the framework). The
/// framework reads `RUSTANGO_SESSION_SECRET` for HMAC-signing
/// the operator-console + tenant-admin cookies AND the JWT
/// payloads issued by `auth_routes::jwt_router` (#81). Same key
/// covers both surfaces.
pub(crate) fn run_deploy_audit(env: &DeployAuditEnv, out: &mut DeployAuditFindings) {
    // RUSTANGO_ENV — production should be explicitly tagged.
    match env.rustango_env.as_deref() {
        Some("prod" | "production") => {
            out.info
                .push("RUSTANGO_ENV is set to a production value".into());
        }
        Some(other) => {
            out.warnings.push(format!(
                "RUSTANGO_ENV is `{other}` — set to `prod` (or `production`) in deployed env"
            ));
        }
        None => {
            out.warnings.push(
                "RUSTANGO_ENV is unset — set to `prod` so config loaders pick the right tier"
                    .into(),
            );
        }
    }

    // RUSTANGO_SESSION_SECRET — required for cookie + JWT signing.
    // Recommended: 32+ bytes of base64-encoded entropy
    // (`openssl rand -base64 32`). The framework treats the raw
    // value as the HMAC key, so length matters.
    match env.session_secret.as_deref() {
        None => {
            out.errors.push(
                "RUSTANGO_SESSION_SECRET is unset — operator + tenant cookies + JWTs would use \
                 an ephemeral random secret that's regenerated on every restart, signing every \
                 user out. Set via `openssl rand -base64 32`."
                    .into(),
            );
        }
        Some(s) if s.len() < 32 => {
            out.errors.push(format!(
                "RUSTANGO_SESSION_SECRET is only {} bytes — need ≥ 32 for HMAC key strength. \
                 Regenerate with `openssl rand -base64 32`.",
                s.len()
            ));
        }
        Some(s) if s.contains("change-me") || s.contains("placeholder") => {
            out.errors.push(
                "RUSTANGO_SESSION_SECRET still contains the scaffolder placeholder \
                 (`change-me-...`) — replace with a real secret via `openssl rand -base64 32`."
                    .into(),
            );
        }
        Some(_) => {
            out.info.push("RUSTANGO_SESSION_SECRET length OK".into());
        }
    }

    // DATABASE_URL — required.
    match env.database_url.as_deref() {
        None => out
            .errors
            .push("DATABASE_URL is unset — required in production".into()),
        Some(url) if url.contains("localhost") || url.contains("127.0.0.1") => {
            out.warnings.push(
                "DATABASE_URL points at localhost / 127.0.0.1 — verify this is intended in \
                 production (typically a managed service hostname)"
                    .into(),
            );
        }
        Some(_) => out.info.push("DATABASE_URL set".into()),
    }

    // RUSTANGO_APEX_DOMAIN — required for tenancy projects, but
    // single-tenant projects don't need it. Surface as info, not
    // warning, when unset (`localhost` is the framework default
    // and works for non-tenancy deployments).
    match env.apex_domain.as_deref() {
        None | Some("localhost") => {
            out.warnings.push(
                "RUSTANGO_APEX_DOMAIN is unset / `localhost` — tenancy projects need this set to \
                 the public-facing apex (e.g. `app.example.com`) so subdomain resolution + \
                 cookie scoping work in production. Single-tenant projects can ignore."
                    .into(),
            );
        }
        Some(_) => out
            .info
            .push("RUSTANGO_APEX_DOMAIN set to a non-localhost value".into()),
    }

    // RUSTANGO_BIND — warn if loopback-only. Common typo in dev
    // configs that get promoted to prod without rebinding.
    match env.bind.as_deref() {
        Some(b) if b.starts_with("127.0.0.1") => {
            out.warnings.push(format!(
                "RUSTANGO_BIND={b} only listens on loopback — production usually wants \
                 `0.0.0.0:<port>` to accept external traffic"
            ));
        }
        Some(_) | None => {} // either explicit non-loopback or framework default (0.0.0.0)
    }
}

/// `manage check --deploy` settings-side audit (#87 slice 4) —
/// loads `Settings::load_from_env()` and flags dev-defaults left in
/// the prod tier. Pure function over the loaded settings + resolved
/// tier, no env-var poking.
///
/// Graceful: missing `config/default.toml` is silently skipped (the
/// project might not use the layered loader at all). Bad TOML
/// shape surfaces as a warning so the operator notices but the
/// rest of the audit still runs.
#[cfg(feature = "config")]
fn run_settings_audit(out: &mut DeployAuditFindings) {
    use crate::config::Settings;

    let env_tier = Settings::current_env_tier();
    out.info
        .push(format!("config tier resolved to `{env_tier}`"));

    let settings = match Settings::load_from_env() {
        Ok(s) => s,
        Err(crate::config::ConfigError::Io { .. }) => {
            // `config/default.toml` not present — project doesn't use
            // the layered loader. Skip silently.
            return;
        }
        Err(e) => {
            out.warnings.push(format!(
                "config: failed to load settings for audit: {e} — fix the file shape \
                 to enable the rest of the deploy audit"
            ));
            return;
        }
    };

    settings_audit_check(&env_tier, &settings, out);
}

/// Pure-function half of [`run_settings_audit`] — caller supplies
/// the resolved tier + settings. Lifted out so unit tests can run
/// against any combination without touching the on-disk config
/// pipeline (which would race against parallel test runners).
#[cfg(feature = "config")]
pub(crate) fn settings_audit_check(
    env_tier: &str,
    settings: &crate::config::Settings,
    out: &mut DeployAuditFindings,
) {
    let in_prod = matches!(env_tier, "prod" | "production");
    if !in_prod {
        // Non-prod tiers don't need the dev-default-leak audit.
        return;
    }

    // [security] headers_preset — must be `strict` (or unset →
    // strict by default at the layer). `"dev"` / `"none"` in prod
    // strips HSTS / XFO / nosniff / Referrer-Policy.
    if let Some(preset) = settings.security.headers_preset.as_deref() {
        if preset == "dev" || preset == "none" {
            out.warnings.push(format!(
                "[security] headers_preset = `{preset}` in prod tier — promote to `strict` \
                 so HSTS / X-Frame-Options / X-Content-Type-Options / Referrer-Policy are emitted"
            ));
        }
    }

    // [security] hsts_max_age_secs = 0 in prod disables HSTS.
    if matches!(settings.security.hsts_max_age_secs, Some(0)) {
        out.warnings.push(
            "[security] hsts_max_age_secs = 0 in prod tier — disables HSTS, leaving TLS-strip \
             attacks viable on first request"
                .into(),
        );
    }

    // [auth] argon2 memory cost — OWASP 2024 floor is 19456 KiB.
    if let Some(kib) = settings.auth.argon2_memory_kib {
        if kib < 19_456 {
            out.warnings.push(format!(
                "[auth] argon2_memory_kib = {kib} in prod tier — OWASP 2024 recommends ≥ 19456 \
                 for password hashing brute-force resistance"
            ));
        }
    }

    // [auth.jwt] access TTL too long. Anything over 1 hour is
    // suspicious — refresh tokens should rotate access tokens.
    if let Some(ttl) = settings.auth.jwt.access_ttl_secs {
        if ttl > 3600 {
            out.warnings.push(format!(
                "[auth.jwt] access_ttl_secs = {ttl} in prod tier — access tokens > 1h widen \
                 the leaked-token blast radius. The refresh flow rotates them; keep this short."
            ));
        }
    }

    // [audit] retention_days unset → log grows forever. Not an
    // error (some compliance regimes mandate forever) but worth
    // info-flagging so the operator decides intentionally.
    if settings.audit.retention_days.is_none() {
        out.info.push(
            "[audit] retention_days unset — log grows forever; consider setting + scheduling \
             `manage audit-cleanup --days <N>`"
                .into(),
        );
    }

    // [routes] legacy_preset = true is a deliberate choice (#85) but
    // worth surfacing in audit output so operators rationalize the
    // `/__admin` shape against the current Django-ish default.
    if matches!(settings.routes.legacy_preset, Some(true)) {
        out.info.push(
            "[routes] legacy_preset = true — using the pre-v0.29 `__`-prefixed URLs \
             (`/__login`, `/__admin`, …). Switch to the friendly defaults when bookmarks allow."
                .into(),
        );
    }

    // [server] bind on loopback in prod. The env-var path catches
    // RUSTANGO_BIND; this catches the TOML case that bypasses it.
    if let Some(bind) = settings.server.bind.as_deref() {
        if bind.starts_with("127.0.0.1") || bind.starts_with("localhost") {
            out.warnings.push(format!(
                "[server] bind = `{bind}` in prod tier — only listens on loopback. \
                 Production usually wants `0.0.0.0:<port>` to accept external traffic."
            ));
        }
    }

    // v0.36 slice 9 — backend × tenancy alignment check. Schema-mode
    // tenancy uses `SET search_path` which only Postgres supports.
    // Flag config files that pair a non-PG backend with tenancy.
    if let Some(backend) = settings.database.resolved_backend() {
        let tenancy_on = crate::config::Settings::detected_features()
            .iter()
            .any(|f| *f == "tenancy");
        if tenancy_on && backend != "postgres" {
            out.warnings.push(format!(
                "[database] backend = `{backend}` with `tenancy` feature on — \
                 schema-mode multi-tenancy is Postgres-only by language semantics \
                 (it relies on `SET search_path`). Either switch to a Postgres URL \
                 in prod, or use `crate::tenancy::DatabasePools<DB>` for the \
                 one-database-per-tenant model that works on sqlite/mysql."
            ));
        }
    }

    // v0.36 slice 7+10 — [admin] section deploy audit. The new
    // settings-driven admin Builder reads these in
    // `admin::Builder::from_settings`; flag dev-defaults left in prod.
    let admin = &settings.admin;

    // CSRF cookie must be Secure in prod (HTTPS only).
    if matches!(admin.csrf_cookie_secure, Some(false)) {
        out.warnings.push(
            "[admin] csrf_cookie_secure = false in prod tier — admin CSRF cookie will be \
             sent over plain HTTP, which strips its tamper resistance. Set true (or remove \
             the override) so the framework default Secure flag applies."
                .into(),
        );
    }

    // Hex color sanity — common typo is missing `#` or RGB shorthand.
    if let Some(hex) = admin.primary_color.as_deref() {
        let stripped = hex.trim_start_matches('#');
        let valid_len = matches!(stripped.len(), 3 | 6 | 8);
        let all_hex = stripped.chars().all(|c| c.is_ascii_hexdigit());
        if !hex.starts_with('#') || !valid_len || !all_hex {
            out.warnings.push(format!(
                "[admin] primary_color = `{hex}` does not parse as a hex color (expected \
                 `#RRGGBB`, `#RGB`, or `#RRGGBBAA`) — the theme will fall back to the default \
                 accent. Check for a missing leading `#` or non-hex characters."
            ));
        }
    }

    // theme_mode allowlist — typo-detection.
    if let Some(mode) = admin.theme_mode.as_deref() {
        if !matches!(mode, "auto" | "light" | "dark") {
            out.warnings.push(format!(
                "[admin] theme_mode = `{mode}` is not one of `auto` / `light` / `dark` — \
                 the chrome will ignore it and fall back to `auto`."
            ));
        }
    }

    // session_timeout_minutes = 0 in prod means no idle expiry — info-flag
    // (some deploys want this deliberately for kiosks etc.).
    if matches!(admin.session_timeout_minutes, Some(0)) {
        out.info.push(
            "[admin] session_timeout_minutes = 0 in prod tier — admin sessions never idle-expire. \
             Confirm this is deliberate (kiosk / single-user setup); otherwise pick a non-zero \
             value so abandoned sessions can't be hijacked."
                .into(),
        );
    }

    // url_prefix smell tests — trailing slash trips up some
    // template hrefs; root-mount empty string is legal but worth
    // info-flagging since it conflicts with most reverse proxies.
    if let Some(prefix) = admin.url_prefix.as_deref() {
        if prefix.ends_with('/') && prefix.len() > 1 {
            out.warnings.push(format!(
                "[admin] url_prefix = `{prefix}` ends with a trailing slash — Builder will \
                 strip it, but config files should write the canonical form (no trailing slash) \
                 so reviewers can grep across deployments."
            ));
        }
    }
}

#[cfg(test)]
mod gen_tests {
    use super::*;

    #[test]
    fn pascal_to_snake_cases() {
        assert_eq!(pascal_to_snake("Post"), "post");
        assert_eq!(pascal_to_snake("PostViewSet"), "post_view_set");
        assert_eq!(pascal_to_snake("API"), "a_p_i"); // simple impl — acceptable
        assert_eq!(pascal_to_snake("UserNotification"), "user_notification");
    }

    #[test]
    fn is_valid_type_name_accepts_pascal() {
        assert!(is_valid_type_name("Post"));
        assert!(is_valid_type_name("PostViewSet"));
        assert!(is_valid_type_name("Foo_Bar"));
    }

    #[test]
    fn is_valid_type_name_rejects_invalid() {
        assert!(!is_valid_type_name(""));
        assert!(!is_valid_type_name("post")); // lowercase
        assert!(!is_valid_type_name("123Foo")); // starts with digit
        assert!(!is_valid_type_name("Post!")); // bad char
    }

    #[test]
    fn parse_name_and_model_basic() {
        let (n, m) = parse_name_and_model(&["PostViewSet".into()]).unwrap();
        assert_eq!(n, "PostViewSet");
        assert_eq!(m, None);
    }

    #[test]
    fn parse_name_and_model_with_model_flag() {
        let args: Vec<String> = vec!["PostViewSet".into(), "--model".into(), "Post".into()];
        let (n, m) = parse_name_and_model(&args).unwrap();
        assert_eq!(n, "PostViewSet");
        assert_eq!(m, Some("Post".into()));
    }

    #[test]
    fn parse_name_and_model_rejects_missing_name() {
        let r = parse_name_and_model(&[]);
        assert!(r.is_err());
    }

    #[test]
    fn parse_name_and_model_rejects_lowercase_name() {
        let r = parse_name_and_model(&["postviewset".into()]);
        assert!(r.is_err());
    }

    // -------- dumpdata --------

    #[test]
    fn parse_dumpdata_args_defaults() {
        let p = parse_dumpdata_args(&[]).unwrap();
        assert!(p.model_filters.is_empty());
        assert_eq!(p.indent, 2);
        assert!(!p.help);
    }

    #[test]
    fn parse_dumpdata_args_help_flag() {
        let p = parse_dumpdata_args(&["--help".into()]).unwrap();
        assert!(p.help);
    }

    #[test]
    fn parse_dumpdata_args_collects_multiple_models() {
        let args: Vec<String> = vec![
            "--model".into(),
            "blog.Article".into(),
            "--model".into(),
            "Author".into(),
        ];
        let p = parse_dumpdata_args(&args).unwrap();
        assert_eq!(p.model_filters, vec!["blog.Article", "Author"]);
    }

    #[test]
    fn parse_dumpdata_args_indent_accepts_zero() {
        let p = parse_dumpdata_args(&["--indent".into(), "0".into()]).unwrap();
        assert_eq!(p.indent, 0);
    }

    #[test]
    fn parse_dumpdata_args_rejects_unknown_flag() {
        let r = parse_dumpdata_args(&["--unknown".into()]);
        assert!(r.is_err());
        assert!(format!("{}", r.unwrap_err()).contains("unknown flag"));
    }

    #[test]
    fn parse_dumpdata_args_rejects_missing_model_value() {
        let r = parse_dumpdata_args(&["--model".into()]);
        assert!(r.is_err());
    }

    #[test]
    fn parse_dumpdata_args_rejects_non_integer_indent() {
        let r = parse_dumpdata_args(&["--indent".into(), "abc".into()]);
        assert!(r.is_err());
    }

    #[test]
    fn parse_dumpdata_args_rejects_positional() {
        let r = parse_dumpdata_args(&["unexpected".into()]);
        assert!(r.is_err());
        assert!(format!("{}", r.unwrap_err()).contains("positional"));
    }

    // -------- loaddata --------

    #[test]
    fn parse_loaddata_args_requires_path() {
        let r = parse_loaddata_args(&[]);
        assert!(r.is_err());
        assert!(format!("{}", r.unwrap_err()).contains("missing"));
    }

    #[test]
    fn parse_loaddata_args_path_then_flag() {
        let p = parse_loaddata_args(&["fixtures.json".into(), "--fail-fast".into()]).unwrap();
        assert_eq!(p.file, "fixtures.json");
        assert!(p.fail_fast);
    }

    #[test]
    fn parse_loaddata_args_flag_then_path() {
        let p = parse_loaddata_args(&["--fail-fast".into(), "fixtures.json".into()]).unwrap();
        assert_eq!(p.file, "fixtures.json");
        assert!(p.fail_fast);
    }

    #[test]
    fn parse_loaddata_args_help_flag_short_circuits() {
        let p = parse_loaddata_args(&["--help".into()]).unwrap();
        assert!(p.help);
    }

    #[test]
    fn parse_loaddata_args_rejects_two_positionals() {
        let r = parse_loaddata_args(&["a.json".into(), "b.json".into()]);
        assert!(r.is_err());
    }

    #[test]
    fn parse_loaddata_args_rejects_unknown_flag() {
        let r = parse_loaddata_args(&["--unknown".into(), "a.json".into()]);
        assert!(r.is_err());
    }

    // -------- json_to_sql_value --------

    fn field(name: &'static str, ty: crate::core::FieldType) -> crate::core::FieldSchema {
        crate::core::FieldSchema {
            name,
            column: name,
            ty,
            nullable: true,
            primary_key: false,
            relation: None,
            max_length: None,
            min: None,
            max: None,
            default: None,
            auto: false,
            unique: false,
            generated_as: None,
            help_text: None,
            choices: None,
            db_comment: None,
            verbose_name: None,
            editable: true,
            blank: false,
            validators: &[],
        }
    }

    #[test]
    fn json_to_sql_value_null_passes_through() {
        let f = field("x", crate::core::FieldType::I64);
        let v = json_to_sql_value(&serde_json::Value::Null, &f).unwrap();
        assert!(matches!(v, crate::core::SqlValue::Null));
    }

    #[test]
    fn json_to_sql_value_i32_range_check() {
        let f = field("x", crate::core::FieldType::I32);
        let v = json_to_sql_value(&serde_json::json!(42), &f).unwrap();
        assert!(matches!(v, crate::core::SqlValue::I32(42)));
        // Out of range → Err.
        let r = json_to_sql_value(&serde_json::json!(99999999999_i64), &f);
        assert!(r.is_err());
    }

    #[test]
    fn json_to_sql_value_string_passes_through() {
        let f = field("x", crate::core::FieldType::String);
        let v = json_to_sql_value(&serde_json::json!("hello"), &f).unwrap();
        match v {
            crate::core::SqlValue::String(s) => assert_eq!(s, "hello"),
            other => panic!("expected SqlValue::String, got {other:?}"),
        }
    }

    #[test]
    fn json_to_sql_value_date_parses_iso() {
        let f = field("x", crate::core::FieldType::Date);
        let v = json_to_sql_value(&serde_json::json!("2025-01-02"), &f).unwrap();
        assert!(matches!(v, crate::core::SqlValue::Date(_)));
        // Malformed → Err.
        let r = json_to_sql_value(&serde_json::json!("not a date"), &f);
        assert!(r.is_err());
    }

    #[test]
    fn json_to_sql_value_datetime_parses_rfc3339() {
        let f = field("x", crate::core::FieldType::DateTime);
        let v = json_to_sql_value(&serde_json::json!("2025-01-02T12:00:00+00:00"), &f).unwrap();
        assert!(matches!(v, crate::core::SqlValue::DateTime(_)));
    }

    #[test]
    fn json_to_sql_value_decimal_parses_string_and_number() {
        let f = field("x", crate::core::FieldType::Decimal);
        // From string.
        let v = json_to_sql_value(&serde_json::json!("123.45"), &f).unwrap();
        assert!(matches!(v, crate::core::SqlValue::Decimal(_)));
        // From number (Django dumpdata emits as string but be forgiving).
        let v = json_to_sql_value(&serde_json::json!(42), &f).unwrap();
        assert!(matches!(v, crate::core::SqlValue::Decimal(_)));
    }

    #[test]
    fn json_to_sql_value_binary_hex_round_trip() {
        let f = field("x", crate::core::FieldType::Binary);
        let v = json_to_sql_value(&serde_json::json!("deadbeef"), &f).unwrap();
        match v {
            crate::core::SqlValue::Binary(b) => assert_eq!(b, vec![0xde, 0xad, 0xbe, 0xef]),
            other => panic!("expected Binary, got {other:?}"),
        }
        // Malformed (odd length).
        let r = json_to_sql_value(&serde_json::json!("abc"), &f);
        assert!(r.is_err());
    }

    #[test]
    fn json_to_sql_value_json_field_stays_value() {
        let f = field("x", crate::core::FieldType::Json);
        let v = json_to_sql_value(&serde_json::json!({"k": "v"}), &f).unwrap();
        match v {
            crate::core::SqlValue::Json(j) => assert_eq!(j, serde_json::json!({"k": "v"})),
            other => panic!("expected Json, got {other:?}"),
        }
    }

    // -------- showurls --------

    #[test]
    fn showurls_help_emits_usage_line() {
        let mut buf: Vec<u8> = Vec::new();
        let r = showurls_cmd(&["--help".into()], &mut buf);
        assert!(r.is_ok());
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("showurls"), "help text mentions verb: {s}");
        assert!(s.contains("plain"));
        assert!(s.contains("json"));
    }

    #[test]
    fn showurls_rejects_unknown_format() {
        let mut buf: Vec<u8> = Vec::new();
        let r = showurls_cmd(&["--format".into(), "xml".into()], &mut buf);
        assert!(r.is_err());
        let e = format!("{}", r.unwrap_err());
        assert!(e.contains("unknown value"), "error: {e}");
    }

    #[test]
    fn showurls_rejects_unknown_flag() {
        let mut buf: Vec<u8> = Vec::new();
        let r = showurls_cmd(&["--badflag".into()], &mut buf);
        assert!(r.is_err());
    }

    #[test]
    fn showurls_rejects_positional() {
        let mut buf: Vec<u8> = Vec::new();
        let r = showurls_cmd(&["extra".into()], &mut buf);
        assert!(r.is_err());
        let e = format!("{}", r.unwrap_err());
        assert!(e.contains("positional"));
    }

    #[test]
    fn showurls_plain_format_runs_without_error() {
        let mut buf: Vec<u8> = Vec::new();
        let r = showurls_cmd(&[], &mut buf);
        // Whether named URLs exist in this test binary depends on
        // other crates / test code that may have registered them.
        // The verb itself must always succeed.
        assert!(r.is_ok(), "showurls plain shouldn't error: {r:?}");
    }

    #[test]
    fn showurls_json_format_emits_json_array() {
        let mut buf: Vec<u8> = Vec::new();
        let r = showurls_cmd(&["--format".into(), "json".into()], &mut buf);
        assert!(r.is_ok());
        let s = String::from_utf8(buf).unwrap();
        // Must be valid JSON.
        let parsed: serde_json::Value = serde_json::from_str(s.trim()).unwrap();
        assert!(parsed.is_array(), "expected JSON array, got: {s}");
    }

    // -------- showmodels --------

    #[test]
    fn showmodels_help_short_circuits() {
        let mut buf: Vec<u8> = Vec::new();
        let r = showmodels_cmd(&["--help".into()], &mut buf);
        assert!(r.is_ok());
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("showmodels"));
        assert!(s.contains("--app"));
    }

    #[test]
    fn showmodels_rejects_unknown_format() {
        let mut buf: Vec<u8> = Vec::new();
        let r = showmodels_cmd(&["--format".into(), "xml".into()], &mut buf);
        assert!(r.is_err());
        let e = format!("{}", r.unwrap_err());
        assert!(e.contains("unknown value"));
    }

    #[test]
    fn showmodels_rejects_unknown_flag() {
        let mut buf: Vec<u8> = Vec::new();
        let r = showmodels_cmd(&["--badflag".into()], &mut buf);
        assert!(r.is_err());
    }

    #[test]
    fn showmodels_rejects_positional() {
        let mut buf: Vec<u8> = Vec::new();
        let r = showmodels_cmd(&["unexpected".into()], &mut buf);
        assert!(r.is_err());
    }

    #[test]
    fn showmodels_app_filter_requires_value() {
        let mut buf: Vec<u8> = Vec::new();
        let r = showmodels_cmd(&["--app".into()], &mut buf);
        assert!(r.is_err());
    }

    #[test]
    fn showmodels_plain_runs_clean() {
        // Inventory contents depend on what's linked in the test
        // binary; we just verify the verb completes without error.
        let mut buf: Vec<u8> = Vec::new();
        let r = showmodels_cmd(&[], &mut buf);
        assert!(r.is_ok(), "showmodels should not error: {r:?}");
    }

    #[test]
    fn showmodels_json_emits_parseable_array() {
        let mut buf: Vec<u8> = Vec::new();
        let r = showmodels_cmd(&["--format".into(), "json".into()], &mut buf);
        assert!(r.is_ok());
        let s = String::from_utf8(buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(s.trim()).unwrap();
        assert!(parsed.is_array());
    }

    // -------- flush --------

    #[test]
    fn parse_flush_args_defaults() {
        let p = parse_flush_args(&[]).unwrap();
        assert!(!p.yes);
        assert!(p.apps.is_empty());
        assert!(p.models.is_empty());
        assert!(!p.help);
    }

    #[test]
    fn parse_flush_args_yes_flag() {
        let p = parse_flush_args(&["--yes".into()]).unwrap();
        assert!(p.yes);
    }

    #[test]
    fn parse_flush_args_collects_multiple_apps_and_models() {
        let args: Vec<String> = vec![
            "--app".into(),
            "blog".into(),
            "--model".into(),
            "Article".into(),
            "--app".into(),
            "shop".into(),
            "--model".into(),
            "shop.Order".into(),
        ];
        let p = parse_flush_args(&args).unwrap();
        assert_eq!(p.apps, vec!["blog", "shop"]);
        assert_eq!(p.models, vec!["Article", "shop.Order"]);
    }

    #[test]
    fn parse_flush_args_help_short_circuits() {
        let p = parse_flush_args(&["--help".into()]).unwrap();
        assert!(p.help);
    }

    #[test]
    fn parse_flush_args_rejects_unknown_flag() {
        let r = parse_flush_args(&["--badflag".into()]);
        assert!(r.is_err());
    }

    #[test]
    fn parse_flush_args_rejects_positional() {
        let r = parse_flush_args(&["unexpected".into()]);
        assert!(r.is_err());
    }

    #[test]
    fn parse_flush_args_app_requires_value() {
        let r = parse_flush_args(&["--app".into()]);
        assert!(r.is_err());
    }

    #[test]
    fn parse_flush_args_model_requires_value() {
        let r = parse_flush_args(&["--model".into()]);
        assert!(r.is_err());
    }

    // -------- sendtestemail --------
    //
    // These tests + the args struct live under `feature = "config"`
    // because `sendtestemail_cmd` reads `Settings.email.*` to pick a
    // backend; the `not(feature = "config")` stub short-circuits with
    // a friendly error and never reaches the parser.

    #[cfg(feature = "config")]
    #[test]
    fn parse_sendtestemail_args_defaults_empty() {
        let p = parse_sendtestemail_args(&[]).unwrap();
        assert!(p.to.is_none());
        assert!(p.from.is_none());
        assert!(p.subject.is_none());
        assert!(!p.help);
    }

    #[cfg(feature = "config")]
    #[test]
    fn parse_sendtestemail_args_collects_to_from_subject() {
        let args: Vec<String> = vec![
            "--to".into(),
            "ops@example.com".into(),
            "--from".into(),
            "bot@example.com".into(),
            "--subject".into(),
            "ping".into(),
        ];
        let p = parse_sendtestemail_args(&args).unwrap();
        assert_eq!(p.to.as_deref(), Some("ops@example.com"));
        assert_eq!(p.from.as_deref(), Some("bot@example.com"));
        assert_eq!(p.subject.as_deref(), Some("ping"));
    }

    #[cfg(feature = "config")]
    #[test]
    fn parse_sendtestemail_args_help_short_circuits() {
        let p = parse_sendtestemail_args(&["--help".into()]).unwrap();
        assert!(p.help);
    }

    #[cfg(feature = "config")]
    #[test]
    fn parse_sendtestemail_args_rejects_unknown_flag() {
        let r = parse_sendtestemail_args(&["--bogus".into()]);
        assert!(r.is_err());
    }

    #[cfg(feature = "config")]
    #[test]
    fn parse_sendtestemail_args_rejects_positional() {
        let r = parse_sendtestemail_args(&["unexpected".into()]);
        assert!(r.is_err());
    }

    #[cfg(feature = "config")]
    #[test]
    fn parse_sendtestemail_args_to_requires_value() {
        let r = parse_sendtestemail_args(&["--to".into()]);
        assert!(r.is_err());
    }

    #[cfg(feature = "config")]
    #[test]
    fn parse_sendtestemail_args_from_requires_value() {
        let r = parse_sendtestemail_args(&["--from".into()]);
        assert!(r.is_err());
    }

    #[cfg(feature = "config")]
    #[test]
    fn parse_sendtestemail_args_subject_requires_value() {
        let r = parse_sendtestemail_args(&["--subject".into()]);
        assert!(r.is_err());
    }

    #[cfg(feature = "config")]
    #[tokio::test]
    async fn sendtestemail_help_short_circuits_without_settings_lookup() {
        let mut buf: Vec<u8> = Vec::new();
        let r = sendtestemail_cmd(&["--help".into()], &mut buf).await;
        assert!(r.is_ok());
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("sendtestemail"));
        assert!(s.contains("--to"));
    }

    #[cfg(feature = "config")]
    #[tokio::test]
    async fn sendtestemail_errors_when_to_missing() {
        let mut buf: Vec<u8> = Vec::new();
        let r = sendtestemail_cmd(&[], &mut buf).await;
        assert!(r.is_err(), "expected Validation error for missing --to");
    }

    #[test]
    fn showmodels_app_filter_excludes_other_apps() {
        // Filter to an app name that definitely doesn't exist —
        // result should be the "no models registered" message.
        let mut buf: Vec<u8> = Vec::new();
        let r = showmodels_cmd(
            &["--app".into(), "xyz_definitely_not_an_app".into()],
            &mut buf,
        );
        assert!(r.is_ok());
        let s = String::from_utf8(buf).unwrap();
        assert!(
            s.contains("no models registered"),
            "expected empty-output marker for unknown app, got: {s}"
        );
    }

    /// Default template (no `--tenant`) keeps the v0.28
    /// `#[derive(ViewSet)]` shape — single-tenant projects rely on
    /// the mount-time `pool` argument.
    #[test]
    fn viewset_template_pool_emits_derive_macro() {
        let body = viewset_template_pool("PostViewSet", "Post", "post_view_set");
        assert!(
            body.contains("#[derive(ViewSet)]"),
            "expected derive macro, got: {body}"
        );
        assert!(
            body.contains("PostViewSet::router"),
            "expected `Name::router(...)` mount hint, got: {body}"
        );
        assert!(
            !body.contains("tenant_router"),
            "pool template must NOT reference tenant_router, got: {body}"
        );
    }

    /// `--tenant` template uses `ViewSet::for_model(...).tenant_router(...)`
    /// and pulls `crate::extractors::Tenant`-shape connections instead
    /// of baking a pool at mount time. Required for tenancy projects
    /// (#80).
    #[test]
    fn viewset_template_tenant_uses_tenant_router() {
        let body = viewset_template_tenant("PostViewSet", "Post", "post_view_set");
        assert!(
            body.contains("ViewSet::for_model"),
            "expected runtime ViewSet::for_model builder, got: {body}"
        );
        assert!(
            body.contains(".tenant_router("),
            "expected `.tenant_router(...)` call, got: {body}"
        );
        assert!(
            !body.contains("#[derive(ViewSet)]"),
            "tenant template must NOT use the derive macro (pool-coupled), got: {body}"
        );
        assert!(
            body.contains("/api/post_view_set"),
            "expected snake-cased path, got: {body}"
        );
        // The hint comment in the template should match the actual
        // function call site Devs will copy from it.
        assert!(
            body.contains("pub fn router()"),
            "expected `pub fn router()` so api_routes.rs can `.merge(...)`, got: {body}"
        );
        // v0.30.5 — the scaffolded body must demonstrate the v0.30
        // unified builder chain (filter / search / ordering / page /
        // perms) so users discover the surface without reading the
        // docs. Stale "v1 scope" caveat must NOT appear.
        assert!(
            !body.contains("v1 scope"),
            "v0.30 unification removed the v1 scope caveat — \
             template must reflect full feature parity, got: {body}"
        );
        for knob in [
            ".filter_fields(",
            ".search_fields(",
            ".ordering(",
            ".page_size(",
            ".permissions_for_model::",
        ] {
            assert!(
                body.contains(knob),
                "expected `{knob}` in tenant template (commented hint), got: {body}"
            );
        }
    }

    /// CWD is process-global; tests that chdir must be serialized
    /// or `cargo test`'s default thread-pool will race them and
    /// trip "No such file or directory" when one test's tempdir
    /// drop runs while another is restoring CWD. This static
    /// mutex serializes every chdir-based test in this module.
    fn cwd_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    /// `project_uses_tenancy` reads Cargo.toml from CWD and looks for
    /// the `tenancy` feature on the `rustango` dep. Tested by
    /// pushing a fixture file into a tempdir, chdir-ing in, and
    /// asserting the result.
    #[test]
    fn project_uses_tenancy_detects_inline_features_array() {
        let _guard = cwd_lock().lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let cargo = dir.path().join("Cargo.toml");
        std::fs::write(
            &cargo,
            r#"[package]
name = "demo"
version = "0.1.0"
edition = "2021"

[dependencies]
rustango = { version = "0.30", features = ["tenancy", "manage"] }
"#,
        )
        .unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let detected = project_uses_tenancy();
        let _ = std::env::set_current_dir(&prev);
        assert!(
            detected,
            "inline-table dep with `tenancy` in features should auto-detect"
        );
    }

    /// `project_uses_tenancy` returns false on the dedicated dep
    /// table form when `tenancy` isn't listed.
    #[test]
    fn project_uses_tenancy_false_when_feature_absent() {
        let _guard = cwd_lock().lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let cargo = dir.path().join("Cargo.toml");
        std::fs::write(
            &cargo,
            r#"[package]
name = "demo"
version = "0.1.0"
edition = "2021"

[dependencies]
rustango = { version = "0.30", features = ["postgres", "manage"] }
"#,
        )
        .unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let detected = project_uses_tenancy();
        let _ = std::env::set_current_dir(&prev);
        assert!(
            !detected,
            "no tenancy feature → must default to single-tenant scaffold"
        );
    }

    /// Missing Cargo.toml (running outside a project) → false.
    /// Scaffolder must not crash; auto-detect just falls back to
    /// the safer single-tenant default.
    #[test]
    fn project_uses_tenancy_false_when_cargo_toml_missing() {
        let _guard = cwd_lock().lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let detected = project_uses_tenancy();
        let _ = std::env::set_current_dir(&prev);
        assert!(!detected);
    }

    /// `resolve_viewset_tenant_mode` is the pure decision the
    /// `make:viewset` command runs after parsing flags. Tested as
    /// a pure function so we don't need chdir + tempdir + writing
    /// to a project tree (which earlier leaked test artifacts when
    /// parallel tests raced on CWD).
    ///
    /// Echo fires only on the implicit-auto-detect path; explicit
    /// `--tenant` / `--no-tenant` users already know what they
    /// asked for.
    #[test]
    fn resolve_viewset_tenant_mode_decision_table() {
        // (explicit_tenant, explicit_no_tenant, project_tenancy)
        // → (tenant_aware, echo)
        let cases: &[(bool, bool, bool, bool, bool)] = &[
            // No flags, no tenancy detected → pool, no echo.
            (false, false, false, false, false),
            // No flags, tenancy detected → tenant + echo.
            (false, false, true, true, true),
            // Explicit --tenant, no tenancy detected → tenant, no echo.
            (true, false, false, true, false),
            // Explicit --tenant, tenancy detected → tenant, no echo
            // (the user asked for it, no need to inform).
            (true, false, true, true, false),
            // Explicit --no-tenant always wins → pool, no echo.
            (false, true, false, false, false),
            (false, true, true, false, false),
            // Both flags set: --no-tenant is the safer default and
            // takes precedence (the function checks it first).
            (true, true, true, false, false),
        ];
        for &(et, ent, pt, want_tenant, want_echo) in cases {
            let (tenant, echo) = resolve_viewset_tenant_mode(et, ent, pt);
            assert_eq!(
                (tenant, echo),
                (want_tenant, want_echo),
                "case (explicit_tenant={et}, explicit_no_tenant={ent}, project_tenancy={pt})"
            );
        }
    }

    // -------- make:api_routes (#82-partial) --------

    /// Tenancy template emits the no-arg `pub fn api()` shape and
    /// references `tenant_router` in the placeholder hint.
    #[test]
    fn api_routes_template_tenant_emits_no_arg_fn() {
        let body = api_routes_template_tenant("regions");
        assert!(
            body.contains("pub fn api() -> Router<()>"),
            "expected no-arg `pub fn api()`, got: {body}"
        );
        assert!(
            body.contains("tenant_router("),
            "expected tenant_router hint comment, got: {body}"
        );
        assert!(
            !body.contains("PgPool"),
            "tenant template must NOT thread PgPool through api(), got: {body}"
        );
    }

    /// Default template threads `PgPool` so per-model derived
    /// viewsets have something to capture at mount time.
    #[test]
    fn api_routes_template_pool_threads_pgpool() {
        let body = api_routes_template_pool("blog");
        assert!(
            body.contains("pub fn api(pool: PgPool) -> Router<()>"),
            "expected pool-arg api fn, got: {body}"
        );
        assert!(
            body.contains("use rustango::sql::sqlx::PgPool;"),
            "expected PgPool import, got: {body}"
        );
    }

    /// App-name validator accepts snake_case and rejects hyphens /
    /// uppercase / leading digits — same rule we apply to table
    /// names in the macro. Keeps `src/<app>/` shape Rust-friendly.
    #[test]
    fn is_valid_app_name_snake_case_only() {
        assert!(is_valid_app_name("regions"));
        assert!(is_valid_app_name("blog_posts"));
        assert!(is_valid_app_name("_internal"));
        assert!(!is_valid_app_name(""));
        assert!(!is_valid_app_name("Regions"));
        assert!(!is_valid_app_name("region-app"));
        assert!(!is_valid_app_name("9_apps"));
    }

    // -------- run_deploy_audit (`manage check --deploy`) --------

    fn good_prod_env() -> DeployAuditEnv {
        DeployAuditEnv {
            rustango_env: Some("prod".into()),
            session_secret: Some("a".repeat(48)),
            database_url: Some("postgres://app:s3cr3t@db.example.com/app_prod".into()),
            apex_domain: Some("app.example.com".into()),
            bind: Some("0.0.0.0:8080".into()),
        }
    }

    fn run(env: &DeployAuditEnv) -> DeployAuditFindings {
        let mut out = DeployAuditFindings::default();
        run_deploy_audit(env, &mut out);
        out
    }

    #[test]
    fn deploy_audit_clean_prod_env_has_no_warnings_or_errors() {
        let r = run(&good_prod_env());
        assert!(
            r.errors.is_empty(),
            "expected no errors in clean prod env, got: {:?}",
            r.errors
        );
        assert!(
            r.warnings.is_empty(),
            "expected no warnings in clean prod env, got: {:?}",
            r.warnings
        );
    }

    #[test]
    fn deploy_audit_unset_session_secret_errors() {
        let env = DeployAuditEnv {
            session_secret: None,
            ..good_prod_env()
        };
        let r = run(&env);
        assert!(
            r.errors
                .iter()
                .any(|e| e.contains("RUSTANGO_SESSION_SECRET")),
            "expected error for unset RUSTANGO_SESSION_SECRET, got: {:?}",
            r.errors
        );
    }

    #[test]
    fn deploy_audit_short_session_secret_errors() {
        let env = DeployAuditEnv {
            session_secret: Some("too-short".into()),
            ..good_prod_env()
        };
        let r = run(&env);
        assert!(
            r.errors
                .iter()
                .any(|e| e.contains("only") && e.contains("bytes")),
            "expected length error for short secret, got: {:?}",
            r.errors
        );
    }

    /// The scaffolder writes
    /// `RUSTANGO_SESSION_SECRET=change-me-base64-encoded-32-bytes-or-more`
    /// to `.env.example`. A user who copied that to `.env` and never
    /// ran `openssl rand -base64 32` should get a loud error in
    /// `--deploy` mode rather than silently shipping a known-public
    /// "secret".
    #[test]
    fn deploy_audit_placeholder_session_secret_errors() {
        let env = DeployAuditEnv {
            session_secret: Some("change-me-base64-encoded-32-bytes-or-more".into()),
            ..good_prod_env()
        };
        let r = run(&env);
        assert!(
            r.errors
                .iter()
                .any(|e| e.contains("placeholder") || e.contains("change-me")),
            "expected error for unchanged placeholder secret, got: {:?}",
            r.errors
        );
    }

    #[test]
    fn deploy_audit_unset_rustango_env_warns() {
        let env = DeployAuditEnv {
            rustango_env: None,
            ..good_prod_env()
        };
        let r = run(&env);
        assert!(
            r.warnings.iter().any(|w| w.contains("RUSTANGO_ENV")),
            "expected warning for unset RUSTANGO_ENV, got: {:?}",
            r.warnings
        );
    }

    #[test]
    fn deploy_audit_dev_rustango_env_warns() {
        let env = DeployAuditEnv {
            rustango_env: Some("dev".into()),
            ..good_prod_env()
        };
        let r = run(&env);
        assert!(
            r.warnings.iter().any(|w| w.contains("`dev`")),
            "expected warning for non-prod RUSTANGO_ENV, got: {:?}",
            r.warnings
        );
    }

    #[test]
    fn deploy_audit_unset_database_url_errors() {
        let env = DeployAuditEnv {
            database_url: None,
            ..good_prod_env()
        };
        let r = run(&env);
        assert!(
            r.errors.iter().any(|e| e.contains("DATABASE_URL")),
            "expected error for unset DATABASE_URL, got: {:?}",
            r.errors
        );
    }

    #[test]
    fn deploy_audit_localhost_database_url_warns() {
        let env = DeployAuditEnv {
            database_url: Some("postgres://app:p@localhost/db".into()),
            ..good_prod_env()
        };
        let r = run(&env);
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("DATABASE_URL") && w.contains("localhost")),
            "expected warning for localhost DATABASE_URL, got: {:?}",
            r.warnings
        );
    }

    #[test]
    fn deploy_audit_localhost_apex_warns_for_tenancy() {
        let env = DeployAuditEnv {
            apex_domain: Some("localhost".into()),
            ..good_prod_env()
        };
        let r = run(&env);
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("RUSTANGO_APEX_DOMAIN")),
            "expected warning for localhost apex, got: {:?}",
            r.warnings
        );
    }

    #[test]
    fn deploy_audit_loopback_bind_warns() {
        let env = DeployAuditEnv {
            bind: Some("127.0.0.1:8080".into()),
            ..good_prod_env()
        };
        let r = run(&env);
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("RUSTANGO_BIND") && w.contains("loopback")),
            "expected warning for loopback bind, got: {:?}",
            r.warnings
        );
    }

    // -------- settings_audit_check (#87 slice 4) --------

    #[cfg(feature = "config")]
    fn settings_run(env_tier: &str, s: &crate::config::Settings) -> DeployAuditFindings {
        let mut out = DeployAuditFindings::default();
        settings_audit_check(env_tier, s, &mut out);
        out
    }

    /// In dev/staging tiers, the audit is a no-op — operators
    /// expect dev defaults to be loud only when promoted to prod.
    #[cfg(feature = "config")]
    #[test]
    fn settings_audit_dev_tier_is_quiet() {
        let mut s = crate::config::Settings::default();
        s.security.headers_preset = Some("dev".into());
        s.security.hsts_max_age_secs = Some(0);
        s.auth.argon2_memory_kib = Some(1024); // dev-fast
        let r = settings_run("dev", &s);
        assert!(
            r.warnings.is_empty(),
            "dev tier should be quiet, got: {:?}",
            r.warnings
        );
        assert!(
            r.info.is_empty(),
            "dev tier should be quiet, got: {:?}",
            r.info
        );
    }

    /// In prod, `headers_preset = "dev"` is a clear footgun —
    /// emit a warning pointing at the fix.
    #[cfg(feature = "config")]
    #[test]
    fn settings_audit_prod_with_dev_headers_preset_warns() {
        let mut s = crate::config::Settings::default();
        s.security.headers_preset = Some("dev".into());
        let r = settings_run("prod", &s);
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("headers_preset") && w.contains("dev")),
            "expected warning for dev headers in prod, got: {:?}",
            r.warnings
        );
    }

    /// `hsts_max_age_secs = 0` in prod disables HSTS.
    #[cfg(feature = "config")]
    #[test]
    fn settings_audit_prod_with_zero_hsts_warns() {
        let mut s = crate::config::Settings::default();
        s.security.hsts_max_age_secs = Some(0);
        let r = settings_run("prod", &s);
        assert!(
            r.warnings.iter().any(|w| w.contains("hsts_max_age_secs")),
            "expected HSTS warning, got: {:?}",
            r.warnings
        );
    }

    /// argon2 below the OWASP 2024 floor warns. Default values
    /// (None) don't warn — they fall through to the framework's
    /// hardcoded sensible default.
    #[cfg(feature = "config")]
    #[test]
    fn settings_audit_prod_low_argon2_warns_but_unset_is_quiet() {
        let mut s = crate::config::Settings::default();
        s.auth.argon2_memory_kib = Some(4096); // way below 19456
        let r = settings_run("prod", &s);
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("argon2_memory_kib") && w.contains("19456")),
            "expected argon2 floor warning, got: {:?}",
            r.warnings
        );

        // Unset = framework default. Quiet.
        let s_default = crate::config::Settings::default();
        let r = settings_run("prod", &s_default);
        assert!(
            !r.warnings.iter().any(|w| w.contains("argon2")),
            "default argon2 should be quiet, got: {:?}",
            r.warnings
        );
    }

    /// JWT access TTL > 1h in prod warns.
    #[cfg(feature = "config")]
    #[test]
    fn settings_audit_prod_long_jwt_access_ttl_warns() {
        let mut s = crate::config::Settings::default();
        s.auth.jwt.access_ttl_secs = Some(86400); // 24h
        let r = settings_run("prod", &s);
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("access_ttl_secs") && w.contains("86400")),
            "expected access TTL warning, got: {:?}",
            r.warnings
        );
    }

    /// Loopback bind in TOML triggers the warning even when
    /// RUSTANGO_BIND env var isn't set.
    #[cfg(feature = "config")]
    #[test]
    fn settings_audit_prod_loopback_bind_warns() {
        let mut s = crate::config::Settings::default();
        s.server.bind = Some("127.0.0.1:8080".into());
        let r = settings_run("prod", &s);
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("[server] bind") && w.contains("loopback")),
            "expected loopback warning, got: {:?}",
            r.warnings
        );
    }

    /// `legacy_preset = true` is a deliberate choice — info, not
    /// warning. `retention_days = None` is also info-level.
    #[cfg(feature = "config")]
    #[test]
    fn settings_audit_legacy_preset_and_unset_retention_are_info() {
        let mut s = crate::config::Settings::default();
        s.routes.legacy_preset = Some(true);
        let r = settings_run("prod", &s);
        assert!(
            r.info.iter().any(|i| i.contains("legacy_preset")),
            "expected legacy_preset info, got: {:?}",
            r.info
        );
        assert!(
            r.info.iter().any(|i| i.contains("retention_days")),
            "expected retention_days info, got: {:?}",
            r.info
        );
        assert!(
            r.warnings.is_empty(),
            "neither should be warnings, got: {:?}",
            r.warnings
        );
    }

    // -------- v0.36 slice 7+10 — [admin] section audit --------

    /// `csrf_cookie_secure = false` in prod is a footgun — admin
    /// CSRF cookie sent over plain HTTP loses tamper resistance.
    #[cfg(feature = "config")]
    #[test]
    fn settings_audit_admin_csrf_insecure_warns_in_prod() {
        let mut s = crate::config::Settings::default();
        s.admin.csrf_cookie_secure = Some(false);
        let r = settings_run("prod", &s);
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("[admin]") && w.contains("csrf_cookie_secure")),
            "expected CSRF secure warning, got: {:?}",
            r.warnings
        );
    }

    /// Malformed hex color trips the format check.
    #[cfg(feature = "config")]
    #[test]
    fn settings_audit_admin_bad_primary_color_warns() {
        let mut s = crate::config::Settings::default();
        s.admin.primary_color = Some("not-a-color".into());
        let r = settings_run("prod", &s);
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("[admin]") && w.contains("primary_color")),
            "expected primary_color warning, got: {:?}",
            r.warnings
        );
    }

    /// Valid hex colors (3 / 6 / 8 hex digits, leading #) stay quiet.
    #[cfg(feature = "config")]
    #[test]
    fn settings_audit_admin_valid_hex_color_is_quiet() {
        for hex in ["#abc", "#2c6fb0", "#2c6fb0ff"] {
            let mut s = crate::config::Settings::default();
            s.admin.primary_color = Some(hex.into());
            let r = settings_run("prod", &s);
            assert!(
                !r.warnings
                    .iter()
                    .any(|w| w.contains("[admin]") && w.contains("primary_color")),
                "expected `{hex}` to be quiet, got: {:?}",
                r.warnings
            );
        }
    }

    /// Bogus theme_mode trips the allowlist.
    #[cfg(feature = "config")]
    #[test]
    fn settings_audit_admin_unknown_theme_mode_warns() {
        let mut s = crate::config::Settings::default();
        s.admin.theme_mode = Some("midnight".into());
        let r = settings_run("prod", &s);
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("[admin]") && w.contains("theme_mode")),
            "expected theme_mode warning, got: {:?}",
            r.warnings
        );
    }

    /// `session_timeout_minutes = 0` is info-level (some deploys
    /// want never-expire kiosk sessions deliberately).
    #[cfg(feature = "config")]
    #[test]
    fn settings_audit_admin_zero_session_timeout_is_info() {
        let mut s = crate::config::Settings::default();
        s.admin.session_timeout_minutes = Some(0);
        let r = settings_run("prod", &s);
        assert!(
            r.info
                .iter()
                .any(|i| i.contains("[admin]") && i.contains("session_timeout_minutes")),
            "expected session_timeout_minutes info, got: {:?}",
            r.info
        );
    }

    /// url_prefix with a trailing slash trips the canonical-form
    /// nudge (Builder strips it, but the config should be clean).
    #[cfg(feature = "config")]
    #[test]
    fn settings_audit_admin_trailing_slash_url_prefix_warns() {
        let mut s = crate::config::Settings::default();
        s.admin.url_prefix = Some("/admin/".into());
        let r = settings_run("prod", &s);
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("[admin]") && w.contains("url_prefix")),
            "expected url_prefix warning, got: {:?}",
            r.warnings
        );
    }

    /// Empty (root-mount) url_prefix stays quiet — legal, just unusual.
    #[cfg(feature = "config")]
    #[test]
    fn settings_audit_admin_empty_url_prefix_is_quiet() {
        let mut s = crate::config::Settings::default();
        s.admin.url_prefix = Some("".into());
        let r = settings_run("prod", &s);
        assert!(
            !r.warnings
                .iter()
                .any(|w| w.contains("[admin]") && w.contains("url_prefix")),
            "empty url_prefix should be quiet, got: {:?}",
            r.warnings
        );
    }

    /// v0.36 slice 9 — pairing `tenancy` feature with a non-PG backend
    /// is a misconfiguration: schema-mode tenancy needs Postgres.
    /// This test only runs when the test profile has tenancy on AND
    /// postgres is the active backend — every other feature combo
    /// makes the assertion logically unreachable.
    #[cfg(all(
        feature = "config",
        feature = "tenancy",
        feature = "postgres",
        any(feature = "sqlite", feature = "mysql"),
    ))]
    #[test]
    fn settings_audit_sqlite_backend_with_tenancy_warns_in_prod() {
        let mut s = crate::config::Settings::default();
        s.database.backend = Some("sqlite".into());
        let r = settings_run("prod", &s);
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("[database]") && w.contains("tenancy")),
            "expected backend × tenancy warning, got: {:?}",
            r.warnings
        );
    }
}

#[cfg(test)]
mod db_cmd_tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| (*x).to_owned()).collect()
    }

    // -------- parse_db_dump_args

    #[test]
    fn dump_no_flags_defaults() {
        let p = parse_db_dump_args(&[]).unwrap();
        assert!(p.out.is_none());
        assert!(!p.data_only);
        assert!(!p.schema_only);
        assert!(!p.no_owner);
    }

    #[test]
    fn dump_out_flag_with_value() {
        let p = parse_db_dump_args(&args(&["--out", "/tmp/db.sql"])).unwrap();
        assert_eq!(p.out.as_deref(), Some("/tmp/db.sql"));
    }

    #[test]
    fn dump_short_o_flag() {
        let p = parse_db_dump_args(&args(&["-o", "/tmp/db.sql"])).unwrap();
        assert_eq!(p.out.as_deref(), Some("/tmp/db.sql"));
    }

    #[test]
    fn dump_data_only_flag() {
        let p = parse_db_dump_args(&args(&["--data-only"])).unwrap();
        assert!(p.data_only);
        assert!(!p.schema_only);
    }

    #[test]
    fn dump_schema_only_flag() {
        let p = parse_db_dump_args(&args(&["--schema-only"])).unwrap();
        assert!(p.schema_only);
        assert!(!p.data_only);
    }

    #[test]
    fn dump_no_owner_flag() {
        let p = parse_db_dump_args(&args(&["--no-owner"])).unwrap();
        assert!(p.no_owner);
    }

    #[test]
    fn dump_out_without_value_errors() {
        let r = parse_db_dump_args(&args(&["--out"]));
        assert!(r.is_err());
    }

    #[test]
    fn dump_data_and_schema_only_conflict() {
        let r = parse_db_dump_args(&args(&["--data-only", "--schema-only"]));
        assert!(r.is_err());
    }

    #[test]
    fn dump_unknown_flag_errors() {
        let r = parse_db_dump_args(&args(&["--bogus"]));
        assert!(r.is_err());
    }

    // -------- build_pg_dump_argv

    #[test]
    fn dump_argv_contains_url_first() {
        let parsed = DbDumpArgs {
            out: None,
            data_only: false,
            schema_only: false,
            no_owner: false,
        };
        let argv = build_pg_dump_argv(&parsed, "postgres://u:p@h/db");
        assert_eq!(argv[0], "postgres://u:p@h/db");
    }

    #[test]
    fn dump_argv_includes_chosen_flags() {
        let parsed = DbDumpArgs {
            out: Some("/tmp/x.sql".into()),
            data_only: true,
            schema_only: false,
            no_owner: true,
        };
        let argv = build_pg_dump_argv(&parsed, "postgres://u:p@h/db");
        assert!(argv.contains(&"--data-only".to_owned()));
        assert!(argv.contains(&"--no-owner".to_owned()));
        assert!(argv.contains(&"--file".to_owned()));
        assert!(argv.contains(&"/tmp/x.sql".to_owned()));
        assert!(!argv.contains(&"--schema-only".to_owned()));
    }

    // -------- parse_db_restore_args

    #[test]
    fn restore_requires_file() {
        let r = parse_db_restore_args(&[]);
        assert!(r.is_err());
    }

    #[test]
    fn restore_positional_file() {
        let p = parse_db_restore_args(&args(&["/tmp/db.sql"])).unwrap();
        assert_eq!(p.file, "/tmp/db.sql");
        assert!(!p.clean);
    }

    #[test]
    fn restore_with_clean_flag() {
        let p = parse_db_restore_args(&args(&["--clean", "/tmp/db.sql"])).unwrap();
        assert!(p.clean);
        assert_eq!(p.file, "/tmp/db.sql");
    }

    #[test]
    fn restore_clean_after_file() {
        let p = parse_db_restore_args(&args(&["/tmp/db.sql", "--clean"])).unwrap();
        assert!(p.clean);
    }

    #[test]
    fn restore_two_files_errors() {
        let r = parse_db_restore_args(&args(&["a.sql", "b.sql"]));
        assert!(r.is_err());
    }

    // -------- build_psql_argv

    #[test]
    fn restore_argv_includes_on_error_stop() {
        let parsed = DbRestoreArgs {
            file: "/tmp/x.sql".into(),
            clean: false,
        };
        let argv = build_psql_argv(&parsed, "postgres://u:p@h/db");
        // ON_ERROR_STOP=1 prevents psql from continuing past errors
        // and silently "succeeding" with a half-restored DB.
        assert!(argv.contains(&"ON_ERROR_STOP=1".to_owned()));
        assert!(argv.contains(&"-f".to_owned()));
        assert!(argv.contains(&"/tmp/x.sql".to_owned()));
        assert!(!argv.iter().any(|a| a.contains("DROP SCHEMA")));
    }

    #[test]
    fn restore_argv_with_clean_drops_schema() {
        let parsed = DbRestoreArgs {
            file: "/tmp/x.sql".into(),
            clean: true,
        };
        let argv = build_psql_argv(&parsed, "postgres://u:p@h/db");
        assert!(argv.iter().any(|a| a.contains("DROP SCHEMA")));
        assert!(argv.iter().any(|a| a.contains("CREATE SCHEMA")));
    }

    // -------- redact_url

    #[test]
    fn redact_masks_password_in_postgres_url() {
        assert_eq!(
            redact_url("postgres://alice:supersecret@localhost:5432/mydb"),
            "postgres://alice:***@localhost:5432/mydb"
        );
    }

    #[test]
    fn redact_passes_through_url_without_credentials() {
        assert_eq!(
            redact_url("postgres://localhost:5432/mydb"),
            "postgres://localhost:5432/mydb"
        );
    }

    #[test]
    fn redact_passes_through_non_urls() {
        assert_eq!(redact_url("--data-only"), "--data-only");
        assert_eq!(redact_url("/tmp/db.sql"), "/tmp/db.sql");
    }

    #[test]
    fn redact_handles_url_with_only_user() {
        // No colon → no password to redact → pass through.
        assert_eq!(
            redact_url("postgres://alice@localhost/db"),
            "postgres://alice@localhost/db"
        );
    }
}
