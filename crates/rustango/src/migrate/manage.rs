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
//! UX: `cargo run --bin manage -- migrate`,
//! `cargo run --bin manage -- makemigrations [name]`, etc. The
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

use crate::sql::sqlx::PgPool;

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
    pool: &PgPool,
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
    pool: &PgPool,
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
        "startapp" => startapp(&args[1..], writer),
        "add-data-op" => add_data_op_cmd(dir, &args[1..], writer),
        "about" => about_cmd(pool, writer).await,
        "check" => check_cmd(pool, dir, &args[1..], writer).await,
        "docs" => docs_cmd(writer),
        "version" | "--version" => version_cmd(writer),
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
    writeln!(w, "      writes. Reads the ledger so the preview is accurate.\n")?;
    writeln!(w, "  downgrade [N]")?;
    writeln!(
        w,
        "      Step back N applied migrations (default 1).\n"
    )?;
    writeln!(w, "  showmigrations | status")?;
    writeln!(w, "      List migrations with [X]/[ ] applied marker.\n")?;
    writeln!(w, "  add-data-op --sql <SQL> [--reverse-sql <SQL>] [--name <name>] [--to <migration>]")?;
    writeln!(w, "      Add a data transformation op (up + optional down).")?;
    writeln!(w, "      --sql        Forward SQL to run (required).")?;
    writeln!(w, "      --reverse-sql  Rollback SQL. Omit for irreversible ops.")?;
    writeln!(w, "      --name       Name suffix for the new migration file.")?;
    writeln!(w, "      --to         Append to an existing migration instead of creating one.\n")?;
    writeln!(w, "  about")?;
    writeln!(w, "      Print framework version, registered models/apps,")?;
    writeln!(w, "      and detected backend configuration.\n")?;
    writeln!(w, "  check [--deploy]")?;
    writeln!(w, "      Run system audits — pending migrations, missing models, common")?;
    writeln!(w, "      misconfigurations. With --deploy: production hardening checks.")?;
    writeln!(w, "      Exits non-zero on any error-level finding.\n")?;
    writeln!(w, "  docs")?;
    writeln!(w, "      Open docs.rs/rustango in the default browser.\n")?;
    writeln!(w, "  version | --version")?;
    writeln!(w, "      Print the rustango framework version.\n")?;
    writeln!(w, "  (To bootstrap a new project from scratch, install + run")?;
    writeln!(w, "  `cargo install cargo-rustango` then `cargo rustango new <name>`.)\n")?;
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

fn makemigrations<W: Write>(
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let mut empty = false;
    let mut name: Option<String> = None;
    let mut app: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--empty" => empty = true,
            "--app" => {
                app = Some(
                    iter.next()
                        .cloned()
                        .ok_or_else(|| {
                            MigrateError::Validation("--app requires an app name".into())
                        })?,
                );
            }
            "--help" | "-h" => {
                writeln!(
                    w,
                    "makemigrations [name]                  diff registry, write next file in <dir>\n\
                     makemigrations --app <app> [name]      diff one app, write to <project_root>/<app>/migrations/\n\
                     makemigrations --empty <name>          empty scaffold for data ops"
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

async fn migrate<W: Write>(
    pool: &PgPool,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let mut dry_run = false;
    let mut positional: Option<&str> = None;
    for arg in args {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--help" | "-h" => {
                writeln!(
                    w,
                    "migrate                    apply pending migrations\n\
                     migrate <target>           forward or back to <target> (`zero` wipes)\n\
                     migrate --dry-run          preview the SQL without writing"
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

    if dry_run {
        if positional.is_some() {
            return Err(MigrateError::Validation(
                "`migrate <target> --dry-run` is not supported in v0.4 — use plain `--dry-run` to preview pending forward migrations".into(),
            ));
        }
        let preview = runner::migrate_dry_run(pool, dir).await?;
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
        let touched = runner::migrate_to(pool, dir, target).await?;
        if touched.is_empty() {
            writeln!(w, "already at {target}")?;
        } else {
            for m in &touched {
                writeln!(w, "  touched {}", m.name)?;
            }
        }
        return Ok(());
    }

    let applied = runner::migrate(pool, dir).await?;
    if applied.is_empty() {
        writeln!(w, "nothing to migrate (already up to date)")?;
    } else {
        for m in &applied {
            writeln!(w, "  applied {}", m.name)?;
        }
    }
    Ok(())
}

async fn downgrade<W: Write>(
    pool: &PgPool,
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
    let touched = runner::downgrade(pool, dir, steps).await?;
    if touched.is_empty() {
        writeln!(w, "nothing to downgrade")?;
    } else {
        for m in &touched {
            writeln!(w, "  rolled back {}", m.name)?;
        }
    }
    Ok(())
}

async fn showmigrations<W: Write>(
    pool: &PgPool,
    dir: &Path,
    w: &mut W,
) -> Result<(), MigrateError> {
    runner::ensure_ledger(pool).await?;
    let all = file::list_dir(dir)?;
    let applied = runner::applied_set(pool).await?;

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
    let prev_snapshot = prior
        .last()
        .map_or_else(|| SchemaSnapshot { tables: vec![], m2m_tables: vec![], indexes: vec![], checks: vec![] }, |m| m.snapshot.clone());
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
                sql = Some(
                    iter.next()
                        .cloned()
                        .ok_or_else(|| MigrateError::Validation("--sql requires a value".into()))?,
                );
            }
            "--reverse-sql" => {
                reverse_sql = Some(
                    iter.next().cloned().ok_or_else(|| {
                        MigrateError::Validation("--reverse-sql requires a value".into())
                    })?,
                );
            }
            "--name" => {
                name = Some(
                    iter.next().cloned().ok_or_else(|| {
                        MigrateError::Validation("--name requires a value".into())
                    })?,
                );
            }
            "--to" => {
                to = Some(
                    iter.next().cloned().ok_or_else(|| {
                        MigrateError::Validation("--to requires a migration name".into())
                    })?,
                );
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
        let rev_note = if reverse_sql.is_some() { " (reversible)" } else { " (irreversible)" };
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
                into = Some(
                    iter.next()
                        .cloned()
                        .ok_or_else(|| {
                            MigrateError::Validation(
                                "--into requires a directory argument".into(),
                            )
                        })?,
                );
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
    // `cargo run --bin manage -- startapp …` from the project root,
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
async fn about_cmd<W: Write>(pool: &PgPool, w: &mut W) -> Result<(), MigrateError> {
    let registered_models = crate::core::inventory::iter::<crate::core::ModelEntry>
        .into_iter()
        .count();
    let mut apps: std::collections::BTreeSet<&'static str> =
        std::collections::BTreeSet::new();
    for entry in crate::core::inventory::iter::<crate::core::ModelEntry> {
        if let Some(app) = entry.resolved_app_label() {
            apps.insert(app);
        }
    }

    writeln!(w, "rustango")?;
    writeln!(w, "  version:        {}", env!("CARGO_PKG_VERSION"))?;
    writeln!(w, "  models:         {registered_models} registered")?;
    writeln!(w, "  apps:           {} ({})",
        apps.len(),
        if apps.is_empty() { "none".to_owned() }
        else { apps.iter().copied().collect::<Vec<_>>().join(", ") }
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

    // DB connectivity
    write!(w, "  db_connect:     ")?;
    let ok = sqlx::query("SELECT 1").execute(pool).await.is_ok();
    writeln!(w, "{}", if ok { "ok" } else { "FAILED" })?;

    Ok(())
}

/// `manage check [--deploy]` — run system audits.
async fn check_cmd<W: Write>(
    pool: &PgPool,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let deploy = args.iter().any(|a| a == "--deploy");
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut info: Vec<String> = Vec::new();

    writeln!(w, "running rustango system check{}...", if deploy { " (deploy mode)" } else { "" })?;

    // Always-on checks
    let model_count = crate::core::inventory::iter::<crate::core::ModelEntry>
        .into_iter()
        .count();
    if model_count == 0 {
        errors.push("no models registered — every #[derive(Model)] struct must be `pub use`d through the binary's crate root".into());
    } else {
        info.push(format!("{model_count} models registered via inventory"));
    }

    // DB connectivity
    if sqlx::query("SELECT 1").execute(pool).await.is_err() {
        errors.push("cannot connect to database — verify DATABASE_URL is reachable".into());
    } else {
        info.push("database reachable".into());
    }

    // Pending migrations
    if dir.exists() {
        let prior = file::list_dir(dir)?;
        if prior.is_empty() && model_count > 0 {
            warnings.push("models registered but no migrations on disk — run `manage makemigrations`".into());
        } else {
            info.push(format!("{} migration(s) on disk", prior.len()));
        }
    }

    // Deploy checks
    if deploy {
        // DEBUG/dev-mode env vars
        if std::env::var("RUSTANGO_ENV").as_deref() != Ok("prod")
            && std::env::var("RUSTANGO_ENV").as_deref() != Ok("production")
        {
            warnings.push("RUSTANGO_ENV is not 'prod' or 'production'".into());
        }
        // Secret key length
        match std::env::var("SECRET_KEY") {
            Ok(s) if s.len() < 32 => {
                errors.push(format!("SECRET_KEY is only {} bytes — need ≥ 32 for cookie signing", s.len()));
            }
            Err(_) => {
                warnings.push("SECRET_KEY env var not set (operator console / sessions need this)".into());
            }
            _ => info.push("SECRET_KEY length OK".into()),
        }
        // DATABASE_URL set
        if std::env::var("DATABASE_URL").is_err() {
            errors.push("DATABASE_URL must be set in production".into());
        }
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

