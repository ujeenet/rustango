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

use std::path::Path;

use rustango_sql::sqlx::PgPool;

use crate::error::MigrateError;
use crate::file::{self, Migration, Operation};
use crate::make::make_migrations;
use crate::runner;
use crate::snapshot::SchemaSnapshot;

/// Parse argv (no binary name) and dispatch to the right subcommand.
///
/// `dir` is the migrations directory (e.g. `./migrations`).
///
/// # Errors
/// Returns whatever the underlying migration function returns, plus
/// [`MigrateError::Validation`] for unknown subcommands or bad argv.
pub async fn run(
    pool: &PgPool,
    dir: &Path,
    args: impl IntoIterator<Item = String>,
) -> Result<(), MigrateError> {
    let args: Vec<String> = args.into_iter().collect();
    let cmd = args.first().map_or("", String::as_str);

    match cmd {
        "" | "--help" | "-h" | "help" => {
            print_help();
            Ok(())
        }
        "makemigrations" => makemigrations(dir, &args[1..]),
        "migrate" => migrate(pool, dir, &args[1..]).await,
        "downgrade" => downgrade(pool, dir, &args[1..]).await,
        "showmigrations" | "status" => showmigrations(pool, dir).await,
        other => Err(MigrateError::Validation(format!(
            "unknown subcommand: `{other}` (run with --help for usage)"
        ))),
    }
}

fn print_help() {
    println!("rustango::manage — Django-style migration runner\n");
    println!("USAGE:");
    println!("  manage <COMMAND> [args]\n");
    println!("COMMANDS:");
    println!("  makemigrations [name]");
    println!("      Diff the inventory registry against the latest snapshot");
    println!("      and write the next migration file. `name` overrides the");
    println!("      auto-derived suffix.\n");
    println!("  makemigrations --empty <name>");
    println!("      Write an empty migration scaffold (`forward: []`) for");
    println!("      hand-authored data migrations. Edit the JSON to add");
    println!("      `data` ops with sql + reverse_sql.\n");
    println!("  migrate");
    println!("      Apply every pending migration in lex order.\n");
    println!("  migrate <target>");
    println!("      Forward or back to <target>. `zero` unapplies every");
    println!("      applied migration.\n");
    println!("  downgrade [N]");
    println!("      Step back N applied migrations (default 1).\n");
    println!("  showmigrations | status");
    println!("      List migrations with [X]/[ ] applied marker.");
}

fn makemigrations(dir: &Path, args: &[String]) -> Result<(), MigrateError> {
    let mut empty = false;
    let mut name: Option<String> = None;
    for arg in args {
        match arg.as_str() {
            "--empty" => empty = true,
            "--help" | "-h" => {
                println!(
                    "makemigrations [name]            generate next migration\n\
                     makemigrations --empty <name>    empty scaffold for data ops"
                );
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
        println!(
            "wrote {} (empty scaffold — fill in `forward` with data ops)",
            file_path(dir, &mig.name).display()
        );
        return Ok(());
    }

    match make_migrations(dir, name.as_deref())? {
        Some(mig) => {
            println!("wrote {}", file_path(dir, &mig.name).display());
            for op in &mig.forward {
                println!("    + {}", describe_op(op));
            }
        }
        None => println!("no changes — registry matches latest snapshot"),
    }
    Ok(())
}

async fn migrate(pool: &PgPool, dir: &Path, args: &[String]) -> Result<(), MigrateError> {
    if args.is_empty() {
        let applied = runner::migrate(pool, dir).await?;
        if applied.is_empty() {
            println!("nothing to migrate (already up to date)");
        } else {
            for m in &applied {
                println!("  applied {}", m.name);
            }
        }
        return Ok(());
    }
    let target = &args[0];
    let touched = runner::migrate_to(pool, dir, target).await?;
    if touched.is_empty() {
        println!("already at {target}");
    } else {
        for m in &touched {
            println!("  touched {}", m.name);
        }
    }
    Ok(())
}

async fn downgrade(pool: &PgPool, dir: &Path, args: &[String]) -> Result<(), MigrateError> {
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
        println!("nothing to downgrade");
    } else {
        for m in &touched {
            println!("  rolled back {}", m.name);
        }
    }
    Ok(())
}

async fn showmigrations(pool: &PgPool, dir: &Path) -> Result<(), MigrateError> {
    runner::ensure_ledger(pool).await?;
    let all = file::list_dir(dir)?;
    let applied = runner::applied_set(pool).await?;

    if all.is_empty() {
        println!("(no migrations in {})", dir.display());
        return Ok(());
    }
    println!("Migrations in {}:", dir.display());
    for m in &all {
        let mark = if applied.contains(&m.name) {
            "[X]"
        } else {
            "[ ]"
        };
        println!("  {mark} {}", m.name);
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
