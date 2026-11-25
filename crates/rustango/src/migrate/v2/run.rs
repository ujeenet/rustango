//! The v2 runner: migrate and downgrade with units, fenced on a run id,
//! resumable from the frozen unit plan (6.2, 6.3, 6.5, 7.1, 9.2).
//!
//! Single database only: a project with registry files (tenancy) is refused
//! until phase 5. No batching yet (phase 3).

use std::collections::HashMap;
use std::path::Path;

use super::format::{File, Kind, Op, OpEnvelope};
use super::invert::{invert, Allow};
use super::ledger::{Ledger, Row, RowState};
use super::load::{load_project, Loaded, Project};
use super::probe::{drift, probe, Verdict};
use super::render::{render_file, Unit, UnitKind};
use super::replay::{apply, apply_file, Ctx};
use super::state::{Backend, HashMemo, State, Target};
use super::{Code, LoadError};
use crate::migrate::MigrateError;
use crate::sql::{raw_execute_pool, raw_execute_tx, Pool};

/// Proof the caller is the v2 runner: only this module builds one, so the
/// v1 system chain skips its switched-DB refusal for v2 alone.
#[derive(Clone, Copy)]
pub(crate) struct V2Token(());

/// Run options.
#[derive(Debug, Default, Clone)]
pub(crate) struct Options {
    /// Record an adopt checkpoint over a catalog that differs from it, with the reason.
    pub(crate) accept_drift: Option<String>,
    /// Let a rollback re-create dropped tables and columns empty.
    pub(crate) accept_data_loss: bool,
    /// Plan only: list the SQL each pending migration would run.
    pub(crate) dry_run: bool,
    /// Set by a shutdown signal: the run stops after the unit in flight and
    /// the next start resumes (6.11).
    pub(crate) stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// The engine v1 ledger name whose v2 ledger this app uses; `None` = default.
    pub(crate) ledger: Option<String>,
    /// A data unit waits this long for a row or table lock; `None` = 5 s.
    pub(crate) data_lock_timeout: Option<std::time::Duration>,
    /// `--accept-step-change <id> --reason <why>`: resume a partial or
    /// failed data step whose code changed, recording the new hash.
    pub(crate) accept_step_change: Option<(String, String)>,
    /// A data unit's statement runs at most this long; `None` = 5 min.
    pub(crate) data_statement_timeout: Option<std::time::Duration>,
    /// Test failpoint: stop as if the process died.
    #[cfg(test)]
    pub(crate) crash: Option<Crash>,
}

/// Where a test run stops as if killed, counted in units this run ran.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Crash {
    BeforeUnit(usize),
    /// After a data unit's statement, before its commit.
    AfterData(usize),
    /// After an autocommit unit's started mark, before its statement.
    AfterStart(usize),
    AfterStatement(usize),
}

/// What a run did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Report {
    /// With `dry_run`: per migration, the SQL it would run.
    pub(crate) planned: Vec<String>,
    pub(crate) system: Vec<String>,
    pub(crate) applied: Vec<String>,
    pub(crate) rolled_back: Vec<String>,
    /// Data steps whose code changed after they ran.
    pub(crate) warnings: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RunError {
    #[error(transparent)]
    Load(#[from] LoadError),
    #[error(transparent)]
    Db(#[from] MigrateError),
}

impl From<crate::sql::ExecError> for RunError {
    fn from(e: crate::sql::ExecError) -> Self {
        Self::Db(e.into())
    }
}

impl From<super::catalog::CatalogError> for RunError {
    fn from(e: super::catalog::CatalogError) -> Self {
        Self::Db(MigrateError::Validation(e.to_string()))
    }
}

impl From<crate::sql::sqlx::Error> for RunError {
    fn from(e: crate::sql::sqlx::Error) -> Self {
        Self::Db(e.into())
    }
}

impl From<RunError> for MigrateError {
    fn from(e: RunError) -> Self {
        match e {
            RunError::Load(e) => Self::Validation(e.to_string()),
            RunError::Db(e) => e,
        }
    }
}

impl RunError {
    pub(crate) fn code(&self) -> Option<Code> {
        match self {
            Self::Load(e) => Some(e.code),
            Self::Db(_) => None,
        }
    }
}

fn refused(code: Code, at: &str, msg: impl Into<String>) -> RunError {
    RunError::Load(LoadError::new(code, at, msg))
}

pub(crate) fn backend_of(pool: &Pool) -> Backend {
    Backend::of(pool.dialect().kind())
}

/// A fencing token unique to this run: time, process and a counter, hashed.
fn new_run_id() -> String {
    use sha2::Digest as _;
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let seed = format!("{nanos}:{}:{n}", std::process::id());
    super::checksum::hex(&sha2::Sha256::digest(seed.as_bytes()))[..32].to_owned()
}

const CTX: Ctx<'static> = Ctx {
    target: Target::Tenant,
    registry: None,
};

/// Tenancy projects run on v2 from phase 5.
fn refuse_registry(project: &Project) -> Result<(), RunError> {
    if let Some(f) = project
        .files
        .iter()
        .find(|f| f.file.target == Target::Registry)
    {
        return Err(refused(
            Code::NotEnabled,
            &f.at,
            "registry migrations (tenancy) run on engine v2 from a later release",
        ));
    }
    Ok(())
}

/// The lock, the v1 system chain around `body` (it sees `project` through
/// `ChainFacts`), then the framework's bootstrap tables. One lock for all
/// of it. Without `write` (a dry run) only `body` runs.
async fn locked<F, Fut, R>(
    pool: &Pool,
    dir: Option<&Path>,
    project: &Project,
    ledger: &Ledger,
    write: bool,
    body: F,
) -> Result<(R, Vec<String>), RunError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<R, RunError>>,
{
    use crate::core::ModelScope;
    let chain = match dir {
        Some(dir) => Some(crate::migrate::make::SystemChain::for_migrations_dir(
            dir,
            &[ModelScope::Registry, ModelScope::Tenant],
        )?),
        None => None,
    };
    crate::migrate::with_migrate_lock_held_v2(V2Token(()), pool, |held| async move {
        let Some(chain) = chain.filter(|_| write) else {
            return Ok(body().await.map(|r| (r, Vec::new())));
        };
        let applied: std::collections::HashSet<String> = ledger
            .rows(pool)
            .await?
            .into_iter()
            .map(|r| format!("{}:{}", r.app, r.id))
            .collect();
        let result = crate::migrate::chains::system_chain_for_v2(
            held,
            pool,
            &chain,
            crate::migrate::MigrationScope::Tenant,
            &project.files,
            &applied,
            body,
        )
        .await;
        if result.is_ok() {
            crate::migrate::manage::ensure_bootstrap_tables(pool).await?;
            crate::migrate::manage::ensure_tenant_bootstrap_tables(pool).await?;
        }
        Ok(result.map(|(system, r)| (r, system.into_iter().map(|m| m.name).collect())))
    })
    .await?
}

/// Apply every pending migration of the engine v2 project around `dir`,
/// resuming a crashed one from its frozen plan.
///
/// # Errors
/// A load error, a refusal (edited applied file, drift, behind the v1
/// head, unowned tables), a failed unit, or a lost lock.
pub(crate) async fn migrate(pool: &Pool, dir: &Path, opts: &Options) -> Result<Report, RunError> {
    let project = load_project(dir)?;
    migrate_project(pool, Some(dir), &project, opts).await
}

/// [`migrate`] for `embed_migrations!` entries: no system chain, as engine
/// v1's embedded runner.
///
/// # Errors
/// As [`migrate`], and an engine v1 entry.
pub(crate) async fn migrate_embedded(
    pool: &Pool,
    entries: &[(&str, &str)],
    opts: &Options,
) -> Result<Report, RunError> {
    let project = super::load::load_embedded(entries)?;
    migrate_project(pool, None, &project, opts).await
}

async fn migrate_project(
    pool: &Pool,
    dir: Option<&Path>,
    project: &Project,
    opts: &Options,
) -> Result<Report, RunError> {
    refuse_registry(project)?;
    super::step::refuse_duplicates().map_err(|e| refused(Code::Unresolved, "data steps", e))?;
    let ledger = Ledger::of(opts);
    let b = backend_of(pool);
    // Every data step the plan needs resolves before the lock and the
    // system chain, so a miss runs zero statements (5.2). A crashed row
    // resumes, so its steps count too.
    let rows = ledger.rows(pool).await?;
    let done: std::collections::HashSet<(String, String)> = rows
        .iter()
        .filter(|r| r.state == RowState::Applied)
        .map(|r| (r.app.clone(), r.id.clone()))
        .collect();
    let misses: Vec<String> = project
        .files
        .iter()
        .filter(|f| !done.contains(&(f.file.app.clone(), f.file.id.clone())))
        .flat_map(|f| super::data::misses(&f.file, b, false))
        .collect();
    super::data::refuse_misses(&misses, "plan")?;
    let warnings = step_changes(pool, &ledger, project, &rows, opts).await?;
    let leaked_before = super::step::leaked_bytes();
    let sweep = sweep_targets(project)?;
    let run_id = new_run_id();
    // A dry run writes nothing: no system chain, no ledger table.
    let ((applied, planned), system) =
        locked(pool, dir, project, &ledger, !opts.dry_run, || async {
            if !opts.dry_run {
                ledger.ensure(pool).await?;
            }
            let mut runner = Runner {
                pool,
                ledger: &ledger,
                run_id: &run_id,
                b: backend_of(pool),
                opts,
                units_run: 0,
                planned: Vec::new(),
                sweep: Some(sweep),
            };
            let applied = runner.apply_project(project).await?;
            Ok((applied, runner.planned))
        })
        .await?;
    let leaked = super::step::leaked_bytes() - leaked_before;
    if leaked > 0 {
        tracing::info!(
            target: "rustango::migrate",
            leaked_bytes = leaked,
            total_bytes = super::step::leaked_bytes(),
            "historical tables interned this run"
        );
    }
    Ok(Report {
        planned,
        system,
        applied,
        rolled_back: Vec::new(),
        warnings,
    })
}

/// Roll back the last `steps` applied migrations, newest first, from their
/// stored bodies. Stops at the adopt checkpoint.
///
/// # Errors
/// A blocker (dropped data, missing `down`, the switch point), a failed
/// unit, or a lost lock.
pub(crate) async fn downgrade(
    pool: &Pool,
    dir: &Path,
    steps: usize,
    opts: &Options,
) -> Result<Report, RunError> {
    rollback(pool, dir, Back::Steps(steps), opts).await
}

/// Roll back every migration the ledger applied after `to` (`<app>:<id>`
/// or `<id>`), newest first: ledger order, never file order.
///
/// # Errors
/// As [`downgrade`], and a `to` the ledger has not applied.
pub(crate) async fn downgrade_to(
    pool: &Pool,
    dir: &Path,
    to: &str,
    opts: &Options,
) -> Result<Report, RunError> {
    rollback(pool, dir, Back::To(to), opts).await
}

/// How far a rollback goes.
#[derive(Clone, Copy)]
enum Back<'a> {
    Steps(usize),
    To(&'a str),
}

async fn rollback(
    pool: &Pool,
    dir: &Path,
    back: Back<'_>,
    opts: &Options,
) -> Result<Report, RunError> {
    let project = load_project(dir)?;
    let ledger = Ledger::of(opts);
    let run_id = new_run_id();
    let (rolled_back, system) = locked(pool, Some(dir), &project, &ledger, true, || async {
        ledger.ensure(pool).await?;
        let mut runner = Runner {
            pool,
            ledger: &ledger,
            run_id: &run_id,
            b: backend_of(pool),
            opts,
            units_run: 0,
            planned: Vec::new(),
            sweep: None,
        };
        runner.downgrade(back).await
    })
    .await?;
    Ok(Report {
        planned: Vec::new(),
        system,
        applied: Vec::new(),
        rolled_back,
        warnings: Vec::new(),
    })
}

/// Data steps whose code hash changed since the ledger recorded it: a
/// warning for an applied migration; for a partial or failed one, a
/// refusal unless `--accept-step-change` names the step (5.2).
async fn step_changes(
    pool: &Pool,
    ledger: &Ledger,
    project: &Project,
    rows: &[Row],
    opts: &Options,
) -> Result<Vec<String>, RunError> {
    let recorded = ledger.step_hashes(pool).await?;
    let mut warnings = Vec::new();
    let mut accepted: Vec<(String, String, String, String, String)> = Vec::new();
    for f in &project.files {
        let Some(row) = rows
            .iter()
            .find(|r| r.app == f.file.app && r.id == f.file.id)
        else {
            continue;
        };
        for env in &f.file.operations {
            let Op::DataRust { id, .. } = &env.op else {
                continue;
            };
            let key = (f.file.app.clone(), id.clone());
            let (Some(was), Some(now)) = (
                recorded.get(&key),
                super::step::registered(&f.file.app, id)
                    .first()
                    .map(|s| s.code_hash()),
            ) else {
                continue;
            };
            if *was == now {
                continue;
            }
            let what = format!(
                "data step `{}:{id}` changed since it ran in `{}`",
                f.file.app, f.file.id
            );
            if row.state == RowState::Applied {
                tracing::warn!(target: "rustango::migrate", "{what}");
                warnings.push(what);
                continue;
            }
            match &opts.accept_step_change {
                Some((want, reason)) if want == id => accepted.push((
                    f.file.app.clone(),
                    f.file.id.clone(),
                    row.run_id.clone(),
                    format!("{id} {now}"),
                    reason.clone(),
                )),
                _ => {
                    return Err(refused(
                        Code::StepChanged,
                        &f.at,
                        format!(
                            "{what} and did not finish: check the change, then pass \
                             `--accept-step-change {id} --reason <why>`"
                        ),
                    ))
                }
            }
        }
    }
    // Only a partial or failed step's change is accepted; an applied one
    // only warns.
    if let Some((want, _)) = &opts.accept_step_change {
        if accepted.is_empty() {
            return Err(refused(
                Code::StepChanged,
                want,
                format!(
                    "`--accept-step-change {want}`: no partial or failed migration runs a changed \
                     `{want}`; an applied one only warns"
                ),
            ));
        }
    }
    if !opts.dry_run {
        for (app, mig, run_id, detail, reason) in &accepted {
            let k = (app.as_str(), mig.as_str());
            let why = format!("{detail} {reason}");
            ledger
                .event(pool, "step_change_accepted", k, None, run_id, Some(&why))
                .await?;
            ledger
                .event(pool, STEP_HASH, k, None, run_id, Some(detail))
                .await?;
        }
    }
    Ok(warnings)
}

/// The ledger event that records a data step's code hash: `"<id> <hash>"`.
pub(crate) const STEP_HASH: &str = "step_hash";

/// A `data_sql` unit as a step: its one statement.
fn sql_step<'t>(
    sql: String,
    binds: Vec<crate::core::SqlValue>,
) -> impl for<'a> FnOnce(
    &'a mut super::step::StepCtx<'t>,
) -> futures_util::future::BoxFuture<'a, Result<(), super::step::StepError>> {
    move |ctx| Box::pin(async move { Ok(ctx.execute(&sql, binds).await.map(drop)?) })
}

/// A `data_rust` unit: the registered `up` or `down`, unbatched.
fn rust_step<'t>(
    f: super::step::StepFn,
) -> impl for<'a> FnOnce(
    &'a mut super::step::StepCtx<'t>,
) -> futures_util::future::BoxFuture<'a, Result<(), super::step::StepError>> {
    move |ctx| Box::pin(async move { f(ctx, super::step::Batch::whole()).await.map(drop) })
}

/// Session timeouts of the runner's data connection (5.4); PG sets its own
/// per transaction.
async fn session_timeouts(
    c: &mut crate::sql::PoolConn,
    b: Backend,
    lock: std::time::Duration,
    stmt: std::time::Duration,
) -> Result<(), RunError> {
    let sql = match b {
        Backend::Postgres => return Ok(()),
        Backend::Mysql => format!(
            "SET SESSION innodb_lock_wait_timeout = {}, SESSION lock_wait_timeout = {}, \
             SESSION max_execution_time = {}",
            lock.as_secs().max(1),
            lock.as_secs().max(1),
            stmt.as_millis()
        ),
        Backend::Sqlite => format!("PRAGMA busy_timeout = {}", lock.as_millis()),
    };
    let mut tx = c.begin().await?;
    tx.execute_unprepared(&sql)
        .await
        .map_err(MigrateError::Driver)?;
    tx.commit().await.map_err(MigrateError::Driver)?;
    Ok(())
}

/// The SQLite datetime sweep's columns for a v2 run: every datetime column
/// in the replayed state, and the ledgers (5.5, 5.7).
fn sweep_targets(project: &Project) -> Result<Vec<(String, String)>, RunError> {
    let mut out: Vec<(String, String)> = crate::migrate::runner::LEDGER_TABLES
        .iter()
        .filter_map(|t| t.legacy_datetime.map(|c| (t.name.to_owned(), c.to_owned())))
        .collect();
    if let Some(state) = super::load::replay(project)?.states.get(&Target::Tenant) {
        for t in state.tables.values() {
            for c in t.columns.iter().filter(|c| c.ty == "datetime") {
                out.push((t.name.clone(), c.column.clone()));
            }
        }
    }
    Ok(out)
}

struct Runner<'a> {
    pool: &'a Pool,
    ledger: &'a Ledger,
    run_id: &'a str,
    b: Backend,
    opts: &'a Options,
    /// Units run so far in this run, for the test failpoints.
    units_run: usize,
    /// What a dry run would execute.
    planned: Vec<String>,
    /// SQLite datetime columns to sweep before the first data unit; taken then.
    sweep: Option<Vec<(String, String)>>,
}

fn renamed_name(r: &super::probe::Renamed) -> &str {
    match r {
        super::probe::Renamed::Index(i) => &i.name,
        super::probe::Renamed::Check(c) => &c.name,
        super::probe::Renamed::Fk(f) => &f.name,
    }
}

fn key(f: &File) -> (&str, &str) {
    (&f.app, &f.id)
}

impl Runner<'_> {
    fn stopping(&self) -> bool {
        self.opts
            .stop
            .as_ref()
            .is_some_and(|s| s.load(std::sync::atomic::Ordering::SeqCst))
    }

    async fn apply_project(&mut self, project: &Project) -> Result<Vec<String>, RunError> {
        let rows: HashMap<(String, String), Row> = self
            .ledger
            .rows(self.pool)
            .await?
            .into_iter()
            .map(|r| ((r.app.clone(), r.id.clone()), r))
            .collect();
        let mut state = State::default();
        let mut memo = HashMemo::default();
        let mut applied = Vec::new();
        let order = project
            .order
            .get(&Target::Tenant)
            .cloned()
            .unwrap_or_default();
        for i in order {
            let f = &project.files[i];
            let k = (f.file.app.clone(), f.file.id.clone());
            match rows.get(&k) {
                Some(row) => {
                    if row.checksum != f.checksum {
                        return Err(refused(
                            Code::Edited,
                            &f.at,
                            format!(
                                "`{}:{}` changed after it ran (checksum {} in the ledger, {} on \
                                 disk); put the file back, or roll it back first",
                                f.file.app, f.file.id, row.checksum, f.checksum
                            ),
                        ));
                    }
                    match row.state {
                        RowState::Applied => {}
                        RowState::RollingBack => {
                            return Err(refused(
                                Code::State,
                                &f.at,
                                "this migration is half rolled back: finish `downgrade` first",
                            ))
                        }
                        RowState::Running | RowState::Failed if self.opts.dry_run => {
                            let from = usize::try_from(row.pos + 1).unwrap_or(0);
                            self.plan(&f.file.id, &row.unit_plan[from.min(row.unit_plan.len())..]);
                        }
                        RowState::Running | RowState::Failed => {
                            self.resume(f, row).await?;
                            applied.push(f.file.id.clone());
                        }
                    }
                    apply_file(&mut state, &f.file, &f.at, &CTX)?;
                }
                None if f.file.kind == Kind::Checkpoint => {
                    state = self.adopt(f, state, &mut memo).await?;
                    applied.push(f.file.id.clone());
                }
                None => {
                    state = self.forward(f, state, &mut memo).await?;
                    applied.push(f.file.id.clone());
                }
            }
        }
        Ok(applied)
    }

    /// Render, record `running` with the frozen plan, run its units.
    async fn forward(
        &mut self,
        f: &Loaded,
        state: State,
        memo: &mut HashMemo,
    ) -> Result<State, RunError> {
        let (units, after) = render_file(&state, &f.file, &f.at, &CTX, self.b)?;
        if self.opts.dry_run {
            self.plan(&f.file.id, &units);
            return Ok(after);
        }
        let hashes = (state.hash(memo), after.hash(memo));
        self.ledger
            .insert(
                self.pool,
                key(&f.file),
                &f.checksum,
                &f.body,
                RowState::Running,
                &units,
                self.run_id,
                (&hashes.0, Some(&hashes.1)),
                false,
            )
            .await?;
        self.finish(f, &units, 0).await?;
        Ok(after)
    }

    fn plan(&mut self, id: &str, units: &[Unit]) {
        self.planned.push(format!("-- {id}"));
        self.planned.extend(
            units
                .iter()
                .flat_map(|u| u.sql.iter().map(|s| format!("{s};"))),
        );
    }

    /// Run `units` from `from`, then mark the row applied.
    async fn finish(&mut self, f: &Loaded, units: &[Unit], from: usize) -> Result<(), RunError> {
        let k = key(&f.file);
        if let Err(e) = self.run_units(k, units, from).await {
            // Best effort: the row stays resumable; the class only, never driver text.
            let _ = self
                .ledger
                .set_state(
                    self.pool,
                    k,
                    self.run_id,
                    RowState::Failed,
                    Some(error_class(&e)),
                )
                .await;
            return Err(e);
        }
        self.ledger
            .set_state(self.pool, k, self.run_id, RowState::Applied, None)
            .await?;
        self.ledger
            .event(
                self.pool,
                "applied",
                k,
                Some(&f.checksum),
                self.run_id,
                None,
            )
            .await?;
        Ok(())
    }

    /// A crashed run's row: take it over, probe a started autocommit unit,
    /// then run the stored plan from the next unit. Never re-rendered.
    async fn resume(&mut self, f: &Loaded, row: &Row) -> Result<(), RunError> {
        let k = key(&f.file);
        if row.run_id != self.run_id {
            self.ledger
                .claim(self.pool, k, &row.run_id, self.run_id)
                .await?;
        }
        self.ledger
            .event(
                self.pool,
                "resumed",
                k,
                Some(&f.checksum),
                self.run_id,
                None,
            )
            .await?;
        let units = &row.unit_plan;
        let from = self.resume_point(k, row, &f.at).await?;
        self.finish(f, units, from).await
    }

    /// Where a crashed row goes on: after its last unit, or, for an
    /// autocommit unit cut off after it started, what its probe says.
    async fn resume_point(
        &mut self,
        k: (&str, &str),
        row: &Row,
        at: &str,
    ) -> Result<usize, RunError> {
        let units = &row.unit_plan;
        let from = usize::try_from(row.pos + 1).unwrap_or(0);
        let Some(started) = row.started_unit else {
            return Ok(from);
        };
        let unit = usize::try_from(started).unwrap_or(0);
        let expects = units
            .get(unit)
            .and_then(|u| u.probe.clone())
            .unwrap_or_default();
        match probe(self.pool, &expects).await? {
            Verdict::Present => {
                let (sql, binds) =
                    self.ledger
                        .advance(self.pool, k, self.run_id, started - 1, started);
                super::ledger::fenced(raw_execute_pool(self.pool, &sql, binds).await?)?;
                Ok(unit + 1)
            }
            Verdict::Absent => Ok(unit),
            Verdict::Partial(diff) if super::probe::rerun_repairs(&diff) => Ok(unit),
            Verdict::Partial(diff) => {
                let list: Vec<String> = diff.iter().map(ToString::to_string).collect();
                Err(refused(
                    Code::State,
                    at,
                    format!(
                        "unit {unit} was cut off and the catalog is half way: {}; the ledger is \
                         untouched",
                        list.join("; ")
                    ),
                ))
            }
        }
    }

    /// Run units `from..`, each committing with its fenced position update.
    async fn run_units(
        &mut self,
        k: (&str, &str),
        units: &[Unit],
        from: usize,
    ) -> Result<(), RunError> {
        for (n, u) in units.iter().enumerate().skip(from) {
            if self.units_run > 0 && self.stopping() {
                return Err(MigrateError::Validation(format!(
                    "stopped by a shutdown signal after {} unit(s); the next start resumes",
                    self.units_run
                ))
                .into());
            }
            #[cfg(test)]
            if self.opts.crash == Some(Crash::BeforeUnit(self.units_run)) {
                return Err(MigrateError::Validation("test crash".into()).into());
            }
            let n = i64::try_from(n).expect("unit index fits");
            match u.kind {
                UnitKind::Tx => self.tx_unit(k, u, n).await?,
                UnitKind::Rebuild => self.rebuild_unit(k, u, n).await?,
                UnitKind::Data => self.data_unit(k, u, n).await?,
                UnitKind::Autocommit => {
                    self.ledger.start_unit(self.pool, k, self.run_id, n).await?;
                    #[cfg(test)]
                    if self.opts.crash == Some(Crash::AfterStart(self.units_run)) {
                        return Err(MigrateError::Validation("test crash".into()).into());
                    }
                    for s in &u.sql {
                        raw_execute_pool(self.pool, s, Vec::new()).await?;
                    }
                    #[cfg(test)]
                    if self.opts.crash == Some(Crash::AfterStatement(self.units_run)) {
                        return Err(MigrateError::Validation("test crash".into()).into());
                    }
                    let (sql, binds) = self.ledger.advance(self.pool, k, self.run_id, n - 1, n);
                    super::ledger::fenced(raw_execute_pool(self.pool, &sql, binds).await?)?;
                }
            }
            self.units_run += 1;
        }
        Ok(())
    }

    /// A data step on the runner's own connection: timeouts set, the escape
    /// guard on, rolled back when poisoned, the ledger moved in the same tx.
    async fn data_unit(&mut self, k: (&str, &str), u: &Unit, n: i64) -> Result<(), RunError> {
        if let Some(targets) = self.sweep.take() {
            crate::migrate::sqlite_datetime::normalise_sqlite_datetimes_in(self.pool, targets)
                .await?;
        }
        let lock = self
            .opts
            .data_lock_timeout
            .unwrap_or(std::time::Duration::from_secs(5));
        let stmt = self
            .opts
            .data_statement_timeout
            .unwrap_or(std::time::Duration::from_secs(300));
        // The runner's own connection, closed after the unit: its session
        // settings never reach the app's pool, and an in-memory SQLite
        // pool of one is free again for the next unit.
        let mut conn = self.pool.acquire().await?;
        conn.close_on_drop();
        session_timeouts(&mut conn, self.b, lock, stmt).await?;
        let binds = super::data::bind_values(&u.binds)
            .map_err(|e| refused(Code::File, &format!("{}:{}", k.0, k.1), e))?;
        let (ledger_sql, ledger_binds) = self.ledger.advance(self.pool, k, self.run_id, n - 1, n);
        let mut tx = conn.begin_write().await?;
        if self.b == Backend::Postgres {
            tx.execute_unprepared(&format!(
                "SET LOCAL lock_timeout = {}; SET LOCAL statement_timeout = {}",
                lock.as_millis(),
                stmt.as_millis()
            ))
            .await
            .map_err(MigrateError::Driver)?;
        }
        let mut hash = None;
        let (mut tx, r) = match &u.step {
            None => {
                let sql = u.sql.join("\n");
                super::step::run_step(tx, super::step::Scope::default(), sql_step(sql, binds)).await
            }
            Some(sr) => {
                let found = super::step::registered(k.0, &sr.id);
                let f = found
                    .first()
                    .and_then(|s| if sr.reverse { s.down } else { Some(s.up) })
                    .ok_or_else(|| {
                        refused(
                            Code::Unresolved,
                            &format!("{}:{}", k.0, k.1),
                            format!("data step `{}:{}` is not registered here", k.0, sr.id),
                        )
                    })?;
                hash = found.first().map(|s| (sr.id.clone(), s.code_hash()));
                let scope = super::step::Scope {
                    app: k.0.to_owned(),
                    footprint: sr.footprint.clone(),
                    tables: sr.tables.clone(),
                };
                super::step::run_step(tx, scope, rust_step(f)).await
            }
        };
        #[cfg(test)]
        let r = match r {
            Ok(()) if self.opts.crash == Some(Crash::AfterData(self.units_run)) => Err(
                super::step::UnitError::Step(super::step::StepError::Message("test crash".into())),
            ),
            other => other,
        };
        if let Err(e) = r {
            let _ = tx.rollback().await;
            return Err(
                MigrateError::Validation(format!("data step in `{}:{}`: {e}", k.0, k.1)).into(),
            );
        }
        let moved = raw_execute_tx(&mut tx, &ledger_sql, ledger_binds).await?;
        if moved != 1 {
            let _ = tx.rollback().await;
            return Err(super::ledger::fenced(moved)
                .expect_err("not one row")
                .into());
        }
        tx.commit().await.map_err(MigrateError::Driver)?;
        if let Some((id, hash)) = hash {
            let detail = format!("{id} {hash}");
            self.ledger
                .event(self.pool, STEP_HASH, k, None, self.run_id, Some(&detail))
                .await?;
        }
        Ok(())
    }

    /// The statements and the fenced position update in one transaction.
    async fn tx_unit(&self, k: (&str, &str), u: &Unit, n: i64) -> Result<(), RunError> {
        let mut conn = self.pool.acquire().await?;
        let mut tx = conn.begin().await?;
        for s in &u.sql {
            raw_execute_tx(&mut tx, s, Vec::new()).await?;
        }
        let (sql, binds) = self.ledger.advance(self.pool, k, self.run_id, n - 1, n);
        let moved = raw_execute_tx(&mut tx, &sql, binds).await?;
        if moved != 1 {
            let _ = tx.rollback().await;
            return Err(super::ledger::fenced(moved)
                .expect_err("not one row")
                .into());
        }
        tx.commit().await?;
        Ok(())
    }

    /// A SQLite rebuild: FK enforcement off outside, the ledger update inside
    /// the rebuild transaction (6.1 rule 4).
    #[cfg(feature = "sqlite")]
    #[allow(irrefutable_let_patterns)] // Pool has one variant in SQLite-only builds.
    async fn rebuild_unit(&self, k: (&str, &str), u: &Unit, n: i64) -> Result<(), RunError> {
        let crate::sql::Pool::Sqlite(sq) = self.pool else {
            return Err(MigrateError::Validation("a rebuild unit is SQLite only".into()).into());
        };
        let mut conn = crate::migrate::rebuild::RebuildConn::acquire(sq, true).await?;
        let result: Result<(), RunError> = async {
            let mut tx = conn.begin().await?;
            for s in &u.sql {
                crate::sql::sqlx::query(s).execute(&mut *tx).await?;
            }
            let (sql, binds) = self.ledger.advance(self.pool, k, self.run_id, n - 1, n);
            let mut q = crate::sql::sqlx::query(&sql);
            for b in binds {
                q = match b {
                    crate::core::SqlValue::I64(v) => q.bind(v),
                    crate::core::SqlValue::String(v) => q.bind(v),
                    other => unreachable!("the ledger binds text and integers: {other:?}"),
                };
            }
            let moved = q.execute(&mut *tx).await?.rows_affected();
            super::ledger::fenced(moved)?;
            tx.commit().await?;
            Ok(())
        }
        .await;
        let finished = conn.finish().await;
        result?;
        finished?;
        Ok(())
    }

    #[cfg(not(feature = "sqlite"))]
    async fn rebuild_unit(&self, _k: (&str, &str), _u: &Unit, _n: i64) -> Result<(), RunError> {
        Err(MigrateError::Validation("a rebuild unit is SQLite only".into()).into())
    }

    /// The per-database switch (9.2) for an adopt checkpoint.
    async fn adopt(
        &mut self,
        f: &Loaded,
        state: State,
        memo: &mut HashMemo,
    ) -> Result<State, RunError> {
        let base = f.file.base.as_ref().ok_or_else(|| {
            refused(
                Code::Checkpoint,
                &f.at,
                "only adopt checkpoints are enabled",
            )
        })?;
        let mut after = state.clone();
        apply_file(&mut after, &f.file, &f.at, &CTX)?;
        let doc = f.file.state.clone().unwrap_or_default();
        let checkpoint = State::from_doc(doc.clone());
        let v1 = self
            .opts
            .ledger
            .as_deref()
            .unwrap_or(crate::migrate::LEDGER_TABLE);
        let legacy: std::collections::HashSet<String> =
            if crate::migrate::try_table_exists_here(self.pool, v1).await? {
                crate::migrate::applied_set_pool_with_ledger(self.pool, v1).await?
            } else {
                Default::default()
            };
        let k = key(&f.file);
        let hashes = (state.hash(memo), after.hash(memo));
        if legacy.contains(&base.legacy) {
            let diff = drift(self.pool, &checkpoint).await?;
            if !diff.is_empty() && self.opts.accept_drift.is_none() {
                let list: Vec<String> = diff.iter().map(ToString::to_string).collect();
                return Err(refused(
                    Code::Drift,
                    &f.at,
                    format!(
                        "the database differs from the checkpoint: {}; fix it, or pass \
                         --accept-drift <reason>",
                        list.join("; ")
                    ),
                ));
            }
            // Names v1 left to the backend become the checkpoint's: the only
            // DDL the switch runs, one unit per object.
            let renames = super::probe::renames(self.pool, &checkpoint).await?;
            let units: Vec<Unit> = renames
                .iter()
                .filter_map(|r| super::render::rename_unit(self.b, r))
                .collect();
            if self.opts.dry_run {
                self.plan(&f.file.id, &units);
                return Ok(after);
            }
            self.ledger
                .insert(
                    self.pool,
                    k,
                    &f.checksum,
                    &f.body,
                    RowState::Running,
                    &units,
                    self.run_id,
                    (&hashes.0, Some(&hashes.1)),
                    true,
                )
                .await?;
            if !diff.is_empty() {
                let reason = self.opts.accept_drift.as_deref();
                self.ledger
                    .event(
                        self.pool,
                        "drift_accepted",
                        k,
                        Some(&f.checksum),
                        self.run_id,
                        reason,
                    )
                    .await?;
            }
            for r in &renames {
                let what = format!("{} -> {}", r.old, renamed_name(&r.to));
                self.ledger
                    .event(
                        self.pool,
                        "renamed",
                        k,
                        Some(&f.checksum),
                        self.run_id,
                        Some(&what),
                    )
                    .await?;
            }
            self.ledger
                .event(
                    self.pool,
                    "adopted",
                    k,
                    Some(&f.checksum),
                    self.run_id,
                    None,
                )
                .await?;
            self.finish(f, &units, 0).await?;
            return Ok(after);
        }
        if !legacy.is_empty() {
            return Err(refused(
                Code::Behind,
                &f.at,
                format!(
                    "migrate this database with the pre-switch release (engine v1) to `{}` first",
                    base.legacy
                ),
            ));
        }
        for t in checkpoint.tables.keys() {
            if crate::migrate::try_table_exists_here(self.pool, t).await? {
                return Err(refused(
                    Code::Unowned,
                    &f.at,
                    format!("table `{t}` exists but no ledger says how it got there"),
                ));
            }
        }
        // Fresh: create the checkpoint's tables as it holds them, FK targets first.
        let mut externs = state.clone();
        externs.externs.extend(checkpoint.externs.clone());
        let order = fk_order(&checkpoint, &f.at)?;
        let units = super::render::render_install(&externs, &order, &f.at, self.b)?;
        if self.opts.dry_run {
            self.plan(&f.file.id, &units);
            return Ok(after);
        }
        self.ledger
            .insert(
                self.pool,
                k,
                &f.checksum,
                &f.body,
                RowState::Running,
                &units,
                self.run_id,
                (&hashes.0, Some(&hashes.1)),
                true,
            )
            .await?;
        self.finish(f, &units, 0).await?;
        Ok(after)
    }

    /// Plan every rollback first (each inverse and blocker), then run them,
    /// newest first: a blocker anywhere runs zero statements (7.1).
    async fn downgrade(&mut self, back: Back<'_>) -> Result<Vec<String>, RunError> {
        let rows = self.ledger.rows(self.pool).await?;
        if let Some(r) = rows
            .iter()
            .find(|r| matches!(r.state, RowState::Running | RowState::Failed))
        {
            return Err(refused(
                Code::State,
                &format!("ledger:{}:{}", r.app, r.id),
                format!(
                    "`{}:{}` did not finish: run `manage dbmigrate apply` to resume it before \
                     rolling back",
                    r.app, r.id
                ),
            ));
        }
        let applied: Vec<&Row> = rows
            .iter()
            .filter(|r| r.state == RowState::Applied)
            .collect();
        // Rows come in `seq` order: `--to` keeps the target and every row before it.
        let steps = match back {
            Back::Steps(n) => n,
            Back::To(to) => {
                let (app, id) = to.split_once(':').map_or((None, to), |(a, i)| (Some(a), i));
                let pos = applied
                    .iter()
                    .position(|r| r.id == id && app.is_none_or(|a| a == r.app))
                    .ok_or_else(|| {
                        refused(Code::State, to, format!("`{to}` is not applied here"))
                    })?;
                applied.len() - pos - 1
            }
        };
        let mut misses = Vec::new();
        for idx in (0..applied.len()).rev().take(steps) {
            let at = format!("ledger:{}:{}", applied[idx].app, applied[idx].id);
            misses.extend(super::data::misses(&body(applied[idx], &at)?, self.b, true));
        }
        super::data::refuse_misses(&misses, "rollback plan")?;
        let mut plan: Vec<(&Row, Vec<Unit>)> = Vec::new();
        for idx in (0..applied.len()).rev().take(steps) {
            let row = applied[idx];
            let at = format!("ledger:{}:{}", row.app, row.id);
            let file = body(row, &at)?;
            if let (Kind::Checkpoint, Some(base)) = (file.kind, &file.base) {
                return Err(refused(
                    Code::Floor,
                    &at,
                    format!(
                        "v2 rollback stops at the switch point (`{}:{}`, from v1 `{}`)",
                        row.app, row.id, base.legacy
                    ),
                ));
            }
            // State before it: every earlier stored body, in apply order.
            let mut state = State::default();
            for r in &applied[..idx] {
                let at = format!("ledger:{}:{}", r.app, r.id);
                apply_file(&mut state, &body(r, &at)?, &at, &CTX)?;
            }
            let down = self.down_file(&file, &state, &at)?;
            let mut after = state.clone();
            apply_file(&mut after, &file, &at, &CTX)?;
            let (units, _) = render_file(&after, &down, &at, &CTX, self.b)?;
            plan.push((row, units));
        }
        let mut done = Vec::new();
        // A half-finished rollback finishes first, from its frozen plan.
        for row in rows.iter().filter(|r| r.state == RowState::RollingBack) {
            self.finish_rollback(row, &row.unit_plan, None).await?;
            done.push(row.id.clone());
        }
        for (row, units) in plan {
            let k = (row.app.as_str(), row.id.as_str());
            self.ledger
                .begin_rollback(self.pool, k, &row.run_id, self.run_id, &units)
                .await?;
            let mine = Row {
                run_id: self.run_id.to_owned(),
                pos: -1,
                started_unit: None,
                ..row.clone()
            };
            self.finish_rollback(&mine, &units, Some(0)).await?;
            done.push(row.id.clone());
        }
        Ok(done)
    }

    /// Run a rollback's units from `from`, or, for a cut-off one, from where
    /// its probe says, then drop its row.
    async fn finish_rollback(
        &mut self,
        row: &Row,
        units: &[Unit],
        from: Option<usize>,
    ) -> Result<(), RunError> {
        let k = (row.app.as_str(), row.id.as_str());
        if row.run_id != self.run_id {
            self.ledger
                .claim(self.pool, k, &row.run_id, self.run_id)
                .await?;
        }
        let from = match from {
            Some(n) => n,
            None => {
                let at = format!("ledger:{}:{}", row.app, row.id);
                self.resume_point(k, row, &at).await?
            }
        };
        self.run_units(k, units, from).await?;
        self.ledger.remove(self.pool, k, self.run_id).await?;
        self.ledger
            .event(
                self.pool,
                "rolled_back",
                k,
                Some(&row.checksum),
                self.run_id,
                None,
            )
            .await?;
        Ok(())
    }

    /// The inverse of `file`'s ops, newest first, as a file to render.
    fn down_file(&self, file: &File, before: &State, at: &str) -> Result<File, RunError> {
        let allow = Allow {
            data_loss: self.opts.accept_data_loss,
        };
        let mut states = vec![before.clone()];
        for (i, env) in file.operations.iter().enumerate() {
            let mut next = states.last().expect("seeded").clone();
            apply(
                &mut next,
                &env.op,
                &format!("{at}#/operations/{i}"),
                &CTX,
                &[],
            )?;
            states.push(next);
        }
        let mut ops = Vec::new();
        for (i, env) in file.operations.iter().enumerate().rev() {
            let op_at = format!("{at}#/operations/{i}");
            for op in invert(&env.op, &states[i + 1], allow, &op_at)? {
                // A raw op keeps its unit kind on the way down.
                let tx = if matches!(op, Op::RawSql { .. }) {
                    env.tx
                } else {
                    None
                };
                ops.push(OpEnvelope {
                    op,
                    tx,
                    allow: Vec::new(),
                });
            }
        }
        Ok(File {
            operations: ops,
            ..file.clone()
        })
    }
}

/// The file a ledger row ran: its stored body, decoded.
fn body(row: &Row, at: &str) -> Result<File, RunError> {
    let raw: serde_json::Value = serde_json::from_str(&row.applied_body)
        .map_err(|e| refused(Code::File, at, e.to_string()))?;
    Ok(super::format::decode(&raw, at)?)
}

/// The class of an error, for the ledger: never driver text or row values.
fn error_class(e: &RunError) -> &'static str {
    match e {
        RunError::Load(l) => l.code.as_str(),
        RunError::Db(MigrateError::Driver(_) | MigrateError::Exec(_)) => "driver",
        RunError::Db(MigrateError::Validation(_)) => "refused",
        RunError::Db(_) => "other",
    }
}

/// The checkpoint's tables with every FK target before its source.
pub(crate) fn fk_order<'s>(
    state: &'s State,
    at: &str,
) -> Result<Vec<&'s super::state::TableState>, RunError> {
    let mut out: Vec<&super::state::TableState> = Vec::new();
    let mut left: Vec<&super::state::TableState> = state.tables.values().map(|t| &**t).collect();
    while !left.is_empty() {
        let before = left.len();
        let mut i = 0;
        while i < left.len() {
            let ready = left[i].fks.iter().all(|fk| {
                fk.to == left[i].name
                    || out.iter().any(|t| t.name == fk.to)
                    || state.externs.contains_key(fk.to.as_str())
            });
            if ready {
                out.push(left.remove(i));
            } else {
                i += 1;
            }
        }
        if left.len() == before {
            return Err(refused(
                Code::NotEnabled,
                at,
                "the checkpoint's FKs form a cycle; a fresh install of it is not enabled yet",
            ));
        }
    }
    Ok(out)
}

#[cfg(test)]
#[path = "run_live_tests.rs"]
mod live_tests;
