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
        "make:viewset" => make_viewset_cmd(&args[1..], writer),
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
    writeln!(w, "  make:viewset <Name> [--model <Model>]")?;
    writeln!(w, "  make:serializer <Name> [--model <Model>]")?;
    writeln!(w, "  make:form <Name>")?;
    writeln!(w, "  make:job <Name>")?;
    writeln!(w, "  make:notification <Name>")?;
    writeln!(w, "  make:middleware <Name>")?;
    writeln!(w, "  make:test <Name>")?;
    writeln!(w, "      Scaffold a single source file with the chosen shape.")?;
    writeln!(w, "      Writes to src/<snake_name>.rs (skips if exists).\n")?;
    writeln!(w, "  db:dump [--out <file>] [--data-only|--schema-only] [--no-owner]")?;
    writeln!(w, "      Run pg_dump against $DATABASE_URL. Default: prints SQL to")?;
    writeln!(w, "      stdout (omit --out to pipe). --data-only / --schema-only")?;
    writeln!(w, "      mirror pg_dump's flags. --no-owner skips OWNER lines.\n")?;
    writeln!(w, "  db:restore <file> [--clean]")?;
    writeln!(w, "      Run psql against $DATABASE_URL with `\\i <file>`. With")?;
    writeln!(w, "      --clean, prepend a `DROP SCHEMA public CASCADE; CREATE SCHEMA public;`")?;
    writeln!(w, "      so the restore lands on a clean database.\n")?;
    writeln!(w, "  db:info")?;
    writeln!(w, "      Print the resolved DB URL (password redacted), detected")?;
    writeln!(w, "      backend, and which `postgres`/`mysql` Cargo features are")?;
    writeln!(w, "      compiled in. Read-only — does not connect.\n")?;
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


// ============================================================ make:* generators

fn parse_name_and_model(args: &[String]) -> Result<(String, Option<String>), MigrateError> {
    let mut name: Option<String> = None;
    let mut model: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--model" => {
                model = Some(iter.next().cloned().ok_or_else(|| {
                    MigrateError::Validation("--model requires a value".into())
                })?);
            }
            other if other.starts_with('-') => {
                return Err(MigrateError::Validation(format!(
                    "unknown flag `{other}`"
                )));
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
    writeln!(w, "  add `pub mod {};` to your src/lib.rs", file_name.trim_end_matches(".rs"))?;
    Ok(())
}

fn make_viewset_cmd<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let (name, model) = parse_name_and_model(args)?;
    let snake = pascal_to_snake(&name);
    let model = model.unwrap_or_else(|| "Post".into());
    let body = format!(
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
    );
    write_generated(w, &format!("{snake}.rs"), body)
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
                        .ok_or_else(|| {
                            MigrateError::Validation("--out requires a path".into())
                        })?,
                );
            }
            "--data-only" => data_only = true,
            "--schema-only" => schema_only = true,
            "--no-owner" => no_owner = true,
            other => {
                return Err(MigrateError::Validation(format!(
                    "unknown flag: {other}"
                )));
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
            MigrateError::Validation(format!(
                "could not run pg_dump (is it on PATH?): {e}"
            ))
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
                return Err(MigrateError::Validation(format!(
                    "unknown flag: {other}"
                )));
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
        argv.push(
            "DROP SCHEMA IF EXISTS public CASCADE; CREATE SCHEMA public;".into(),
        );
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
            MigrateError::Validation(format!(
                "could not run psql (is it on PATH?): {e}"
            ))
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
            let scheme = url
                .split("://")
                .next()
                .unwrap_or("(unknown)");
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
        assert!(!is_valid_type_name("post"));     // lowercase
        assert!(!is_valid_type_name("123Foo"));   // starts with digit
        assert!(!is_valid_type_name("Post!"));    // bad char
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
