//! `manage dbmigrate <verb>` (11.1): the engine v2 command group. Each
//! verb refuses an engine v1 project and names the command for it.

use std::io::Write;
use std::path::{Path, PathBuf};

use super::state::{Backend, Target};
use super::LoadError;
use crate::migrate::MigrateError;
use crate::sql::Pool;

/// The verbs; `make`, `sql` and `check --offline` never dial the database.
pub(crate) const VERBS: &[&str] = &[
    "make", "apply", "down", "status", "check", "sql", "adopt", "squash",
];

/// `dbmigrate --help`. Under tenancy `apply`, `down`, `status`,
/// `check --drift` and `sql` work on the registry and every tenant.
const HELP: &str = "manage dbmigrate <verb>   migrations engine v2
  make [name] [--app <app>]        write the next migration from the models
  apply [<id>] [--plan]            apply pending migrations (--plan: print, run nothing)
  down [<n> | --to <id>] [--plan]  roll back; under tenancy add --registry,
                                   --tenant <slug> or --all-tenants
  status                           applied and pending migrations
  check [--offline | --drift]      validate files; --drift compares the database
  sql <id> [--dialect <db>|all]    print a migration's SQL
  adopt                            switch an engine v1 project (one way)
  squash <app> <from>..<to>        write a checkpoint; --prune <id> deletes files
";

/// Whether `args` (after `dbmigrate`) needs no connection.
#[cfg(any(feature = "manage", all(feature = "tenancy", feature = "postgres")))]
pub(crate) fn pool_free(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        Some("make" | "sql" | "help" | "--help" | "-h") | None => true,
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

/// `--database <alias>`: that configured database, whose `database` chain
/// the verb works on.
fn database(args: &[String]) -> Result<Option<(String, Pool)>, MigrateError> {
    let Some(db) = value(args, "--database")? else {
        return Ok(None);
    };
    let pool = crate::databases::get(db).ok_or_else(|| {
        usage(format!(
            "database `{db}` is not configured: register it with \
             `rustango::databases::register(\"{db}\", pool)`"
        ))
    })?;
    Ok(Some((db.to_owned(), pool)))
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

/// Where `dbmigrate` works: one database, or under tenancy the fleet, the
/// registry and each tenant. The tenancy CLI passes `Fleet`, so a tenancy
/// project never reaches the single-database runner.
pub(crate) enum Route<'a, F: Fleet = NoFleet> {
    Single(&'a Pool),
    #[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
    Fleet(&'a F),
}

impl<F: Fleet> Clone for Route<'_, F> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<F: Fleet> Copy for Route<'_, F> {}

impl<F: Fleet> Route<'_, F> {
    /// The database of a verb with no per-database form: the registry
    /// under tenancy.
    pub(crate) fn pool(&self) -> Pool {
        match self {
            Self::Single(p) => (*p).clone(),
            Self::Fleet(f) => f.registry(),
        }
    }
}

/// The verbs that work per database under tenancy, as the tenancy CLI runs them.
pub(crate) trait Fleet: Sync {
    fn registry(&self) -> Pool;
    async fn apply(&self, dir: &Path, args: &[String], w: Out<'_>) -> Result<(), MigrateError>;
    async fn down(&self, dir: &Path, args: &[String], w: Out<'_>) -> Result<(), MigrateError>;
    async fn status(&self, dir: &Path, args: &[String], w: Out<'_>) -> Result<(), MigrateError>;
    async fn drift(&self, dir: &Path, args: &[String], w: Out<'_>) -> Result<(), MigrateError>;
    /// The registry's backend and each tenant's, for `sql` without `--dialect`.
    async fn backends(&self) -> Result<Vec<Backend>, MigrateError>;
    async fn prune_gate(
        &self,
        dir: &Path,
        id: &str,
    ) -> Result<super::squash::PruneGate, MigrateError>;
}

/// A verb's output under tenancy.
pub(crate) type Out<'a> = &'a mut (dyn Write + Send);

/// No fleet: the single-database CLI.
pub(crate) enum NoFleet {}

impl Fleet for NoFleet {
    fn registry(&self) -> Pool {
        match *self {}
    }
    async fn apply(&self, _: &Path, _: &[String], _: Out<'_>) -> Result<(), MigrateError> {
        match *self {}
    }
    async fn down(&self, _: &Path, _: &[String], _: Out<'_>) -> Result<(), MigrateError> {
        match *self {}
    }
    async fn status(&self, _: &Path, _: &[String], _: Out<'_>) -> Result<(), MigrateError> {
        match *self {}
    }
    async fn drift(&self, _: &Path, _: &[String], _: Out<'_>) -> Result<(), MigrateError> {
        match *self {}
    }
    async fn backends(&self) -> Result<Vec<Backend>, MigrateError> {
        match *self {}
    }
    async fn prune_gate(
        &self,
        _: &Path,
        _: &str,
    ) -> Result<super::squash::PruneGate, MigrateError> {
        match *self {}
    }
}

/// Run `manage dbmigrate <args>` on the project around `dir`.
///
/// # Errors
/// A usage error, the refusal of an engine v1 project, and every load,
/// check and run error.
pub(crate) async fn run<W: Write + Send, F: Fleet>(
    route: Route<'_, F>,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let verb = args.first().map_or("", String::as_str);
    let rest = args.get(1..).unwrap_or_default();
    let pool = &route.pool();
    match (verb, route) {
        ("make", _) if value(rest, "--rebase")?.is_some() => rebase(pool, dir, rest, w).await,
        ("make", _) if rest.iter().any(|a| a == "--merge") => {
            let path = super::merge::merge(dir, &app_folder(dir, rest)?).map_err(load)?;
            writeln!(w, "Wrote {}", path.display()).map_err(io)
        }
        ("make", _) => make(pool, matches!(route, Route::Fleet(_)), dir, rest, w),
        ("apply", Route::Single(pool)) => {
            // SIGTERM or Ctrl-C: the unit in flight finishes, then it stops (6.11).
            let on_signal = crate::shutdown::StopOnSignal::listen();
            apply(pool, dir, rest, w, on_signal.flag()).await
        }
        ("down", Route::Single(pool)) => down(pool, dir, rest, w).await,
        ("status", Route::Single(pool)) => status(pool, dir, rest, w).await,
        ("check", Route::Single(pool)) if rest.iter().any(|a| a == "--drift") => {
            drift(pool, dir, rest, w).await
        }
        ("apply", Route::Fleet(f)) => f.apply(dir, rest, w).await,
        ("down", Route::Fleet(f)) => f.down(dir, rest, w).await,
        ("status", Route::Fleet(f)) => f.status(dir, rest, w).await,
        ("check", Route::Fleet(f)) if rest.iter().any(|a| a == "--drift") => {
            f.drift(dir, rest, w).await
        }
        ("check", _) => check(pool, dir, rest, w).await,
        ("sql", _) => {
            // `--dialect` reads nothing; the fleet's backends need the registry.
            let defaults = match route {
                _ if value(rest, "--dialect")?.is_some() => Vec::new(),
                Route::Single(p) => vec![super::run::backend_of(p)],
                Route::Fleet(f) => f.backends().await?,
            };
            sql(&defaults, dir, rest, w)
        }
        ("adopt", _) => adopt(pool, dir, w),
        ("squash", _) => match value(rest, "--prune")? {
            Some(id) => {
                let gate = prune_gate(route, dir, rest, id).await?;
                prune_with(dir, rest, id, &gate, w)
            }
            None => make_squash(dir, rest, w),
        },
        ("" | "help" | "--help" | "-h", _) => w.write_all(HELP.as_bytes()).map_err(io),
        (other, _) => Err(usage(format!(
            "unknown verb `{other}`; one of: {}",
            VERBS.join(", ")
        ))),
    }
}

fn make<W: Write>(
    pool: &Pool,
    tenancy: bool,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
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
        no_split: args.iter().any(|a| a == "--no-split"),
        tenancy: tenancy || args.iter().any(|a| a == "--tenancy"),
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

/// What lets `squash --prune <checkpoint> [--app <app>] [--assume-all-applied]`
/// delete the files it replaces: this database's ledger, or every one
/// under tenancy.
async fn prune_gate<F: Fleet>(
    route: Route<'_, F>,
    dir: &Path,
    args: &[String],
    id: &str,
) -> Result<super::squash::PruneGate, MigrateError> {
    if args.iter().any(|a| a == "--assume-all-applied") {
        return Ok(super::squash::PruneGate::Assumed);
    }
    match route {
        Route::Single(pool) => Ok(super::squash::PruneGate::Here {
            applied: super::ledger::Ledger::default_names()
                .rows(pool)
                .await?
                .iter()
                .any(|r| r.id == id && r.state == super::ledger::RowState::Applied),
        }),
        Route::Fleet(f) => f.prune_gate(dir, id).await,
    }
}

/// `squash --prune` once its gate is known.
fn prune_with<W: Write>(
    dir: &Path,
    args: &[String],
    id: &str,
    gate: &super::squash::PruneGate,
    w: &mut W,
) -> Result<(), MigrateError> {
    let removed = super::squash::prune(dir, &app_folder(dir, args)?, id, gate).map_err(load)?;
    for p in removed {
        writeln!(w, "Removed {}", p.display()).map_err(io)?;
    }
    Ok(())
}

fn make_squash<W: Write>(dir: &Path, args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let free: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let (Some(app), Some(range)) = (free.first(), free.get(1)) else {
        return Err(usage("squash <app> <from>..<to>"));
    };
    let (from, to) = range
        .split_once("..")
        .ok_or_else(|| usage("squash <app> <from>..<to>"))?;
    let folder = if app.as_str() == "project" {
        dir.to_path_buf()
    } else {
        crate::migrate::file::project_root_of(dir)
            .join(app.as_str())
            .join("migrations")
    };
    let path = super::squash::squash(dir, &folder, app, from, to).map_err(load)?;
    writeln!(w, "Wrote {}", path.display()).map_err(io)
}

/// The folder `--app` names, else the flat one.
fn app_folder(dir: &Path, args: &[String]) -> Result<PathBuf, MigrateError> {
    Ok(match value(args, "--app")? {
        Some(app) => crate::migrate::file::project_root_of(dir)
            .join(app)
            .join("migrations"),
        None => dir.to_path_buf(),
    })
}

/// `make --rebase <id> [--force]`: refused when this database ran it or a
/// remote branch has it.
async fn rebase<W: Write>(
    pool: &Pool,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let id = value(args, "--rebase")?.unwrap_or_default();
    let folder = app_folder(dir, args)?;
    let applied = super::ledger::Ledger::default_names()
        .rows(pool)
        .await?
        .iter()
        .any(|r| r.id == id);
    let shared = super::merge::on_remote_branch(&folder.join(format!("{id}.json")));
    let force = args.iter().any(|a| a == "--force");
    let path = super::merge::rebase(dir, &folder, id, applied, shared, force).map_err(load)?;
    writeln!(w, "Moved {id} to {}", path.display()).map_err(io)
}

/// `apply`'s flags; a typo is refused, never ignored.
const APPLY: crate::cli_args::Spec<'static> = crate::cli_args::Spec {
    verb: "dbmigrate apply",
    usage: "dbmigrate apply [<id>] [--plan | --dry-run] [--allow-rollback] [--database <alias>] \
            [--accept-drift <why>] [--accept-step-change <step> --reason <why>] \
            [--max-duration <secs>] [--accept-archive-size] [--accept-failover <why>] \
            [--resume-forward]",
    switches: &[
        "--plan",
        "--dry-run",
        "--allow-rollback",
        "--accept-archive-size",
        "--resume-forward",
    ],
    valued: &[
        "--accept-step-change",
        "--reason",
        "--max-duration",
        "--accept-drift",
        "--accept-failover",
        "--database",
    ],
    max_positionals: 1,
};

/// `down`'s flags.
const DOWN: crate::cli_args::Spec<'static> = crate::cli_args::Spec {
    verb: "dbmigrate down",
    usage: "dbmigrate down [<n> | --to <id>] [--plan | --dry-run] [--accept-data-loss] \
            [--cascade] [--database <alias>]",
    switches: &["--plan", "--dry-run", "--accept-data-loss", "--cascade"],
    valued: &["--to", "--database"],
    max_positionals: 1,
};

/// Each planned statement, or each rolled-back id, under `label` when given.
pub(crate) fn write_back<W: Write + ?Sized>(
    w: &mut W,
    label: Option<&str>,
    report: &super::run::Report,
    dry_run: bool,
) -> Result<(), MigrateError> {
    let head = label.map(|l| format!("{l}: ")).unwrap_or_default();
    if dry_run {
        if let Some(l) = label {
            writeln!(w, "-- {l}").map_err(io)?;
        }
        for p in &report.planned {
            writeln!(w, "{p}").map_err(io)?;
        }
        if report.planned.is_empty() {
            writeln!(w, "{head}nothing to roll back").map_err(io)?;
        }
        return Ok(());
    }
    for id in &report.rolled_back {
        writeln!(w, "{head}Rolled back {id}").map_err(io)?;
    }
    if report.rolled_back.is_empty() {
        writeln!(w, "{head}nothing to roll back").map_err(io)?;
    }
    Ok(())
}

/// `apply` on one database; `stop` set by a shutdown signal ends it after
/// the unit in flight, with an error.
pub(crate) async fn apply<W: Write>(
    pool: &Pool,
    dir: &Path,
    args: &[String],
    w: &mut W,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), MigrateError> {
    crate::cli_args::parse(args, &APPLY)?;
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
            "--database",
        ],
    );
    let db = database(args)?;
    let pool = db.as_ref().map_or(pool, |(_, p)| p);
    let opts = super::run::Options {
        database: db.as_ref().map(|(d, _)| d.clone()),
        max_duration,
        per_run_limits: true,
        accept_step_change,
        accept_drift: value(args, "--accept-drift")?.map(str::to_owned),
        dry_run: args.iter().any(|a| a == "--plan" || a == "--dry-run"),
        accept_archive_size: args.iter().any(|a| a == "--accept-archive-size"),
        accept_failover: value(args, "--accept-failover")?.map(str::to_owned),
        resume_forward: args.iter().any(|a| a == "--resume-forward"),
        up_to: target.map(str::to_owned),
        stop: Some(stop.clone()),
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
    // A plan applies nothing: it prints the SQL only.
    for id in report.applied.iter().filter(|_| !opts.dry_run) {
        writeln!(w, "Applied {id}").map_err(io)?;
    }
    if let Some(why) = &report.stopped {
        writeln!(w, "{}: {why}", super::run::STOPPED).map_err(io)?;
        if stop.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(MigrateError::Validation(format!(
                "{}: a shutdown signal stopped the run",
                super::run::STOPPED
            )));
        }
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
    let p = crate::cli_args::parse(args, &DOWN)?;
    let back = Back::of(&p)?;
    let db = database(args)?;
    let pool = db.as_ref().map_or(pool, |(_, p)| p);
    let opts = super::run::Options {
        database: db.as_ref().map(|(d, _)| d.clone()),
        ..back.opts(super::run::Options::default())
    };
    let report = back.run(pool, dir, &opts).await?;
    write_back(w, None, &report, back.dry_run())
}

/// How far `down` goes and with which flags; the tenancy CLI parses its
/// own flags around the same.
pub(crate) struct Back<'a> {
    to: Option<&'a str>,
    steps: usize,
    dry_run: bool,
    accept_data_loss: bool,
    cascade: bool,
}

impl<'a> Back<'a> {
    pub(crate) fn of(p: &'a crate::cli_args::Parsed) -> Result<Self, MigrateError> {
        let to = p.value("--to")?;
        let steps = match (p.positional(0), to) {
            (Some(n), Some(_)) => return Err(usage(format!("`{n}` and `--to`: pick one"))),
            (Some(n), None) => n
                .parse()
                .map_err(|_| usage(format!("`{n}` is not a number of steps")))?,
            (None, _) => 1,
        };
        Ok(Self {
            to,
            steps,
            dry_run: p.has("--plan") || p.has("--dry-run"),
            accept_data_loss: p.has("--accept-data-loss"),
            cascade: p.has("--cascade"),
        })
    }

    /// `base` with these flags.
    pub(crate) fn opts(&self, base: super::run::Options) -> super::run::Options {
        super::run::Options {
            dry_run: self.dry_run,
            accept_data_loss: self.accept_data_loss,
            cascade: self.cascade,
            ..base
        }
    }

    pub(crate) fn dry_run(&self) -> bool {
        self.dry_run
    }

    /// `--to`'s id; `None` for a count of steps.
    #[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
    pub(crate) fn to(&self) -> Option<&'a str> {
        self.to
    }

    /// The count of steps, when no `--to` is given.
    #[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
    pub(crate) fn steps(&self) -> usize {
        self.steps
    }

    /// The rollback on `pool`, with `opts` from [`Self::opts`].
    pub(crate) async fn run(
        &self,
        pool: &Pool,
        dir: &Path,
        opts: &super::run::Options,
    ) -> Result<super::run::Report, MigrateError> {
        self.run_steps(pool, dir, opts, self.steps).await
    }

    /// [`Self::run`] with `steps` in place of the parsed count.
    pub(crate) async fn run_steps(
        &self,
        pool: &Pool,
        dir: &Path,
        opts: &super::run::Options,
        steps: usize,
    ) -> Result<super::run::Report, MigrateError> {
        match self.to {
            Some(to) => super::run::downgrade_to(pool, dir, to, opts).await,
            None => super::run::downgrade(pool, dir, steps, opts).await,
        }
        .map_err(run_err)
    }
}

async fn status<W: Write>(
    pool: &Pool,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let project = super::load::load_project(dir).map_err(load)?;
    let db = database(args)?;
    let pool = db.as_ref().map_or(pool, |(_, p)| p);
    let opts = super::run::Options {
        database: db.as_ref().map(|(d, _)| d.clone()),
        ..super::run::Options::default()
    };
    for line in status_lines(pool, &project, &opts).await? {
        writeln!(w, "{line}").map_err(io)?;
    }
    Ok(())
}

/// One `[X] app:id` line per migration a run with `opts` on `pool` covers.
///
/// # Errors
/// Reading the ledger.
pub(crate) async fn status_lines(
    pool: &Pool,
    project: &super::load::Project,
    opts: &super::run::Options,
) -> Result<Vec<String>, MigrateError> {
    use super::ledger::RowState;
    let rows = super::ledger::Ledger::of(opts).rows(pool).await?;
    let mut out = Vec::new();
    for &chain in opts.chains() {
        for i in super::load::chain(project, chain, opts.database.as_deref()) {
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
            out.push(format!("[{mark}] {}:{}{note}", f.app, f.id));
        }
    }
    Ok(out)
}

async fn check<W: Write>(
    pool: &Pool,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    if args.iter().any(|a| a == "--drift") {
        return drift(pool, dir, args, w).await;
    }
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

/// What `check --drift` says for a database with no applied row: it
/// compared nothing, so it never says "No drift".
pub(crate) const NOTHING_APPLIED: &str = "No migration is applied here: nothing to compare.";

/// `check --drift [--target registry]`: the catalog against the state at
/// this database's applied set (S6); any difference fails.
async fn drift<W: Write>(
    pool: &Pool,
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let target = match value(args, "--target")? {
        Some("registry") => Target::Registry,
        None | Some("tenant") => Target::Tenant,
        Some(other) => return Err(usage(format!("--target {other}: `registry` or `tenant`"))),
    };
    // Files that do not load are refused, as on every other verb.
    super::load::load_project(dir).map_err(load)?;
    let Some(found) = drift_lines(pool, target).await? else {
        writeln!(w, "{NOTHING_APPLIED}").map_err(io)?;
        return Ok(());
    };
    for m in &found {
        writeln!(w, "{m}").map_err(io)?;
    }
    if !found.is_empty() {
        return Err(MigrateError::Validation(format!(
            "dbmigrate check --drift: {} difference(s) from the migrations' state",
            found.len()
        )));
    }
    writeln!(w, "No drift.").map_err(io)
}

/// Each difference between `pool`'s catalog and `target`'s state after the
/// rows its ledger applied, replayed from their stored bodies with the
/// squash marker rule (S6, item 28); `None` when no row of `target` is
/// applied, so nothing was compared.
///
/// # Errors
/// A stored body that does not replay, or reading the ledger or catalog.
pub(crate) async fn drift_lines(
    pool: &Pool,
    target: Target,
) -> Result<Option<Vec<String>>, MigrateError> {
    let rows = super::ledger::Ledger::default_names().rows(pool).await?;
    let applied: Vec<&super::ledger::Row> = rows
        .iter()
        .filter(|r| r.state == super::ledger::RowState::Applied)
        .collect();
    let mut hist = super::history::History::read(applied).map_err(load)?;
    let mine: Vec<usize> = (0..hist.len())
        .filter(|&p| hist.bodies[p].target == target)
        .collect();
    if mine.is_empty() {
        return Ok(None);
    }
    hist.keep(&mine);
    let ctx = super::replay::Ctx {
        target,
        registry: None,
    };
    let state = hist.state_before(hist.len(), &ctx).map_err(load)?;
    let found = super::probe::drift(pool, &state)
        .await
        .map_err(|e| MigrateError::Validation(e.to_string()))?;
    Ok(Some(found.iter().map(ToString::to_string).collect()))
}

/// `sql <id> [--dialect pg|mysql|sqlite|all]`: by default the backend of
/// each database it runs on (under tenancy the registry's and each tenant's).
fn sql<W: Write>(
    defaults: &[Backend],
    dir: &Path,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let target = positional(args, &["--dialect"])
        .ok_or_else(|| usage("`sql` needs a migration id (`<id>` or `<app>:<id>`)"))?;
    let list = match value(args, "--dialect")? {
        Some("all") => vec![Backend::Postgres, Backend::Mysql, Backend::Sqlite],
        Some(d) => vec![backend_named(d)?],
        None => defaults.to_vec(),
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
        "Next: commit these files, then run `manage dbmigrate apply` on each database \
         (under tenancy it migrates the registry, then every tenant).\n\
         The switch is one way: v2 rollback stops at the adopt checkpoint. A database \
         behind the engine v1 head must first run `manage migrate` from the commit \
         before this one.\n\
         The `system/migrations` folder and the v1 ledger stay as they are; engine v2 \
         does not read them."
    )
    .map_err(io)
}

#[cfg(all(test, feature = "sqlite"))]
#[path = "cli_tests.rs"]
mod tests;
