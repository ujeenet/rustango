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
    /// Test failpoint: stop as if the process died.
    #[cfg(test)]
    pub(crate) crash: Option<Crash>,
}

/// Where a test run stops as if killed, counted in units this run ran.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Crash {
    BeforeUnit(usize),
    /// After an autocommit unit's started mark, before its statement.
    AfterStart(usize),
    AfterStatement(usize),
}

/// What a run did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Report {
    pub(crate) system: Vec<String>,
    pub(crate) applied: Vec<String>,
    pub(crate) rolled_back: Vec<String>,
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
    match pool.dialect().kind() {
        crate::sql::DialectKind::Postgres => Backend::Postgres,
        crate::sql::DialectKind::Mysql => Backend::Mysql,
        _ => Backend::Sqlite,
    }
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

/// The lock, the v1 system chain first, then `body`, then the framework's
/// bootstrap tables. One lock for all of it.
async fn locked<F, Fut, R>(pool: &Pool, dir: &Path, body: F) -> Result<(R, Vec<String>), RunError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<R, RunError>>,
{
    use crate::core::ModelScope;
    let chain = crate::migrate::make::SystemChain::for_migrations_dir(
        dir,
        &[ModelScope::Registry, ModelScope::Tenant],
    )?;
    let out = crate::migrate::with_migrate_lock_held_v2(V2Token(()), pool, |held| async move {
        let system = crate::migrate::chains::system_chain_for_v2(
            held,
            pool,
            &chain,
            crate::migrate::MigrationScope::Tenant,
        )
        .await?;
        let result = body().await;
        if result.is_ok() {
            crate::migrate::manage::ensure_bootstrap_tables(pool).await?;
            crate::migrate::manage::ensure_tenant_bootstrap_tables(pool).await?;
        }
        Ok((
            result,
            system.into_iter().map(|m| m.name).collect::<Vec<_>>(),
        ))
    })
    .await?;
    let (result, system) = out;
    Ok((result?, system))
}

/// Apply every pending migration of the engine v2 project around `dir`,
/// resuming a crashed one from its frozen plan.
///
/// # Errors
/// A load error, a refusal (edited applied file, drift, behind the v1
/// head, unowned tables), a failed unit, or a lost lock.
pub(crate) async fn migrate(pool: &Pool, dir: &Path, opts: &Options) -> Result<Report, RunError> {
    let project = load_project(dir)?;
    refuse_registry(&project)?;
    let ledger = Ledger::default_names();
    let run_id = new_run_id();
    let (applied, system) = locked(pool, dir, || async {
        ledger.ensure(pool).await?;
        let mut runner = Runner {
            pool,
            ledger: &ledger,
            run_id: &run_id,
            b: backend_of(pool),
            opts,
            units_run: 0,
        };
        runner.apply_project(&project).await
    })
    .await?;
    Ok(Report {
        system,
        applied,
        rolled_back: Vec::new(),
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
    load_project(dir)?;
    let ledger = Ledger::default_names();
    let run_id = new_run_id();
    let (rolled_back, system) = locked(pool, dir, || async {
        ledger.ensure(pool).await?;
        let mut runner = Runner {
            pool,
            ledger: &ledger,
            run_id: &run_id,
            b: backend_of(pool),
            opts,
            units_run: 0,
        };
        runner.downgrade(steps).await
    })
    .await?;
    Ok(Report {
        system,
        applied: Vec::new(),
        rolled_back,
    })
}

struct Runner<'a> {
    pool: &'a Pool,
    ledger: &'a Ledger,
    run_id: &'a str,
    b: Backend,
    opts: &'a Options,
    /// Units run so far in this run, for the test failpoints.
    units_run: usize,
}

fn key(f: &File) -> (&str, &str) {
    (&f.app, &f.id)
}

impl Runner<'_> {
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
        let mut from = usize::try_from(row.pos + 1).unwrap_or(0);
        if let Some(started) = row.started_unit {
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
                    from = unit + 1;
                }
                Verdict::Absent => from = unit,
                Verdict::Partial(diff) => {
                    let list: Vec<String> = diff.iter().map(ToString::to_string).collect();
                    return Err(refused(
                        Code::State,
                        &f.at,
                        format!(
                            "unit {unit} was cut off and the catalog is half way: {}; the \
                             ledger is untouched",
                            list.join("; ")
                        ),
                    ));
                }
            }
        }
        self.finish(f, units, from).await
    }

    /// Run units `from..`, each committing with its fenced position update.
    async fn run_units(
        &mut self,
        k: (&str, &str),
        units: &[Unit],
        from: usize,
    ) -> Result<(), RunError> {
        for (n, u) in units.iter().enumerate().skip(from) {
            #[cfg(test)]
            if self.opts.crash == Some(Crash::BeforeUnit(self.units_run)) {
                return Err(MigrateError::Validation("test crash".into()).into());
            }
            let n = i64::try_from(n).expect("unit index fits");
            match u.kind {
                UnitKind::Tx => self.tx_unit(k, u, n).await?,
                UnitKind::Rebuild => self.rebuild_unit(k, u, n).await?,
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
            let moved =
                crate::sql::sqlx::query(&self.ledger.advance_literal(k, self.run_id, n - 1, n))
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
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
        let v1 = crate::migrate::LEDGER_TABLE;
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
            self.ledger
                .insert(
                    self.pool,
                    k,
                    &f.checksum,
                    &f.body,
                    RowState::Running,
                    &[],
                    self.run_id,
                    (&hashes.0, Some(&hashes.1)),
                    true,
                )
                .await?;
            if !diff.is_empty() {
                self.ledger
                    .event(
                        self.pool,
                        "drift_accepted",
                        k,
                        Some(&f.checksum),
                        self.run_id,
                        self.opts.accept_drift.as_deref(),
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
            self.ledger
                .set_state(self.pool, k, self.run_id, RowState::Applied, None)
                .await?;
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

    async fn downgrade(&mut self, steps: usize) -> Result<Vec<String>, RunError> {
        let rows = self.ledger.rows(self.pool).await?;
        let mut done = Vec::new();
        // A half-finished rollback finishes first.
        for row in rows.iter().filter(|r| r.state == RowState::RollingBack) {
            self.finish_rollback(
                row,
                &row.unit_plan,
                usize::try_from(row.pos + 1).unwrap_or(0),
            )
            .await?;
            done.push(row.id.clone());
        }
        let applied: Vec<&Row> = rows
            .iter()
            .filter(|r| r.state == RowState::Applied)
            .collect();
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
            self.finish_rollback(&mine, &units, 0).await?;
            done.push(row.id.clone());
        }
        Ok(done)
    }

    async fn finish_rollback(
        &mut self,
        row: &Row,
        units: &[Unit],
        from: usize,
    ) -> Result<(), RunError> {
        let k = (row.app.as_str(), row.id.as_str());
        if row.run_id != self.run_id {
            self.ledger
                .claim(self.pool, k, &row.run_id, self.run_id)
                .await?;
        }
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
fn fk_order<'s>(state: &'s State, at: &str) -> Result<Vec<&'s super::state::TableState>, RunError> {
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
