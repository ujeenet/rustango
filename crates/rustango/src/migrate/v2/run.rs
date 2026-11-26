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
use super::render::{render_plan, Unit, UnitKind};
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
    /// CLI `--max-duration`: stop cleanly between batches once it passes.
    pub(crate) max_duration: Option<std::time::Duration>,
    /// A CLI run honours `max_batches_per_run`; boot and embedded runs move
    /// every batch, so `/ready` never opens on a half-done move.
    pub(crate) per_run_limits: bool,
    /// Row estimate above which a batched op is a long move (`MV2-W011`).
    pub(crate) long_move_rows: Option<u64>,
    /// A boot waiter gives up when the lock holder's heartbeat is this old
    /// while it still holds the lock (6.11); `None` waits on.
    pub(crate) stale_after: Option<std::time::Duration>,
    /// `--accept-archive-size`: archive a drop above [`ARCHIVE_LIMIT`].
    pub(crate) accept_archive_size: bool,
    /// `--accept-failover <reason>`: a recorded MySQL DDL session ran on
    /// another server; resume probes without ending it, logged (14 item 16).
    pub(crate) accept_failover: Option<String>,
    /// `--resume-forward`: a half rolled back migration goes forward again
    /// from where the rollback stopped (7.6).
    pub(crate) resume_forward: bool,
    /// `down --cascade`: roll back dependents in other apps too (7.1).
    pub(crate) cascade: bool,
    /// `apply <target>`: stop after this migration (`<app>:<id>` or `<id>`).
    pub(crate) up_to: Option<String>,
    /// How often a run holding the lock refreshes its heartbeat; `None` = 30 s.
    pub(crate) heartbeat_every: Option<std::time::Duration>,
    /// Test failpoint: stop as if the process died.
    #[cfg(test)]
    pub(crate) crash: Option<Crash>,
}

/// Where a test run stops as if killed, counted in units this run ran.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Crash {
    BeforeUnit(usize),
    /// After a batch's statement, before its commit (counted in batches).
    MidBatch(usize),
    /// The batch committed and its answer was lost (counted in batches).
    LostAck(usize),
    /// An unbatched transactional unit committed and its answer was lost.
    LostUnitAck(usize),
    /// After a data unit's statement, before its commit.
    AfterData(usize),
    /// After an autocommit unit's started mark, before its statement.
    AfterStart(usize),
    /// An autocommit unit's statement left running on the server, as a
    /// killed client's DDL keeps running (6.3).
    DdlLeftRunning(usize),
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
    /// Data steps whose code changed after they ran, and long moves.
    pub(crate) warnings: Vec<String>,
    /// Why a clean stop ended the run early; the next run resumes.
    pub(crate) stopped: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RunError {
    #[error(transparent)]
    Load(#[from] LoadError),
    #[error(transparent)]
    Db(#[from] MigrateError),
    /// A planned stop (SIGTERM, `--max-duration`, a batch cap): the row is
    /// failed with class `stopped` and the next run resumes it.
    #[error("{}: {}", STOPPED, .0)]
    Stopped(String),
}

/// How status, boot and the CLI name a planned stop.
pub(crate) const STOPPED: &str = "stopped, resumes on the next run";

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
            RunError::Stopped(why) => Self::Validation(format!("{STOPPED}: {why}")),
        }
    }
}

impl RunError {
    pub(crate) fn code(&self) -> Option<Code> {
        match self {
            Self::Load(e) => Some(e.code),
            Self::Db(_) | Self::Stopped(_) => None,
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
    let got_lock = std::sync::atomic::AtomicBool::new(false);
    // A dry run writes nothing: no system chain, no ledger table.
    let run = locked(pool, dir, project, &ledger, !opts.dry_run, || async {
        got_lock.store(true, std::sync::atomic::Ordering::SeqCst);
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
            batches_run: 0,
            warnings: Vec::new(),
            started: std::time::Instant::now(),
            planned: Vec::new(),
            sweep: Some(sweep),
        };
        // A long unit keeps its heartbeat fresh, so boot waiters wait on it.
        let applying = runner.apply_project(project);
        let result = tokio::select! {
            r = applying => r,
            never = heartbeats(
                pool,
                &ledger,
                &run_id,
                opts.heartbeat_every.unwrap_or(HEARTBEAT_EVERY),
            ) => match never {},
        };
        let (applied, stopped) = match result {
            Ok(applied) => (applied, None),
            Err(RunError::Stopped(why)) => (Vec::new(), Some(why)),
            Err(e) => return Err(e),
        };
        Ok((applied, runner.planned, stopped, runner.warnings))
    });
    let ((applied, planned, stopped, long_moves), system) = match opts.stale_after {
        None => run.await?,
        Some(stale) => tokio::select! {
            r = run => r?,
            e = stale_holder(pool, &ledger, stale, &got_lock) => return Err(e),
        },
    };
    let mut warnings = warnings;
    warnings.extend(long_moves);
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
        stopped,
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

/// Whether `target` (`<app>:<id>` or `<id>`) names `(app, id)`.
fn names(target: &str, app: &str, id: &str) -> bool {
    match target.split_once(':') {
        Some((a, i)) => a == app && i == id,
        None => target == id,
    }
}

/// For an applied `target`, the rows applied after it (newest last);
/// `None` when it is not applied.
///
/// # Errors
/// Driver failures.
pub(crate) async fn applied_after(
    pool: &Pool,
    opts: &Options,
    target: &str,
) -> Result<Option<Vec<String>>, RunError> {
    let rows = Ledger::of(opts).rows(pool).await?;
    let Some(pos) = rows.iter().position(|r| names(target, &r.app, &r.id)) else {
        return Ok(None);
    };
    Ok(Some(
        rows[pos + 1..]
            .iter()
            .map(|r| format!("{}:{}", r.app, r.id))
            .collect(),
    ))
}

/// Whether a data op among `earlier` (with a real down) declares that it
/// restores what `drop` removes: `table.column` for a column, `table` for a
/// table.
fn restored(earlier: &[OpEnvelope], drop: &Op) -> bool {
    let target = match drop {
        Op::DropColumn { table, column, .. } => format!("{table}.{column}"),
        Op::DropTable { table, .. } => table.clone(),
        _ => return false,
    };
    earlier.iter().any(|env| match &env.op {
        Op::DataSql {
            restores,
            down: super::format::DataDown::Sql(_),
            ..
        }
        | Op::DataRust {
            restores,
            down: super::format::RustDown::Rust,
            ..
        } => restores.contains(&target),
        _ => false,
    })
}

/// The rows applied after `pos` that descend from it over `parents` (same
/// app) and `depends_on` (any app), in `seq` order (7.1 step 1).
fn descendants(rows: &[&Row], bodies: &[File], pos: usize) -> Vec<usize> {
    let mut set: Vec<(String, String)> = vec![(rows[pos].app.clone(), rows[pos].id.clone())];
    let mut out = Vec::new();
    for i in pos + 1..rows.len() {
        let f = &bodies[i];
        let hit = f
            .parents
            .iter()
            .any(|p| set.contains(&(rows[i].app.clone(), p.clone())))
            || f.depends_on.iter().any(|d| set.contains(d));
        if hit {
            set.push((rows[i].app.clone(), rows[i].id.clone()));
            out.push(i);
        }
    }
    out
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
            batches_run: 0,
            warnings: Vec::new(),
            started: std::time::Instant::now(),
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
        stopped: None,
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
) -> futures_util::future::BoxFuture<'a, Result<u64, super::step::StepError>> {
    move |ctx| Box::pin(async move { Ok(ctx.execute(&sql, binds).await?) })
}

/// A `data_rust` unit or batch: the registered `up` or `down`.
fn rust_step<'t>(
    f: super::step::StepFn,
    batch: super::step::Batch,
) -> impl for<'a> FnOnce(
    &'a mut super::step::StepCtx<'t>,
) -> futures_util::future::BoxFuture<
    'a,
    Result<super::step::Progress, super::step::StepError>,
> {
    move |ctx| Box::pin(async move { f(ctx, batch).await })
}

/// How often a data unit that hit a deadlock or lock timeout runs again.
const DATA_RETRIES: u32 = 5;

/// The event a rollback opens with; its reason is the count of forward ops
/// it undoes.
const ROLLBACK_BEGAN: &str = "rollback_began";

/// The largest archive a drop makes without `--accept-archive-size`.
const ARCHIVE_LIMIT: u64 = 256 << 20;

/// Captured changes an undo restore puts back per transaction.
const UNDO_BATCH: u32 = 500;

/// Jittered backoff: about 100 ms doubled per try, at most 5 s.
fn backoff(tries: u32) -> std::time::Duration {
    use std::hash::{BuildHasher as _, Hasher as _};
    let base = 100_u64.saturating_mul(1 << tries.min(6)).min(5_000);
    let rand = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    std::time::Duration::from_millis(base / 2 + rand % (base / 2 + 1))
}

/// One attempt of a data unit failed: run it again, or give up.
enum Attempt {
    Retry(String),
    Fatal(RunError),
}

fn retry_or_fatal(e: RunError) -> Attempt {
    let retry = match &e {
        RunError::Db(MigrateError::Driver(d)) => super::step::retryable_driver(d),
        RunError::Db(MigrateError::Exec(x)) => super::step::retryable(x),
        _ => false,
    };
    if retry {
        Attempt::Retry(e.to_string())
    } else {
        Attempt::Fatal(e)
    }
}

/// The key bound of the next batch: the key `size` rows above `lo` (on the
/// database, so a string key follows its collation), and whether rows lie
/// beyond it. No row there: this is the last batch.
async fn keyset(
    tx: &mut crate::sql::PoolTx<'_>,
    bp: &super::render::BatchPlan,
    lo: Option<&crate::core::SqlValue>,
) -> Result<(Option<crate::core::SqlValue>, bool), RunError> {
    let keys = batch_keys(
        tx,
        bp,
        lo.map(|v| (">", v)),
        false,
        2,
        bp.size.saturating_sub(1),
    )
    .await?;
    let mut keys = keys.into_iter();
    let hi = keys.next();
    let more = keys.next().is_some();
    Ok((hi.clone(), hi.is_none() || !more))
}

/// The highest key above `lo`: the cursor a last, open-ended batch leaves,
/// so a down walk starts above every row it covered.
async fn top_key(
    tx: &mut crate::sql::PoolTx<'_>,
    bp: &super::render::BatchPlan,
    lo: Option<&crate::core::SqlValue>,
) -> Result<Option<crate::core::SqlValue>, RunError> {
    let keys = batch_keys(tx, bp, lo.map(|v| (">", v)), true, 1, 0).await?;
    Ok(keys.into_iter().next().or_else(|| lo.cloned()))
}

/// A down batch below `hi`: its exclusive lower key, `None` when the batch
/// reaches the bottom (the last one).
async fn keyset_down(
    tx: &mut crate::sql::PoolTx<'_>,
    bp: &super::render::BatchPlan,
    hi: &crate::core::SqlValue,
) -> Result<Option<crate::core::SqlValue>, RunError> {
    let keys = batch_keys(tx, bp, Some(("<=", hi)), true, 1, bp.size).await?;
    Ok(keys.into_iter().next())
}

/// Keys of the batch table, ordered, filtered by `key <op> value`.
async fn batch_keys(
    tx: &mut crate::sql::PoolTx<'_>,
    bp: &super::render::BatchPlan,
    filter: Option<(&str, &crate::core::SqlValue)>,
    desc: bool,
    limit: u32,
    offset: u64,
) -> Result<Vec<crate::core::SqlValue>, RunError> {
    use crate::core::SqlValue;
    let d = tx.dialect();
    let k = d.quote_ident(&bp.key);
    let t = d.quote_ident(&bp.table);
    let kind = d.kind();
    let as_text = match (kind, bp.key_ty.as_str()) {
        (crate::sql::DialectKind::Postgres, "uuid") => format!("CAST({k} AS TEXT)"),
        (crate::sql::DialectKind::Postgres, "i16" | "i32") => format!("CAST({k} AS BIGINT)"),
        (crate::sql::DialectKind::Mysql, "uuid") => format!("CAST({k} AS CHAR)"),
        (crate::sql::DialectKind::Mysql, "i16" | "i32") => format!("CAST({k} AS SIGNED)"),
        _ => k.clone(),
    };
    let mut binds = Vec::new();
    let mut sql = format!("SELECT {as_text} FROM {t}");
    if let Some((op, v)) = filter {
        binds.push(v.clone());
        sql.push_str(&format!(" WHERE {k} {op} "));
        d.push_placeholder(&mut sql, 1);
    }
    let dir = if desc { " DESC" } else { "" };
    sql.push_str(&format!(" ORDER BY {k}{dir} LIMIT {limit} OFFSET {offset}"));
    if super::data::BATCH_KEY_TYPES[..3].contains(&bp.key_ty.as_str()) {
        let rows: Vec<(i64,)> = crate::sql::raw_query_tx(tx, &sql, binds).await?;
        Ok(rows.into_iter().map(|r| SqlValue::I64(r.0)).collect())
    } else {
        let rows: Vec<(String,)> = crate::sql::raw_query_tx(tx, &sql, binds).await?;
        rows.into_iter()
            .map(|r| cursor_value(&serde_json::Value::String(r.0).to_string(), &bp.key_ty))
            .collect::<Result<_, _>>()
            .map_err(|e| MigrateError::Validation(e).into())
    }
}

/// A cursor as stored in the progress row: JSON typed by the key.
fn cursor_json(v: &crate::core::SqlValue) -> Result<String, String> {
    use crate::core::SqlValue as V;
    let j = match v {
        V::I16(n) => serde_json::json!(n),
        V::I32(n) => serde_json::json!(n),
        V::I64(n) => serde_json::json!(n),
        V::String(s) => serde_json::json!(s),
        V::Uuid(u) => serde_json::json!(u.to_string()),
        other => return Err(format!("{other:?} cannot be a batch cursor")),
    };
    Ok(j.to_string())
}

/// A stored cursor back as the key's type.
fn cursor_value(json: &str, ty: &str) -> Result<crate::core::SqlValue, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let ty = match ty {
        "i16" | "i32" | "i64" => crate::core::FieldType::I64,
        other => crate::migrate::snapshot::field_type_from_name(other)
            .ok_or_else(|| format!("unknown key type `{other}`"))?,
    };
    crate::migrate::manage::fixtures::json_to_sql_value_of(&v, ty)
}

/// `next` is above `prev` for a key of `ty`. Strings compare as Rust
/// strings here; the database's collation decides the batches themselves.
fn advances(prev: &crate::core::SqlValue, next: &crate::core::SqlValue, ty: &str) -> bool {
    use crate::core::SqlValue as V;
    let int = |v: &V| match v {
        V::I16(n) => Some(i64::from(*n)),
        V::I32(n) => Some(i64::from(*n)),
        V::I64(n) => Some(*n),
        _ => None,
    };
    match (prev, next) {
        (V::String(a), V::String(b)) => b > a,
        (V::Uuid(a), V::Uuid(b)) => b > a,
        _ if ty.starts_with('i') => int(prev).zip(int(next)).is_some_and(|(a, b)| b > a),
        _ => false,
    }
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

/// Row estimate above which a batched op is a long move (`MV2-W011`).
pub(crate) const LONG_MOVE_ROWS: u64 = 1_000_000;

/// How often a run holding the lock refreshes its heartbeat.
const HEARTBEAT_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// Refresh this run's heartbeat every `every`; never returns.
async fn heartbeats(
    pool: &Pool,
    ledger: &Ledger,
    run_id: &str,
    every: std::time::Duration,
) -> std::convert::Infallible {
    loop {
        tokio::time::sleep(every).await;
        // Best effort: display and boot waiters only (6.2).
        let _ = ledger.touch(pool, run_id).await;
    }
}

/// Resolves with an error once a run holding the lock has a heartbeat older
/// than `stale`, while this one still waits for the lock; pends after that.
async fn stale_holder(
    pool: &Pool,
    ledger: &Ledger,
    stale: std::time::Duration,
    got_lock: &std::sync::atomic::AtomicBool,
) -> RunError {
    let every = (stale / 4).clamp(
        std::time::Duration::from_millis(100),
        std::time::Duration::from_secs(5),
    );
    loop {
        tokio::time::sleep(every).await;
        if got_lock.load(std::sync::atomic::Ordering::SeqCst) {
            return std::future::pending().await;
        }
        match ledger.stale_running(pool, stale).await {
            Ok(Some((app, id, age))) => {
                return RunError::Db(MigrateError::Validation(format!(
                    "the migrate lock is held by a run of `{app}:{id}` whose heartbeat is {age}s \
                     old: it looks hung; stopping instead of waiting on it"
                )))
            }
            Ok(None) => {}
            // The ledger may not exist yet; keep waiting.
            Err(_) => {}
        }
    }
}

/// What resume does with a crashed row (6.5), decided before it is taken.
enum ResumeAt {
    /// Run from this unit.
    Unit(usize),
    /// The probe found this cut-off unit done: mark it, run the next.
    Done(usize),
    /// Drop these invalid indexes, then run this unit again.
    Rebuild(usize, Vec<String>),
}

/// A cut-off unit whose probe is neither done nor repairable.
fn half_way(at: &str, unit: usize, diff: &[super::probe::Mismatch]) -> RunError {
    let list: Vec<String> = diff.iter().map(ToString::to_string).collect();
    refused(
        Code::State,
        at,
        format!(
            "unit {unit} was cut off and the catalog is half way: {}; the ledger is untouched",
            list.join("; ")
        ),
    )
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
    /// Batches committed in this run, for the test failpoints and stops.
    batches_run: usize,
    /// Long moves found while applying.
    warnings: Vec<String>,
    started: std::time::Instant,
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

    /// Rows this binary has no file for: a newer one migrated the database
    /// (7.8). With nothing pending it is ahead: warn and go on. With files
    /// pending, they would run on a schema this binary cannot replay.
    fn refuse_unknown_rows(
        &mut self,
        project: &Project,
        rows: &HashMap<(String, String), Row>,
    ) -> Result<(), RunError> {
        let known: std::collections::HashSet<(&str, &str)> = project
            .files
            .iter()
            .map(|f| (f.file.app.as_str(), f.file.id.as_str()))
            .collect();
        let mut unknown: Vec<String> = rows
            .keys()
            .filter(|(a, i)| !known.contains(&(a.as_str(), i.as_str())))
            .map(|(a, i)| format!("{a}:{i}"))
            .collect();
        if unknown.is_empty() {
            return Ok(());
        }
        unknown.sort();
        let pending: Vec<String> = project
            .files
            .iter()
            .filter(|f| !rows.contains_key(&(f.file.app.clone(), f.file.id.clone())))
            .map(|f| format!("{}:{}", f.file.app, f.file.id))
            .collect();
        if pending.is_empty() {
            self.warnings.push(format!(
                "the database is ahead of this binary: it holds {} it does not know",
                unknown.join(", ")
            ));
            return Ok(());
        }
        Err(refused(
            Code::State,
            "ledger",
            format!(
                "the database holds {} that this binary does not know, and {} pending here: roll \
                 back with the newer binary first",
                unknown.join(", "),
                pending.join(", ")
            ),
        ))
    }

    async fn apply_project(&mut self, project: &Project) -> Result<Vec<String>, RunError> {
        let rows: HashMap<(String, String), Row> = self
            .ledger
            .rows(self.pool)
            .await?
            .into_iter()
            .map(|r| ((r.app.clone(), r.id.clone()), r))
            .collect();
        self.refuse_unknown_rows(project, &rows)?;
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
                        RowState::RollingBack if self.opts.resume_forward => {
                            self.resume_forward(f, row, &state).await?;
                            applied.push(f.file.id.clone());
                        }
                        RowState::RollingBack => {
                            return Err(refused(
                                Code::State,
                                &f.at,
                                "this migration is half rolled back: finish `downgrade` first, \
                                 or `apply --resume-forward` to go forward again",
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
            if self
                .opts
                .up_to
                .as_deref()
                .is_some_and(|t| names(t, &f.file.app, &f.file.id))
            {
                break;
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
        let (units, after) = render_plan(&state, &f.file, &f.at, &CTX, self.b)?;
        let moves = self.long_moves(&units).await?;
        let archives = self.archive_sizes(&f.file, &state, &f.at).await?;
        if self.opts.dry_run {
            self.plan(&f.file.id, &units);
            self.planned.extend(moves);
            self.planned.extend(archives);
            return Ok(after);
        }
        for m in moves.into_iter().filter(|m| m.contains("MV2-W011")) {
            tracing::warn!(target: "rustango::migrate", "{m}");
            self.warnings.push(m);
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

    /// Each archived drop's estimated size, as `--plan` prints it. Above
    /// [`ARCHIVE_LIMIT`] the run refuses before any statement, unless
    /// `--accept-archive-size` (7.5).
    async fn archive_sizes(
        &self,
        file: &File,
        before: &State,
        at: &str,
    ) -> Result<Vec<String>, RunError> {
        let mut out = Vec::new();
        for env in &file.operations {
            let Op::DropColumn {
                table,
                column,
                was,
                restore: super::format::Restore::Archive,
            } = &env.op
            else {
                continue;
            };
            if !before.tables.contains_key(table.as_str()) {
                continue;
            }
            let rows = super::catalog::estimate_rows(self.pool, table)
                .await?
                .unwrap_or(0);
            let width = u64::from(was.column.max_length.unwrap_or(16)) + 8;
            let bytes = u64::try_from(rows).unwrap_or(0) * width;
            if bytes > ARCHIVE_LIMIT && !self.opts.accept_archive_size {
                return Err(refused(
                    Code::Blocked,
                    at,
                    format!(
                        "archiving `{table}.{column}` takes ~{} MiB, above {} MiB: pass \
                         --accept-archive-size, or drop it without `restore: \"archive\"`",
                        bytes >> 20,
                        ARCHIVE_LIMIT >> 20
                    ),
                ));
            }
            out.push(format!(
                "-- archive of `{table}.{column}` into `{}`: ~{rows} rows, ~{} KiB",
                super::format::archive_name(table, column),
                bytes >> 10
            ));
        }
        Ok(out)
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
            if let RunError::Stopped(why) = &e {
                // A planned stop: failed with class `stopped`, the event says why.
                self.ledger
                    .set_state(self.pool, k, self.run_id, RowState::Failed, Some("stopped"))
                    .await?;
                self.ledger
                    .event(self.pool, "stopped", k, None, self.run_id, Some(why))
                    .await?;
                return Err(e);
            }
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

    /// `--resume-forward` (7.6): a half rolled back row goes forward again
    /// from the first forward op its rollback touched; a batched data op the
    /// rollback walked down continues up from its cursor.
    async fn resume_forward(
        &mut self,
        f: &Loaded,
        row: &Row,
        before: &State,
    ) -> Result<(), RunError> {
        let k = key(&f.file);
        if row.started_unit.is_some() {
            return Err(refused(
                Code::State,
                &f.at,
                "a reverse unit was cut off: run `downgrade` to settle it first",
            ));
        }
        let m: usize = self
            .ledger
            .last_reason(self.pool, k, ROLLBACK_BEGAN)
            .await?
            .and_then(|r| r.parse().ok())
            .ok_or_else(|| refused(Code::State, &f.at, "no record of where this rollback began"))?;
        let back = &row.unit_plan;
        let done = usize::try_from(row.pos + 1).unwrap_or(0).min(back.len());
        let mut touched: Vec<usize> = back[..done].iter().map(|u| u.op).collect();
        if let Some(next) = back.get(done) {
            if next.batch.is_some() {
                // Its batches may have walked down: it goes up again from the cursor.
                touched.push(next.op);
            } else if done > 0 && back[done - 1].op == next.op {
                return Err(refused(
                    Code::State,
                    &f.at,
                    format!(
                        "the rollback stopped inside the undo of op {}: run `downgrade` to finish \
                         it, then apply",
                        next.op
                    ),
                ));
            }
        }
        let from_op = touched.into_iter().min().unwrap_or(m);
        let (units, _) = render_plan(before, &f.file, &f.at, &CTX, self.b)?;
        let first = units
            .iter()
            .position(|u| u.op >= from_op)
            .unwrap_or(units.len());
        let pos = i64::try_from(first).expect("unit index fits") - 1;
        self.ledger
            .replan(
                self.pool,
                k,
                (&row.run_id, self.run_id),
                RowState::Running,
                &units,
                pos,
            )
            .await?;
        self.ledger
            .event(
                self.pool,
                "resumed_forward",
                k,
                Some(&f.checksum),
                self.run_id,
                Some(&format!("from op {from_op}")),
            )
            .await?;
        self.finish(f, &units, first).await
    }

    /// A crashed run's row: take it over, probe a started autocommit unit,
    /// then run the stored plan from the next unit. Never re-rendered.
    async fn resume(&mut self, f: &Loaded, row: &Row) -> Result<(), RunError> {
        let k = key(&f.file);
        // Decide before taking the row: a refusal leaves the ledger untouched.
        let at = self.resume_point(row, &f.at).await?;
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
        let from = self.take_resume(k, at).await?;
        self.finish(f, units, from).await
    }

    /// Where a crashed row goes on: after its last unit, or, for an
    /// autocommit unit cut off after it started, what its probe says. Ends
    /// the unit's dead DDL session first; writes nothing to the ledger.
    async fn resume_point(&self, row: &Row, at: &str) -> Result<ResumeAt, RunError> {
        let units = &row.unit_plan;
        let from = usize::try_from(row.pos + 1).unwrap_or(0);
        let Some(started) = row.started_unit else {
            return Ok(ResumeAt::Unit(from));
        };
        let unit = usize::try_from(started).unwrap_or(0);
        // A dead client's DDL keeps running: end it before probing (6.5).
        if let Some(s) = &row.session {
            let reaped = super::session::reap(
                self.pool,
                s,
                self.lock_timeout(),
                self.opts.accept_failover.is_some(),
            )
            .await
            .map_err(|e| match e {
                super::session::ReapError::Db(e) => RunError::Db(e),
                e => refused(Code::State, at, e.to_string()),
            })?;
            if let (super::session::Reaped::Failover(diff), Some(why)) =
                (reaped, &self.opts.accept_failover)
            {
                self.ledger
                    .event(
                        self.pool,
                        "accept_failover",
                        (&row.app, &row.id),
                        None,
                        self.run_id,
                        Some(&format!("{why}: {diff}")),
                    )
                    .await?;
            }
        }
        let expects = units
            .get(unit)
            .and_then(|u| u.probe.clone())
            .unwrap_or_default();
        Ok(match probe(self.pool, &expects).await? {
            Verdict::Present => ResumeAt::Done(unit),
            Verdict::Absent => ResumeAt::Unit(unit),
            Verdict::Partial(diff) => match super::probe::repair(&diff) {
                Some(super::probe::Repair::Rerun) => ResumeAt::Unit(unit),
                Some(super::probe::Repair::DropInvalid(names)) => ResumeAt::Rebuild(unit, names),
                None => return Err(half_way(at, unit, &diff)),
            },
        })
    }

    /// Act on a [`ResumeAt`] once the row is this run's: the unit to run next.
    async fn take_resume(&mut self, k: (&str, &str), at: ResumeAt) -> Result<usize, RunError> {
        match at {
            ResumeAt::Unit(n) => Ok(n),
            ResumeAt::Done(n) => {
                let to = i64::try_from(n).expect("unit index fits");
                let (sql, binds) = self.ledger.advance(self.pool, k, self.run_id, to - 1, to);
                super::ledger::fenced(raw_execute_pool(self.pool, &sql, binds).await?)?;
                Ok(n + 1)
            }
            ResumeAt::Rebuild(n, names) => {
                let mut ddl =
                    super::session::DdlConn::open(self.pool, "", self.lock_timeout()).await?;
                for name in names {
                    let sql = format!(
                        "DROP INDEX CONCURRENTLY {}",
                        self.pool.dialect().quote_ident(&name)
                    );
                    ddl.execute(&sql).await.map_err(MigrateError::Driver)?;
                }
                Ok(n)
            }
        }
    }

    /// The runner's own connection for a data unit, closed after the
    /// attempt: its session settings never reach the app's pool, and an
    /// in-memory SQLite pool of one is free again for the next unit.
    async fn unit_conn(&self) -> Result<crate::sql::PoolConn, Attempt> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| Attempt::Fatal(e.into()))?;
        conn.close_on_drop();
        session_timeouts(
            &mut conn,
            self.b,
            self.lock_timeout(),
            self.statement_timeout(),
        )
        .await
        .map_err(Attempt::Fatal)?;
        Ok(conn)
    }

    /// A data unit's write transaction, with PG's local timeouts.
    async fn unit_tx<'c>(
        &self,
        conn: &'c mut crate::sql::PoolConn,
    ) -> Result<crate::sql::PoolTx<'c>, Attempt> {
        let mut tx = conn
            .begin_write()
            .await
            .map_err(|e| retry_or_fatal(e.into()))?;
        if self.b == Backend::Postgres {
            tx.execute_unprepared(&format!(
                "SET LOCAL lock_timeout = {}; SET LOCAL statement_timeout = {}",
                self.lock_timeout().as_millis(),
                self.statement_timeout().as_millis()
            ))
            .await
            .map_err(|e| Attempt::Fatal(MigrateError::Driver(e).into()))?;
        }
        Ok(tx)
    }

    fn statement_timeout(&self) -> std::time::Duration {
        self.opts
            .data_statement_timeout
            .unwrap_or(std::time::Duration::from_secs(300))
    }

    /// The lock timeout of data and DDL units; also how long a reaped
    /// session may take to go.
    fn lock_timeout(&self) -> std::time::Duration {
        self.opts
            .data_lock_timeout
            .unwrap_or(std::time::Duration::from_secs(5))
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
                return Err(RunError::Stopped(format!(
                    "a shutdown signal after {} unit(s)",
                    self.units_run
                )));
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
                    // MySQL units are one statement (6.1 rule 3): the session names it.
                    let first = u.sql.first().map_or("", String::as_str);
                    let mut ddl =
                        super::session::DdlConn::open(self.pool, first, self.lock_timeout())
                            .await?;
                    self.ledger
                        .start_unit(self.pool, k, self.run_id, n, ddl.session.as_ref())
                        .await?;
                    #[cfg(test)]
                    if self.opts.crash == Some(Crash::AfterStart(self.units_run)) {
                        return Err(MigrateError::Validation("test crash".into()).into());
                    }
                    #[cfg(test)]
                    if self.opts.crash == Some(Crash::DdlLeftRunning(self.units_run)) {
                        let sql = u.sql.clone();
                        tokio::spawn(async move {
                            for s in &sql {
                                let _ = ddl.execute(s).await;
                            }
                        });
                        return Err(MigrateError::Validation("test crash".into()).into());
                    }
                    for s in &u.sql {
                        ddl.execute(s).await.map_err(MigrateError::Driver)?;
                    }
                    drop(ddl);
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
        // Only a SQLite pool holds legacy datetime text to sweep.
        if let Some(targets) = self.sweep.take() {
            #[cfg(feature = "sqlite")]
            crate::migrate::sqlite_datetime::normalise_sqlite_datetimes_in(self.pool, targets)
                .await?;
            #[cfg(not(feature = "sqlite"))]
            let _ = targets;
        }
        let ui = usize::try_from(n).unwrap_or(0);
        let mut batches_here = 0_u64;
        loop {
            if let Some(bp) = &u.batch {
                if batches_here > 0 || self.batches_run > 0 {
                    if self.stopping() {
                        return Err(RunError::Stopped(format!(
                            "a shutdown signal between batches of `{}:{}`",
                            k.0, k.1
                        )));
                    }
                    if self.out_of_time() {
                        return Err(RunError::Stopped(format!(
                            "--max-duration reached between batches of `{}:{}`",
                            k.0, k.1
                        )));
                    }
                    if self.opts.per_run_limits
                        && bp.max_batches_per_run.is_some_and(|m| batches_here >= m)
                    {
                        return Err(RunError::Stopped(format!(
                            "`{}:{}` ran its {batches_here} batches for this run",
                            k.0, k.1
                        )));
                    }
                }
            }
            let restore = u.step.as_ref().filter(|s| s.reverse && s.undo);
            let mut tries = 0_u32;
            let done = loop {
                let attempt = match restore {
                    Some(sr) => self.undo_attempt(k, n, sr).await,
                    None => self.data_attempt(k, u, n, ui).await,
                };
                match attempt {
                    Ok(done) => break done,
                    Err(Attempt::Retry(why)) if tries < DATA_RETRIES => {
                        tries += 1;
                        let pause = backoff(tries);
                        tracing::warn!(
                            target: "rustango::migrate",
                            "`{}:{}` unit {n}: {why}; retry {tries} of {DATA_RETRIES} in {pause:?}",
                            k.0,
                            k.1
                        );
                        tokio::time::sleep(pause).await;
                    }
                    Err(Attempt::Retry(why)) => {
                        return Err(MigrateError::Validation(format!(
                            "data step in `{}:{}`: {why} ({DATA_RETRIES} retries)",
                            k.0, k.1
                        ))
                        .into())
                    }
                    Err(Attempt::Fatal(e)) => return Err(e),
                }
            };
            if (u.batch.is_none() && restore.is_none()) || done {
                return Ok(());
            }
            batches_here += 1;
            self.batches_run += 1;
            if let Some(ms) = u.batch.as_ref().and_then(|b| b.sleep_ms) {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            }
        }
    }

    /// Each batched unit's row estimate, as `--plan` prints it, marked
    /// `MV2-W011` above `long_move_rows`.
    async fn long_moves(&self, units: &[Unit]) -> Result<Vec<String>, RunError> {
        let limit = self.opts.long_move_rows.unwrap_or(LONG_MOVE_ROWS);
        let mut out = Vec::new();
        for bp in units.iter().filter_map(|u| u.batch.as_ref()) {
            let est = super::catalog::estimate_rows(self.pool, &bp.table).await?;
            let rows = est.map_or_else(|| "unknown".to_owned(), |n| n.to_string());
            let long = est.is_some_and(|n| u64::try_from(n).unwrap_or(0) > limit);
            out.push(format!(
                "-- batched move over `{}` by `{}`, {} per batch, ~{rows} rows{}",
                bp.table,
                bp.key,
                bp.size,
                if long {
                    format!(" [MV2-W011: above {limit}; the app waits until it finishes]")
                } else {
                    String::new()
                }
            ));
        }
        Ok(out)
    }

    fn out_of_time(&self) -> bool {
        self.opts
            .max_duration
            .is_some_and(|d| self.started.elapsed() >= d)
    }

    /// One transaction of a data unit: the whole unit, or one batch. `Ok(true)`
    /// when the unit is done (its position moved).
    #[allow(clippy::too_many_lines)]
    async fn data_attempt(
        &mut self,
        k: (&str, &str),
        u: &Unit,
        n: i64,
        ui: usize,
    ) -> Result<bool, Attempt> {
        let fatal = |e: RunError| Attempt::Fatal(e);
        let mut conn = self.unit_conn().await?;
        let binds = super::data::bind_values(&u.binds)
            .map_err(|e| fatal(refused(Code::File, &format!("{}:{}", k.0, k.1), e)))?;
        let mut tx = self.unit_tx(&mut conn).await?;
        // A batched unit continues from its cursor, read in this transaction;
        // a reverse one from the forward op's.
        let pi = u.batch.as_ref().and_then(|b| b.down_of).unwrap_or(ui);
        let (lo, rows_done, batches) = match &u.batch {
            None => (None, 0, 0),
            Some(bp) => {
                let got = self
                    .ledger
                    .progress_tx(&mut tx, k, pi)
                    .await
                    .map_err(|e| fatal(e.into()))?;
                let (hi, rows, batches) = got.unwrap_or((None, 0, 0));
                let lo = hi
                    .map(|j| cursor_value(&j, &bp.key_ty))
                    .transpose()
                    .map_err(|e| fatal(refused(Code::State, &format!("{}:{}", k.0, k.1), e)))?;
                (lo, rows, batches)
            }
        };
        let mut hash = None;
        // (rows changed, the cursor after this batch, unit done)
        let (mut tx, r) = match (&u.step, &u.batch) {
            // Down: nothing in effect, nothing to undo.
            (_, Some(bp)) if bp.down_of.is_some() && lo.is_none() => (tx, Ok((0, None, true))),
            (_, Some(bp)) if bp.down_of.is_some() => {
                let hi = lo.clone().expect("checked above");
                let next = keyset_down(&mut tx, bp, &hi)
                    .await
                    .map_err(retry_or_fatal)?;
                let last = next.is_none();
                let (tx, r) = match &u.step {
                    None => {
                        let (sql, params) = super::data::expand(
                            &u.sql.join("\n"),
                            tx.dialect(),
                            binds,
                            &bp.key,
                            next.as_ref(),
                            Some(&hi),
                        );
                        super::step::run_step(
                            tx,
                            super::step::Scope::default(),
                            sql_step(sql, params),
                        )
                        .await
                    }
                    Some(sr) => {
                        let f = super::step::registered(k.0, &sr.id)
                            .first()
                            .and_then(|s| s.down)
                            .ok_or_else(|| {
                                fatal(refused(
                                    Code::Unresolved,
                                    &format!("{}:{}", k.0, k.1),
                                    format!("data step `{}:{}` has no down here", k.0, sr.id),
                                ))
                            })?;
                        let scope = super::step::Scope {
                            app: k.0.to_owned(),
                            footprint: sr.footprint.clone(),
                            tables: sr.tables.clone(),
                            undo: None,
                        };
                        let batch = super::step::Batch::down(next.clone(), hi.clone(), bp.size);
                        let (tx, r) = super::step::run_step(tx, scope, rust_step(f, batch)).await;
                        (tx, r.map(|_| 0))
                    }
                };
                (tx, r.map(|rows| (rows, next, last)))
            }
            (None, None) => {
                let sql = u.sql.join("\n");
                let (tx, r) =
                    super::step::run_step(tx, super::step::Scope::default(), sql_step(sql, binds))
                        .await;
                (tx, r.map(|rows| (rows, None, true)))
            }
            (None, Some(bp)) => {
                let (hi, last) = keyset(&mut tx, bp, lo.as_ref())
                    .await
                    .map_err(retry_or_fatal)?;
                let upper = if last { None } else { hi.clone() };
                let (sql, params) = super::data::expand(
                    &u.sql.join("\n"),
                    tx.dialect(),
                    binds,
                    &bp.key,
                    lo.as_ref(),
                    upper.as_ref(),
                );
                let (tx, r) =
                    super::step::run_step(tx, super::step::Scope::default(), sql_step(sql, params))
                        .await;
                let (mut tx, r) = (tx, r);
                // The last batch is open-ended: its cursor is the top key it covered.
                let cursor = if last && r.is_ok() {
                    top_key(&mut tx, bp, lo.as_ref())
                        .await
                        .map_err(retry_or_fatal)?
                } else {
                    hi
                };
                (tx, r.map(|rows| (rows, cursor, last)))
            }
            (Some(sr), _) => {
                let found = super::step::registered(k.0, &sr.id);
                let f = found
                    .first()
                    .and_then(|s| if sr.reverse { s.down } else { Some(s.up) })
                    .ok_or_else(|| {
                        fatal(refused(
                            Code::Unresolved,
                            &format!("{}:{}", k.0, k.1),
                            format!("data step `{}:{}` is not registered here", k.0, sr.id),
                        ))
                    })?;
                hash = found.first().map(|s| (sr.id.clone(), s.code_hash()));
                let scope = super::step::Scope {
                    app: k.0.to_owned(),
                    footprint: sr.footprint.clone(),
                    tables: sr.tables.clone(),
                    undo: (sr.undo && !sr.reverse).then(|| super::step::UndoTarget {
                        sql: self.ledger.undo_record_sql(tx.dialect()),
                        app: k.0.to_owned(),
                        mig: k.1.to_owned(),
                        op: i32::try_from(ui).unwrap_or(i32::MAX),
                    }),
                };
                let batch = match &u.batch {
                    Some(bp) => super::step::Batch::range(lo.clone(), bp.size),
                    None => super::step::Batch::whole(),
                };
                let (mut tx, r) = super::step::run_step(tx, scope, rust_step(f, batch)).await;
                let r = match (r, &u.batch) {
                    (Ok(super::step::Progress::Next(v)), Some(bp)) => {
                        // Strict progress: the cursor moves up, or the step is wrong.
                        match &lo {
                            Some(prev) if !advances(prev, &v, &bp.key_ty) => {
                                Err(super::step::UnitError::Step(
                                    super::step::StepError::Message(format!(
                                        "the batch ended at {v:?}, not above the last cursor \
                                         {prev:?}: a batch must advance"
                                    )),
                                ))
                            }
                            _ => Ok((0, Some(v), false)),
                        }
                    }
                    // Done: the cursor is the top key, so a down walk covers it all.
                    (Ok(_), Some(bp)) => top_key(&mut tx, bp, lo.as_ref())
                        .await
                        .map(|top| (0, top, true))
                        .map_err(|e| {
                            super::step::UnitError::Step(super::step::StepError::Message(
                                e.to_string(),
                            ))
                        }),
                    (Ok(_), None) => Ok((0, None, true)),
                    (Err(e), _) => Err(e),
                };
                (tx, r)
            }
        };
        #[cfg(test)]
        let r = match r {
            Ok(v)
                if self.opts.crash == Some(Crash::AfterData(self.units_run))
                    || (u.batch.is_some()
                        && self.opts.crash == Some(Crash::MidBatch(self.batches_run))) =>
            {
                let _ = v;
                Err(super::step::UnitError::Step(
                    super::step::StepError::Message("test crash".into()),
                ))
            }
            other => other,
        };
        let (rows, cursor, done) = match r {
            Ok(v) => v,
            Err(e) => {
                let _ = tx.rollback().await;
                let why = format!("data step in `{}:{}`: {e}", k.0, k.1);
                return Err(if e.retryable() {
                    Attempt::Retry(why)
                } else {
                    fatal(MigrateError::Validation(why).into())
                });
            }
        };
        let batches = batches + 1;
        if u.batch.is_some() {
            let rows_done = rows_done + i64::try_from(rows).unwrap_or(i64::MAX);
            let json = cursor
                .as_ref()
                .map(cursor_json)
                .transpose()
                .map_err(|e| fatal(refused(Code::State, &format!("{}:{}", k.0, k.1), e)))?;
            self.ledger
                .put_progress_tx(
                    &mut tx,
                    k,
                    pi,
                    json.as_deref(),
                    rows_done,
                    batches,
                    if u.batch.as_ref().is_some_and(|b| b.down_of.is_some()) {
                        super::ledger::Walk::Down
                    } else {
                        super::ledger::Walk::Up
                    },
                )
                .await
                .map_err(|e| fatal(e.into()))?;
            let detail = format!(
                "op {} batch {batches} up to {} ({rows} rows{})",
                u.op,
                json.as_deref().unwrap_or("the end"),
                if done { ", last" } else { "" }
            );
            let (sql, ev) =
                self.ledger
                    .event_stmt(tx.dialect(), "batch", k, None, self.run_id, Some(&detail));
            raw_execute_tx(&mut tx, &sql, ev)
                .await
                .map_err(|e| fatal(e.into()))?;
        }
        // Fenced: the unit's position when done, else a heartbeat at `pos`.
        let (sql, fb) = if done {
            self.ledger.advance(self.pool, k, self.run_id, n - 1, n)
        } else {
            self.ledger.heartbeat(self.pool, k, self.run_id, n - 1)
        };
        let moved = raw_execute_tx(&mut tx, &sql, fb)
            .await
            .map_err(|e| fatal(e.into()))?;
        if moved != 1 {
            let _ = tx.rollback().await;
            return Err(fatal(
                super::ledger::fenced(moved)
                    .expect_err("not one row")
                    .into(),
            ));
        }
        let committed = self.lose_ack(u.batch.is_some(), tx.commit().await);
        if let Err(e) = committed {
            // An unknown outcome (6.3): read what the database holds.
            let landed = match u.batch {
                Some(_) => self.batch_committed(k, pi, batches).await?,
                None => self.unit_committed(k, n).await.map_err(fatal)?,
            };
            if !landed {
                return Err(Attempt::Retry(format!("commit failed: {e}")));
            }
            tracing::warn!(
                target: "rustango::migrate",
                "`{}:{}` unit {n}: the commit's answer was lost ({e}); it committed",
                k.0,
                k.1
            );
        }
        if let Some((id, hash)) = hash {
            let detail = format!("{id} {hash}");
            self.ledger
                .event(self.pool, STEP_HASH, k, None, self.run_id, Some(&detail))
                .await
                .map_err(|e| fatal(e.into()))?;
        }
        Ok(done)
    }

    /// Test failpoint: report a commit that went through as lost.
    fn lose_ack(&self, batched: bool, r: Result<(), sqlx::Error>) -> Result<(), sqlx::Error> {
        #[cfg(test)]
        if r.is_ok()
            && self.opts.crash
                == Some(if batched {
                    Crash::LostAck(self.batches_run)
                } else {
                    Crash::LostUnitAck(self.units_run)
                })
        {
            return Err(sqlx::Error::Protocol(
                "test: the commit's answer was lost".into(),
            ));
        }
        let _ = batched;
        r
    }

    /// One batch of an undo restore (7.4): up to [`UNDO_BATCH`] captured
    /// changes, newest first, put back and deleted in one transaction with
    /// the fenced heartbeat. `Ok(true)` once none are left (the unit is done).
    async fn undo_attempt(
        &mut self,
        k: (&str, &str),
        n: i64,
        sr: &super::render::StepRef,
    ) -> Result<bool, Attempt> {
        let fatal = |e: RunError| Attempt::Fatal(e);
        let at = format!("{}:{}", k.0, k.1);
        let of = sr.undo_of.ok_or_else(|| {
            fatal(refused(
                Code::State,
                &at,
                "an undo restore without its forward unit",
            ))
        })?;
        let mut conn = self.unit_conn().await?;
        let mut tx = self.unit_tx(&mut conn).await?;
        let recs = self
            .ledger
            .undo_rows_tx(&mut tx, k, of, UNDO_BATCH)
            .await
            .map_err(|e| retry_or_fatal(e.into()))?;
        let done = recs.is_empty();
        if !done {
            let scope = super::step::Scope {
                app: k.0.to_owned(),
                footprint: sr.footprint.clone(),
                tables: sr.tables.clone(),
                undo: None,
            };
            let batch = recs.clone();
            let (back, r) = super::step::run_step(tx, scope, move |ctx| {
                Box::pin(async move {
                    let mut skipped = 0_u64;
                    for rec in &batch {
                        if !super::step::restore(ctx, rec).await? {
                            skipped += 1;
                        }
                    }
                    Ok(skipped)
                })
            })
            .await;
            tx = back;
            match r {
                Ok(skipped) if skipped > 0 => tracing::info!(
                    target: "rustango::migrate",
                    "`{at}`: {skipped} captured change(s) left as they are: edited since, or gone"
                ),
                Ok(_) => {}
                Err(e) => {
                    let _ = tx.rollback().await;
                    let why = format!("undo restore in `{at}`: {e}");
                    return Err(if e.retryable() {
                        Attempt::Retry(why)
                    } else {
                        fatal(MigrateError::Validation(why).into())
                    });
                }
            }
            let ids: Vec<i64> = recs.iter().map(|r| r.id).collect();
            self.ledger
                .undo_delete_tx(&mut tx, &ids)
                .await
                .map_err(|e| fatal(e.into()))?;
            #[cfg(test)]
            if self.opts.crash == Some(Crash::MidBatch(self.batches_run)) {
                let _ = tx.rollback().await;
                return Err(fatal(MigrateError::Validation("test crash".into()).into()));
            }
        }
        let (sql, fb) = if done {
            self.ledger.advance(self.pool, k, self.run_id, n - 1, n)
        } else {
            self.ledger.heartbeat(self.pool, k, self.run_id, n - 1)
        };
        let moved = raw_execute_tx(&mut tx, &sql, fb)
            .await
            .map_err(|e| fatal(e.into()))?;
        if moved != 1 {
            let _ = tx.rollback().await;
            return Err(fatal(
                super::ledger::fenced(moved)
                    .expect_err("not one row")
                    .into(),
            ));
        }
        // Restored rows are deleted with the batch, so a redo after a lost
        // answer finds the next ones; the last batch carries the advance.
        // Failpoint: a batch's lost answer counts in batches, the last one in units.
        if let Err(e) = self.lose_ack(!done, tx.commit().await) {
            if done && self.unit_committed(k, n).await.map_err(fatal)? {
                return Ok(true);
            }
            return Err(Attempt::Retry(format!("commit failed: {e}")));
        }
        Ok(done)
    }

    /// After a lost commit answer on unit `n`: its commit carries the
    /// ledger advance, so a row at `n` or past it means it committed.
    async fn unit_committed(&self, k: (&str, &str), n: i64) -> Result<bool, RunError> {
        let rows = self.ledger.rows(self.pool).await?;
        Ok(rows
            .iter()
            .any(|r| r.app == k.0 && r.id == k.1 && r.run_id == self.run_id && r.pos >= n))
    }

    /// The outcome of a transactional unit's commit: a lost answer whose
    /// ledger advance landed is done, never an error.
    async fn commit_outcome(
        &self,
        k: (&str, &str),
        n: i64,
        r: Result<(), MigrateError>,
    ) -> Result<(), RunError> {
        let r = r.and_then(|()| self.lose_ack(false, Ok(())).map_err(MigrateError::Driver));
        let Err(e) = r else {
            return Ok(());
        };
        if self.unit_committed(k, n).await? {
            tracing::warn!(
                target: "rustango::migrate",
                "`{}:{}` unit {n}: the commit's answer was lost ({e}); it committed",
                k.0,
                k.1
            );
            return Ok(());
        }
        Err(e.into())
    }

    /// After a lost commit answer: did batch `batches` of unit `ui` commit?
    async fn batch_committed(
        &self,
        k: (&str, &str),
        ui: usize,
        batches: i64,
    ) -> Result<bool, Attempt> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| Attempt::Fatal(e.into()))?;
        let mut tx = conn.begin().await.map_err(|e| Attempt::Fatal(e.into()))?;
        let got = self
            .ledger
            .progress_tx(&mut tx, k, ui)
            .await
            .map_err(|e| Attempt::Fatal(e.into()))?;
        let _ = tx.rollback().await;
        Ok(got.is_some_and(|(_, _, b)| b >= batches))
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
        let r = tx.commit().await.map_err(MigrateError::Driver);
        self.commit_outcome(k, n, r).await
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
            let r = tx.commit().await;
            self.commit_outcome(k, n, r).await
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
        // Applied rows, and a partial one: its forward work in effect rolls back (7.6).
        let applied: Vec<&Row> = rows
            .iter()
            .filter(|r| r.state != RowState::RollingBack)
            .collect();
        let mut bodies = Vec::with_capacity(applied.len());
        for r in &applied {
            let at = format!("ledger:{}:{}", r.app, r.id);
            bodies.push(body(r, &at)?);
        }
        // Rows come in `seq` order; the chosen ones newest first.
        let chosen: Vec<usize> = match back {
            Back::Steps(n) => (0..applied.len()).rev().take(n).collect(),
            Back::To(to) => {
                let pos = applied
                    .iter()
                    .position(|r| names(to, &r.app, &r.id))
                    .ok_or_else(|| {
                        refused(Code::State, to, format!("`{to}` is not applied here"))
                    })?;
                let set = descendants(&applied, &bodies, pos);
                let others: Vec<String> = set
                    .iter()
                    .filter(|&&i| applied[i].app != applied[pos].app)
                    .map(|&i| format!("{}:{}", applied[i].app, applied[i].id))
                    .collect();
                if !others.is_empty() && !self.opts.cascade {
                    return Err(refused(
                        Code::Blocked,
                        to,
                        format!(
                            "migrations in other apps depend on `{to}`: {}; pass --cascade to \
                             roll them back too",
                            others.join(", ")
                        ),
                    ));
                }
                set.into_iter().rev().collect()
            }
        };
        // What each chosen row has in effect: its whole body, or a partial
        // row's prefix.
        for &idx in &chosen {
            let at = format!("ledger:{}:{}", applied[idx].app, applied[idx].id);
            bodies[idx] = self
                .in_effect(applied[idx], bodies[idx].clone(), &at)
                .await?;
        }
        let mut misses = Vec::new();
        for &idx in &chosen {
            misses.extend(super::data::misses(&bodies[idx], self.b, true));
        }
        super::data::refuse_misses(&misses, "rollback plan")?;
        let mut plan: Vec<(&Row, Vec<Unit>)> = Vec::new();
        let mut in_effect_ops: Vec<usize> = Vec::new();
        let mut downs: Vec<File> = Vec::new();
        for &idx in &chosen {
            let row = applied[idx];
            let at = format!("ledger:{}:{}", row.app, row.id);
            let file = bodies[idx].clone();
            if file.kind == Kind::Checkpoint {
                return Err(refused(
                    Code::Floor,
                    &at,
                    match &file.base {
                        Some(base) => format!(
                            "v2 rollback stops at the switch point (`{}:{}`, from v1 `{}`)",
                            row.app, row.id, base.legacy
                        ),
                        None => format!(
                            "v2 rollback stops at the squash checkpoint: squashed at `{}:{}`",
                            row.app, row.id
                        ),
                    },
                ));
            }
            // State before it: every earlier stored body, in apply order.
            let mut state = State::default();
            for (r, f) in applied[..idx].iter().zip(&bodies) {
                let at = format!("ledger:{}:{}", r.app, r.id);
                apply_file(&mut state, f, &at, &CTX)?;
            }
            let (down, of) = self.down_file(&file, &state, &at)?;
            let mut after = state.clone();
            apply_file(&mut after, &file, &at, &CTX)?;
            let (mut units, _) = render_plan(&after, &down, &at, &CTX, self.b)?;
            // A batched reverse walks down the progress its forward unit
            // left; an undo restore reads that unit's undo rows.
            for u in &mut units {
                let fwd_op = of[u.op];
                if let Some(bp) = u.batch.as_mut() {
                    bp.down_of = row
                        .unit_plan
                        .iter()
                        .position(|f| f.op == fwd_op && f.batch.is_some());
                }
                if let Some(sr) = u.step.as_mut().filter(|s| s.undo) {
                    sr.undo_of = row
                        .unit_plan
                        .iter()
                        .position(|f| f.op == fwd_op && f.step.is_some());
                }
                // From here on a reverse unit names the forward op it undoes.
                u.op = fwd_op;
            }
            plan.push((row, units));
            in_effect_ops.push(file.operations.len());
            downs.push(down);
        }
        // Data probes before any statement (7.1 step 6): every hit is a blocker.
        if !plan.is_empty() {
            let mut now = State::default();
            for (r, f) in applied.iter().zip(&bodies) {
                let at = format!("ledger:{}:{}", r.app, r.id);
                apply_file(&mut now, f, &at, &CTX)?;
            }
            let ops: Vec<&OpEnvelope> = downs.iter().flat_map(|f| &f.operations).collect();
            let needs = super::preflight::checkable(&ops, &now);
            let hits = super::preflight::probe(self.pool, &needs).await?;
            if !hits.is_empty() {
                let list: Vec<String> = hits.iter().map(|h| format!("  - {h}")).collect();
                return Err(refused(
                    Code::Blocked,
                    "rollback plan",
                    format!(
                        "the rows do not fit the schema the rollback restores; nothing ran:\n{}",
                        list.join("\n")
                    ),
                ));
            }
        }
        let mut done = Vec::new();
        // A half-finished rollback finishes first, from its frozen plan.
        for row in rows.iter().filter(|r| r.state == RowState::RollingBack) {
            self.finish_rollback(row, &row.unit_plan, None).await?;
            done.push(row.id.clone());
        }
        for ((row, units), ops) in plan.into_iter().zip(in_effect_ops) {
            let k = (row.app.as_str(), row.id.as_str());
            self.ledger
                .begin_rollback(self.pool, k, &row.run_id, self.run_id, &units)
                .await?;
            // How many forward ops were in effect: `--resume-forward` needs it.
            self.ledger
                .event(
                    self.pool,
                    ROLLBACK_BEGAN,
                    k,
                    Some(&row.checksum),
                    self.run_id,
                    Some(&ops.to_string()),
                )
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
        let at = match from {
            Some(n) => ResumeAt::Unit(n),
            None => {
                let at = format!("ledger:{}:{}", row.app, row.id);
                self.resume_point(row, &at).await?
            }
        };
        if row.run_id != self.run_id {
            self.ledger
                .claim(self.pool, k, &row.run_id, self.run_id)
                .await?;
        }
        let from = self.take_resume(k, at).await?;
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
    /// The inverse of `file`'s ops, newest first, and for each reverse op the
    /// index of the forward op it undoes.
    fn down_file(
        &self,
        file: &File,
        before: &State,
        at: &str,
    ) -> Result<(File, Vec<usize>), RunError> {
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
        let mut of = Vec::new();
        for (i, env) in file.operations.iter().enumerate().rev() {
            let op_at = format!("{at}#/operations/{i}");
            // A drop that an earlier data op `restores` runs its reverse
            // down after the re-add: no data is lost (7.5).
            let allow = Allow {
                data_loss: allow.data_loss || restored(&file.operations[..i], &env.op),
            };
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
                of.push(i);
            }
        }
        Ok((
            File {
                operations: ops,
                ..file.clone()
            },
            of,
        ))
    }

    /// What `row` has in effect: an applied row its whole body; a partial
    /// one the ops its committed units cover, plus a batched op whose
    /// batches began. A cut inside any other multi-unit op is refused.
    async fn in_effect(&self, row: &Row, file: File, at: &str) -> Result<File, RunError> {
        if row.state == RowState::Applied {
            return Ok(file);
        }
        if row.started_unit.is_some() {
            return Err(refused(
                Code::State,
                at,
                "an autocommit unit was cut off: run `manage dbmigrate apply` to settle it, then \
                 roll back",
            ));
        }
        let units = &row.unit_plan;
        let done = usize::try_from(row.pos + 1).unwrap_or(0);
        let mut ops = units[..done.min(units.len())]
            .iter()
            .map(|u| u.op + 1)
            .max()
            .unwrap_or(0);
        if let Some(next) = units.get(done) {
            let begun = next.batch.is_some() && {
                let mut conn = self.pool.acquire().await?;
                let mut tx = conn.begin().await?;
                let got = self
                    .ledger
                    .progress_tx(&mut tx, (&row.app, &row.id), done)
                    .await?;
                let _ = tx.rollback().await;
                got.is_some_and(|(_, _, batches)| batches > 0)
            };
            if begun {
                ops = next.op + 1;
            } else if done > 0 && units[done - 1].op == next.op {
                return Err(refused(
                    Code::State,
                    at,
                    format!(
                        "the run stopped inside op {} (unit {done} of its units): run `manage \
                         dbmigrate apply` to finish it, then roll back",
                        next.op
                    ),
                ));
            }
        }
        Ok(File {
            operations: file.operations[..ops.min(file.operations.len())].to_vec(),
            ..file
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
        RunError::Stopped(_) => "stopped",
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
