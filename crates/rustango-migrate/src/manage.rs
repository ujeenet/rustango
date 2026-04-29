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

use rustango_sql::sqlx::PgPool;

use crate::error::MigrateError;
use crate::file::{self, Migration, Operation};
use crate::make::make_migrations;
use crate::runner;
use crate::snapshot::SchemaSnapshot;

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
    writeln!(w, "      List migrations with [X]/[ ] applied marker.")?;
    Ok(())
}

fn makemigrations<W: Write>(
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let mut empty = false;
    let mut name: Option<String> = None;
    for arg in args {
        match arg.as_str() {
            "--empty" => empty = true,
            "--help" | "-h" => {
                writeln!(
                    w,
                    "makemigrations [name]            generate next migration\n\
                     makemigrations --empty <name>    empty scaffold for data ops"
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
/// As [`crate::file::list_dir`] / [`crate::file::write`].
pub fn make_empty(dir: &Path, name: &str) -> Result<Migration, MigrateError> {
    let prior = file::list_dir(dir)?;
    let prev_snapshot = prior
        .last()
        .map_or_else(|| SchemaSnapshot { tables: vec![] }, |m| m.snapshot.clone());
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
        scope: crate::MigrationScope::default(),
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
