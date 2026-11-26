//! `manage dbmigrate <verb>` (11.1): the engine v2 command group. Each
//! verb refuses an engine v1 project and names the command for it.

use std::io::Write;
use std::path::{Path, PathBuf};

use super::state::{Backend, Target};
use super::LoadError;
use crate::migrate::MigrateError;
use crate::sql::Pool;

/// The verbs; `make`, `sql` and `check --offline` never dial the database.
pub(crate) const VERBS: &[&str] = &["make", "apply", "down", "status", "check", "sql", "adopt"];

/// Whether `args` (after `dbmigrate`) needs no connection.
pub(crate) fn pool_free(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        Some("make" | "sql") | None => true,
        Some("check") => args.iter().any(|a| a == "--offline"),
        _ => false,
    }
}

fn usage(msg: impl std::fmt::Display) -> MigrateError {
    MigrateError::Validation(format!("dbmigrate: {msg}"))
}

fn load(e: LoadError) -> MigrateError {
    MigrateError::Validation(e.to_string())
}

fn run_err(e: super::run::RunError) -> MigrateError {
    e.into()
}

fn io(e: std::io::Error) -> MigrateError {
    MigrateError::Io(e)
}

/// The value after `flag`, if given.
fn value<'a>(args: &'a [String], flag: &str) -> Result<Option<&'a str>, MigrateError> {
    match args.iter().position(|a| a == flag) {
        None => Ok(None),
        Some(i) => args
            .get(i + 1)
            .map(|v| Some(v.as_str()))
            .ok_or_else(|| usage(format!("`{flag}` needs a value"))),
    }
}

/// Every value given after `flag`.
fn values<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
    args.windows(2)
        .filter(|w| w[0] == flag)
        .map(|w| w[1].as_str())
        .collect()
}

/// The first argument that is neither a flag nor a flag's value.
fn positional<'a>(args: &'a [String], with_value: &[&str]) -> Option<&'a str> {
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if with_value.contains(&a.as_str()) {
            skip = true;
            continue;
        }
        if !a.starts_with("--") {
            return Some(a);
        }
    }
    None
}

fn backend_named(s: &str) -> Result<Backend, MigrateError> {
    Ok(match s {
        "postgres" | "pg" | "postgresql" => Backend::Postgres,
        "mysql" => Backend::Mysql,
        "sqlite" => Backend::Sqlite,
        other => return Err(usage(format!("unknown backend `{other}`"))),
    })
}

/// `--backends a,b`, else the backend of the database URL.
fn backends(args: &[String], pool: &Pool) -> Result<Vec<Backend>, MigrateError> {
    match value(args, "--backends")? {
        Some(list) => list.split(',').map(|s| backend_named(s.trim())).collect(),
        None => Ok(vec![super::run::backend_of(pool)]),
    }
}

/// Run `manage dbmigrate <args>` on the project around `dir`.
///
/// # Errors
/// A usage error, the refusal of an engine v1 project, and every load,
/// check and run error.
pub(crate) async fn run<W: Write + Send>(
    pool: &Pool,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let verb = args.first().map_or("", String::as_str);
    let rest = args.get(1..).unwrap_or_default();
    match verb {
        "make" => make(pool, dir, rest, w),
        "apply" => apply(pool, dir, rest, w).await,
        "down" => down(pool, dir, rest, w).await,
        "status" => status(pool, dir, w).await,
        "check" => check(pool, dir, rest, w).await,
        "sql" => sql(pool, dir, rest, w),
        "adopt" => adopt(pool, dir, w),
        "" | "help" | "--help" | "-h" => {
            writeln!(w, "manage dbmigrate <{}>", VERBS.join("|")).map_err(io)
        }
        other => Err(usage(format!(
            "unknown verb `{other}`; one of: {}",
            VERBS.join(", ")
        ))),
    }
}

fn make<W: Write>(pool: &Pool, dir: &Path, args: &[String], w: &mut W) -> Result<(), MigrateError> {
    if let Some(name) = value(args, "--data-rust")? {
        let list = |flag: &str| -> Result<Vec<String>, MigrateError> {
            Ok(value(args, flag)?
                .map(|v| v.split(',').map(|s| s.trim().to_owned()).collect())
                .unwrap_or_default())
        };
        let folder = match value(args, "--app")? {
            Some(app) => crate::migrate::file::project_root_of(dir)
                .join(app)
                .join("migrations"),
            None => dir.to_path_buf(),
        };
        let made =
            super::make::make_data_rust(dir, &folder, name, &list("--tables")?, &list("--write")?)
                .map_err(load)?;
        writeln!(w, "Wrote {}", made.path.display()).map_err(io)?;
        for l in &made.lines {
            writeln!(w, "  {l}").map_err(io)?;
        }
        return Ok(());
    }
    let flags = ["--app", "--backends", "--fill", "--rename"];
    let mut fills = std::collections::BTreeMap::new();
    for f in values(args, "--fill") {
        let (k, v) = f
            .split_once('=')
            .ok_or_else(|| usage(format!("`--fill {f}`: write `table.column=<sql>`")))?;
        fills.insert(k.to_owned(), v.to_owned());
    }
    let mut renames = Vec::new();
    for r in values(args, "--rename") {
        let parsed = r.split_once('.').and_then(|(t, rest)| {
            rest.split_once(':')
                .map(|(from, to)| (t.to_owned(), from.to_owned(), to.to_owned()))
        });
        renames
            .push(parsed.ok_or_else(|| usage(format!("`--rename {r}`: write `table.old:new`")))?);
    }
    let opts = super::make::MakeOptions {
        backends: backends(args, pool)?,
        name: positional(args, &flags).map(str::to_owned),
        fills,
        dry_run: args.iter().any(|a| a == "--check"),
        renames,
        no_rename: args.iter().any(|a| a == "--no-rename"),
    };
    crate::migrate::make::check_models()?;
    let (folder, models): (PathBuf, _) = match value(args, "--app")? {
        Some(app) => (
            crate::migrate::file::project_root_of(dir)
                .join(app)
                .join("migrations"),
            crate::migrate::snapshot::SchemaSnapshot::from_registry_for_app(app),
        ),
        None => (
            dir.to_path_buf(),
            crate::migrate::snapshot::SchemaSnapshot::from_registry(),
        ),
    };
    match super::make::make(dir, &folder, &models, &opts).map_err(load)? {
        None => writeln!(w, "No changes detected.").map_err(io),
        Some(made) if opts.dry_run => Err(MigrateError::Validation(format!(
            "the models have changes no migration has: {} (run `manage dbmigrate make`)",
            made.lines.join("; ")
        ))),
        Some(made) => {
            writeln!(w, "Wrote {}", made.path.display()).map_err(io)?;
            for l in &made.lines {
                writeln!(w, "  {l}").map_err(io)?;
            }
            Ok(())
        }
    }
}

async fn apply<W: Write>(
    pool: &Pool,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let accept_step_change = match value(args, "--accept-step-change")? {
        None => None,
        Some(id) => {
            let why = value(args, "--reason")?
                .ok_or_else(|| usage("`--accept-step-change` needs `--reason <why>`"))?;
            Some((id.to_owned(), why.to_owned()))
        }
    };
    let max_duration = value(args, "--max-duration")?
        .map(|v| {
            v.trim_end_matches('s')
                .parse::<u64>()
                .map(std::time::Duration::from_secs)
                .map_err(|_| usage(format!("`--max-duration {v}`: seconds, e.g. 300")))
        })
        .transpose()?;
    let target = positional(
        args,
        &[
            "--accept-step-change",
            "--reason",
            "--max-duration",
            "--accept-drift",
            "--accept-failover",
        ],
    );
    let opts = super::run::Options {
        max_duration,
        per_run_limits: true,
        accept_step_change,
        accept_drift: value(args, "--accept-drift")?.map(str::to_owned),
        dry_run: args.iter().any(|a| a == "--plan" || a == "--dry-run"),
        accept_archive_size: args.iter().any(|a| a == "--accept-archive-size"),
        accept_failover: value(args, "--accept-failover")?.map(str::to_owned),
        resume_forward: args.iter().any(|a| a == "--resume-forward"),
        up_to: target.map(str::to_owned),
        ..super::run::Options::default()
    };
    // An applied target with later rows means rolling back (decision 6).
    if let Some(t) = target {
        let later = super::run::applied_after(pool, &opts, t)
            .await
            .map_err(run_err)?;
        if let Some(later) = later {
            if later.is_empty() {
                writeln!(w, "Nothing to apply.").map_err(io)?;
                return Ok(());
            }
            if !args.iter().any(|a| a == "--allow-rollback") {
                return Err(usage(format!(
                    "`apply {t}` would roll back {}: pass --allow-rollback, or use `down`",
                    later.join(", ")
                )));
            }
            let report = super::run::downgrade_to(pool, dir, t, &opts)
                .await
                .map_err(run_err)?;
            for id in &report.rolled_back {
                writeln!(w, "Rolled back {id}").map_err(io)?;
            }
            return Ok(());
        }
    }
    let report = super::run::migrate(pool, dir, &opts)
        .await
        .map_err(run_err)?;
    for p in &report.planned {
        writeln!(w, "{p}").map_err(io)?;
    }
    for warning in &report.warnings {
        writeln!(w, "warning: {warning}").map_err(io)?;
    }
    for id in &report.applied {
        writeln!(w, "Applied {id}").map_err(io)?;
    }
    if let Some(why) = &report.stopped {
        writeln!(w, "{}: {why}", super::run::STOPPED).map_err(io)?;
    } else if !opts.dry_run && report.applied.is_empty() {
        writeln!(w, "Nothing to apply.").map_err(io)?;
    }
    Ok(())
}

async fn down<W: Write>(
    pool: &Pool,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let opts = super::run::Options {
        accept_data_loss: args.iter().any(|a| a == "--accept-data-loss"),
        cascade: args.iter().any(|a| a == "--cascade"),
        ..super::run::Options::default()
    };
    let steps = match value(args, "--to")? {
        Some(to) => {
            let report = super::run::downgrade_to(pool, dir, to, &opts)
                .await
                .map_err(run_err)?;
            for id in &report.rolled_back {
                writeln!(w, "Rolled back {id}").map_err(io)?;
            }
            return Ok(());
        }
        None => match positional(args, &["--to"]) {
            Some(n) => n
                .parse()
                .map_err(|_| usage(format!("`{n}` is not a number of steps")))?,
            None => 1,
        },
    };
    let report = super::run::downgrade(pool, dir, steps, &opts)
        .await
        .map_err(run_err)?;
    for id in &report.rolled_back {
        writeln!(w, "Rolled back {id}").map_err(io)?;
    }
    Ok(())
}

async fn status<W: Write>(pool: &Pool, dir: &Path, w: &mut W) -> Result<(), MigrateError> {
    use super::ledger::RowState;
    let project = super::load::load_project(dir).map_err(load)?;
    let rows = super::ledger::Ledger::of(&super::run::Options::default())
        .rows(pool)
        .await?;
    for &i in project.order.get(&Target::Tenant).into_iter().flatten() {
        let f = &project.files[i].file;
        let row = rows.iter().find(|r| r.app == f.app && r.id == f.id);
        let mark = match row.map(|r| &r.state) {
            None => " ",
            Some(RowState::Applied) => "X",
            Some(RowState::Running | RowState::RollingBack) => "~",
            Some(RowState::Failed) => "!",
        };
        let note = match row {
            Some(r) if r.error_class.as_deref() == Some("stopped") => {
                format!(" ({})", super::run::STOPPED)
            }
            _ => String::new(),
        };
        writeln!(w, "[{mark}] {}:{}{note}", f.app, f.id).map_err(io)?;
    }
    Ok(())
}

async fn check<W: Write>(
    pool: &Pool,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    crate::migrate::make::check_models()?;
    let models = crate::migrate::snapshot::SchemaSnapshot::from_registry();
    let online = (!args.iter().any(|a| a == "--offline")).then_some(pool);
    let found = super::check::check(dir, &backends(args, pool)?, Some(&models), online)
        .await
        .map_err(load)?;
    for f in &found {
        writeln!(w, "{f}").map_err(io)?;
    }
    let errors = found
        .iter()
        .filter(|f| f.severity == super::check::Severity::Error)
        .count();
    if errors > 0 {
        return Err(MigrateError::Validation(format!(
            "dbmigrate check: {errors} error(s)"
        )));
    }
    writeln!(w, "No issues found.").map_err(io)
}

fn sql<W: Write>(pool: &Pool, dir: &Path, args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let target = positional(args, &["--dialect"])
        .ok_or_else(|| usage("`sql` needs a migration id (`<id>` or `<app>:<id>`)"))?;
    let list = match value(args, "--dialect")? {
        Some("all") => vec![Backend::Postgres, Backend::Mysql, Backend::Sqlite],
        Some(d) => vec![backend_named(d)?],
        None => vec![super::run::backend_of(pool)],
    };
    let project = super::load::load_project(dir).map_err(load)?;
    let (app, id) = target
        .split_once(':')
        .map_or((None, target), |(a, i)| (Some(a), i));
    let idx = project
        .files
        .iter()
        .position(|f| f.file.id == id && app.is_none_or(|a| a == f.file.app))
        .ok_or_else(|| usage(format!("no migration `{target}`")))?;
    for b in list {
        let units = super::render::render_project(&project, b).map_err(load)?;
        let mine = units
            .into_iter()
            .find(|(i, _)| *i == idx)
            .map(|(_, u)| u)
            .unwrap_or_default();
        writeln!(w, "-- {} on {}", target, b.as_str()).map_err(io)?;
        write!(w, "{}", super::render::golden(&mine)).map_err(io)?;
    }
    Ok(())
}

fn adopt<W: Write>(pool: &Pool, dir: &Path, w: &mut W) -> Result<(), MigrateError> {
    let report = super::adopt::adopt(dir, super::run::backend_of(pool)).map_err(load)?;
    for p in &report.written {
        writeln!(w, "Wrote {}", p.display()).map_err(io)?;
    }
    for p in &report.removed {
        writeln!(w, "Removed {}", p.display()).map_err(io)?;
    }
    for s in &report.data_steps {
        writeln!(w, "Not kept (does not run on fresh installs): {s}").map_err(io)?;
    }
    writeln!(
        w,
        "Next: run `manage dbmigrate apply` on each database to switch it."
    )
    .map_err(io)
}

#[cfg(all(test, feature = "sqlite"))]
#[path = "cli_tests.rs"]
mod tests;
