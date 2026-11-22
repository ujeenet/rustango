//! SPIKE S5: the frozen-plan runner and resume after a crash (6.3, 6.5).
//!
//! A file is planned once; the units go into the ledger with the row. A run
//! that finds a `running` row executes that stored plan from `pos`, never a
//! re-render. Transactional units carry the fenced `pos` update in their own
//! transaction. An autocommit unit (MySQL DDL, PG CONCURRENTLY) is marked
//! started first, with the session that runs it; a resumer reaps that
//! session if it is still alive, then lets the unit's probe decide.
//! Throwaway: ledger SQL is written here, not in the dialect emitters.

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::lock::{try_lock, LockScope};
use super::ops::SpikeOp;
use super::probe::{probe, Mismatch, Prop, Verdict};
use super::render::{self, Op, State, UnitKind};
use crate::core::SqlValue;
use crate::sql::{
    raw_execute_pool, raw_execute_tx, raw_query_pool, Dialect, DialectKind, Pool, PoolConn,
};

/// The spike ledger (one table; progress lives in the row).
pub const LEDGER: &str = "_rgo_s5_ledger";

// ------------------------------------------------------------------ files

/// A `data_sql` op with a keyset batch. `up` may use `{{t:..}}`, `{{c:..}}`
/// and `{{batch_range}}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataSql {
    pub id: String,
    pub table: String,
    pub key: String,
    pub size: i64,
    pub up: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOp {
    Schema(Op),
    Data(DataSql),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub app: String,
    pub id: String,
    pub ops: Vec<FileOp>,
}

impl File {
    fn checksum(&self) -> String {
        sha(&format!("{:?}", self.ops))
    }
}

fn sha(s: &str) -> String {
    hex(&Sha256::digest(s.as_bytes()))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ------------------------------------------------------------------- plan

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanKind {
    Tx,
    Autocommit,
    Rebuild,
    /// One entry for a whole batched op; progress is `effect_hi`.
    Batched,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Batch {
    pub table: String,
    pub key: String,
    pub size: i64,
}

/// One frozen unit. `probe` is required on an autocommit unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanUnit {
    pub op: usize,
    pub kind: PlanKind,
    /// Batched: one statement whose two placeholders are `(lo, hi]`.
    pub sql: Vec<String>,
    pub sha: String,
    pub probe: Option<SpikeOp>,
    pub batch: Option<Batch>,
}

/// What an autocommit unit's probe compares: the after-state of its op.
/// `None` when an op has more than one autocommit unit (each would need
/// its own intermediate state) or no probe mapping.
#[must_use]
pub fn unit_probe(op: &Op, autocommit_units: usize) -> Option<SpikeOp> {
    if let Op::RawSql { state, .. } = op {
        // Every statement unit probes the declared final state.
        return match state.as_slice() {
            [one] => unit_probe(one, 1),
            _ => None,
        };
    }
    if autocommit_units != 1 {
        return None;
    }
    Some(match op {
        Op::CreateTable { table, .. } => SpikeOp::CreateTable(table.clone()),
        Op::DropTable { was } => SpikeOp::DropTable { was: was.clone() },
        Op::AddColumn {
            table,
            column,
            fill: None,
        } => SpikeOp::AddColumn {
            table: table.clone(),
            column: column.clone(),
        },
        Op::DropColumn { table, was } => SpikeOp::DropColumn {
            table: table.clone(),
            was: was.clone(),
        },
        Op::AlterColumn { table, from, to } => SpikeOp::AlterColumn {
            table: table.clone(),
            from: from.clone(),
            to: to.clone(),
        },
        Op::AddIndex { index, .. } => SpikeOp::AddIndex(index.decl.clone()),
        Op::DropIndex { was, .. } => SpikeOp::DropIndex {
            was: was.decl.clone(),
        },
        Op::AddCheck { table, check } => SpikeOp::AddCheck {
            table: table.clone(),
            check: check.clone(),
        },
        Op::DropCheck { table, was } => SpikeOp::DropCheck {
            table: table.clone(),
            was: was.clone(),
        },
        Op::AddFk { table, fk } => SpikeOp::AddFk {
            table: table.clone(),
            fk: fk.clone(),
        },
        Op::DropFk { table, was } => SpikeOp::DropFk {
            table: table.clone(),
            was: was.clone(),
        },
        _ => return None,
    })
}

/// Plan `file` from `state` on `d`: units in op order, and the state after.
///
/// # Errors
/// A render error, or an autocommit unit with no probe.
pub fn plan(file: &File, state: &State, d: &dyn Dialect) -> Result<(Vec<PlanUnit>, State), String> {
    let schema: Vec<(usize, &Op)> = file
        .ops
        .iter()
        .enumerate()
        .filter_map(|(i, o)| match o {
            FileOp::Schema(op) => Some((i, op)),
            FileOp::Data(_) => None,
        })
        .collect();
    let ops: Vec<Op> = schema.iter().map(|(_, o)| (*o).clone()).collect();
    let (units, after) =
        render::render_file(state, &ops, d).map_err(|(i, e)| format!("op {}: {e}", schema[i].0))?;
    let mut out = Vec::new();
    for (fi, fop) in file.ops.iter().enumerate() {
        match fop {
            FileOp::Data(ds) => {
                let range = format!(
                    "{k} > {} AND {k} <= {}",
                    d.placeholder(1),
                    d.placeholder(2),
                    k = d.quote_ident(&ds.key)
                );
                let sql = render::expand_templates(state, &after, d, &ds.up, Some(&range))
                    .map_err(|e| format!("op {fi}: {e}"))?;
                out.push(PlanUnit {
                    op: fi,
                    kind: PlanKind::Batched,
                    sha: sha(&sql),
                    sql: vec![sql],
                    probe: None,
                    batch: Some(Batch {
                        table: ds.table.clone(),
                        key: ds.key.clone(),
                        size: ds.size,
                    }),
                });
            }
            FileOp::Schema(op) => {
                let si = schema.iter().position(|(i, _)| *i == fi).expect("indexed");
                let mine: Vec<&render::Unit> = units.iter().filter(|u| u.op == si).collect();
                let autos = mine
                    .iter()
                    .filter(|u| u.kind == UnitKind::Autocommit)
                    .count();
                for u in mine {
                    let (kind, probe) = match u.kind {
                        UnitKind::Tx => (PlanKind::Tx, None),
                        UnitKind::Rebuild => (PlanKind::Rebuild, None),
                        UnitKind::Autocommit => (
                            PlanKind::Autocommit,
                            Some(unit_probe(op, autos).ok_or_else(|| {
                                format!("op {fi}: autocommit unit with no probe")
                            })?),
                        ),
                    };
                    out.push(PlanUnit {
                        op: fi,
                        kind,
                        sha: sha(&u.sql.join(";\n")),
                        sql: u.sql.clone(),
                        probe,
                        batch: None,
                    });
                }
            }
        }
    }
    Ok((out, after))
}

// ---------------------------------------------------------------- decide

/// What resume does with a started autocommit unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Present: the effect is in; mark the unit done.
    Skip,
    /// Absent: run the unit.
    Run,
    /// Only an invalid PG index: drop it CONCURRENTLY, then run the unit.
    Rebuild,
    /// Only not-validated constraints: `VALIDATE` each one.
    Validate(Vec<String>),
    /// Anything else: stop with the differences.
    Refuse(Vec<Mismatch>),
}

fn validity(p: Prop) -> bool {
    matches!(
        p,
        Prop::IndexValid | Prop::CheckValidated | Prop::FkValidated
    )
}

/// The resume decision for `op`'s probe `verdict`.
#[must_use]
pub fn decide(op: &SpikeOp, verdict: &Verdict) -> Action {
    let ms = match verdict {
        Verdict::Present => return Action::Skip,
        Verdict::Absent => return Action::Run,
        Verdict::Partial(ms) => ms,
    };
    let is_drop = matches!(
        op,
        SpikeOp::DropTable { .. }
            | SpikeOp::DropColumn { .. }
            | SpikeOp::DropIndex { .. }
            | SpikeOp::DropCheck { .. }
            | SpikeOp::DropFk { .. }
    );
    if !ms.is_empty() && ms.iter().all(|m| validity(m.prop)) {
        // A killed DROP INDEX CONCURRENTLY leaves the index invalid: drop again.
        if is_drop {
            return Action::Run;
        }
        if ms.iter().all(|m| m.prop == Prop::IndexValid) {
            return Action::Rebuild;
        }
        if ms.iter().all(|m| m.prop != Prop::IndexValid) {
            return Action::Validate(ms.iter().map(|m| m.object.clone()).collect());
        }
    }
    Action::Refuse(ms.clone())
}

// ---------------------------------------------------------------- runner

/// Where a kill hook fires: after the started mark (autocommit), after the
/// statements (inside the tx for tx units and batches), after the commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Point {
    Started,
    Executed,
    Committed,
}

pub type HookFut = Pin<Box<dyn Future<Output = ()> + Send>>;
pub type Hook<'h> = &'h (dyn Fn(&str, usize, Point) -> HookFut + Sync);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub file: String,
    pub unit: usize,
    pub verdict: Verdict,
    pub action: Action,
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub decisions: Vec<Decision>,
    /// Files whose `running` row this run took over.
    pub resumed: Vec<String>,
    /// Sessions of a dead run this run terminated.
    pub reaped: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("migrate lock held by another runner")]
    LockHeld,
    #[error("lock lost: {file} unit {unit} (fenced update matched 0 rows)")]
    LockLost { file: String, unit: usize },
    #[error("{file} unit {unit}: catalog differs from the plan: {diff:?}")]
    Refused {
        file: String,
        unit: usize,
        diff: Vec<Mismatch>,
    },
    #[error("{0}: file changed since it started")]
    Checksum(String),
    #[error("{0}")]
    Exec(String),
}

fn ex(e: impl std::fmt::Display) -> RunError {
    RunError::Exec(e.to_string())
}

pub struct Runner<'a> {
    pub pool: &'a Pool,
    pub scope: LockScope,
    pub hook: Option<Hook<'a>>,
}

/// A ledger row as the resumer needs it.
#[derive(Debug, Clone)]
pub struct Row {
    pub state: String,
    pub checksum: String,
    pub plan: String,
    pub plan_hash: String,
    pub pos: i64,
    pub started: Option<i64>,
    pub session: Option<String>,
    pub effect_hi: Option<i64>,
    pub run_id: String,
}

/// Create the ledger table.
///
/// # Errors
/// Driver errors.
pub async fn ensure_ledger(pool: &Pool) -> Result<(), RunError> {
    let d = pool.dialect();
    let sql = format!(
        "CREATE TABLE IF NOT EXISTS {} (app VARCHAR(100) NOT NULL, id VARCHAR(100) NOT NULL, \
         state VARCHAR(20) NOT NULL, checksum VARCHAR(64) NOT NULL, unit_plan TEXT NOT NULL, \
         unit_plan_hash VARCHAR(64) NOT NULL, pos BIGINT NOT NULL, started_unit BIGINT, \
         session VARCHAR(100), effect_hi BIGINT, run_id VARCHAR(64) NOT NULL, \
         PRIMARY KEY (app, id))",
        d.quote_ident(LEDGER)
    );
    raw_execute_pool(pool, &sql, Vec::new())
        .await
        .map(|_| ())
        .map_err(ex)
}

/// Read `(app, id)`'s ledger row.
///
/// # Errors
/// Driver errors.
pub async fn read_row(pool: &Pool, app: &str, id: &str) -> Result<Option<Row>, RunError> {
    let d = pool.dialect();
    let sql = format!(
        "SELECT state, checksum, unit_plan, unit_plan_hash, pos, started_unit, session, \
         effect_hi, run_id FROM {} WHERE app = {} AND id = {}",
        d.quote_ident(LEDGER),
        d.placeholder(1),
        d.placeholder(2)
    );
    type R = (
        String,
        String,
        String,
        String,
        i64,
        Option<i64>,
        Option<String>,
        Option<i64>,
        String,
    );
    let rows: Vec<R> = raw_query_pool(
        &sql,
        vec![
            SqlValue::from(app.to_owned()),
            SqlValue::from(id.to_owned()),
        ],
        pool,
    )
    .await
    .map_err(ex)?;
    Ok(rows.into_iter().next().map(|r| Row {
        state: r.0,
        checksum: r.1,
        plan: r.2,
        plan_hash: r.3,
        pos: r.4,
        started: r.5,
        session: r.6,
        effect_hi: r.7,
        run_id: r.8,
    }))
}

/// `UPDATE ledger SET .. WHERE app, id, run_id, pos` with binds in text order.
/// A `None` value writes NULL literally (a typed NULL bind differs per driver).
#[must_use]
pub fn fence_sql(
    d: &dyn Dialect,
    sets: &[(&str, Option<SqlValue>)],
    app: &str,
    id: &str,
    run_id: &str,
    pos: i64,
) -> (String, Vec<SqlValue>) {
    let mut binds = Vec::new();
    let mut n = 0;
    let mut ph = |v: SqlValue, binds: &mut Vec<SqlValue>| {
        n += 1;
        binds.push(v);
        d.placeholder(n)
    };
    let set: Vec<String> = sets
        .iter()
        .map(|(c, v)| match v {
            Some(v) => format!("{c} = {}", ph(v.clone(), &mut binds)),
            None => format!("{c} = NULL"),
        })
        .collect();
    let w_app = ph(SqlValue::from(app.to_owned()), &mut binds);
    let w_id = ph(SqlValue::from(id.to_owned()), &mut binds);
    let w_run = ph(SqlValue::from(run_id.to_owned()), &mut binds);
    let w_pos = ph(SqlValue::from(pos), &mut binds);
    (
        format!(
            "UPDATE {} SET {} WHERE app = {w_app} AND id = {w_id} AND run_id = {w_run} AND pos = {w_pos}",
            d.quote_ident(LEDGER),
            set.join(", ")
        ),
        binds,
    )
}

/// A fenced ledger update on the pool (outside any unit tx).
///
/// # Errors
/// `LockLost` when no row matched.
pub async fn fenced(
    pool: &Pool,
    sets: &[(&str, Option<SqlValue>)],
    file: &File,
    run_id: &str,
    pos: i64,
) -> Result<(), RunError> {
    let (sql, binds) = fence_sql(pool.dialect(), sets, &file.app, &file.id, run_id, pos);
    match raw_execute_pool(pool, &sql, binds).await.map_err(ex)? {
        1 => Ok(()),
        _ => Err(RunError::LockLost {
            file: file.id.clone(),
            unit: pos as usize,
        }),
    }
}

/// The DDL session's identity: PG `pid:backend_start` (pids are reused),
/// MySQL `CONNECTION_ID()` (not reused while the server runs).
async fn session_of(c: &mut PoolConn) -> Result<Option<String>, RunError> {
    #[allow(unused_imports)]
    use crate::sql::PooledConn;
    match c.conn() {
        #[cfg(feature = "postgres")]
        PooledConn::Postgres(c) => {
            let (pid, start): (i32, String) = sqlx::query_as(
                "SELECT pid, backend_start::text FROM pg_stat_activity WHERE pid = pg_backend_pid()",
            )
            .fetch_one(&mut **c)
            .await
            .map_err(ex)?;
            Ok(Some(format!("{pid}:{start}")))
        }
        #[cfg(feature = "mysql")]
        PooledConn::Mysql(c) => {
            let id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
                .fetch_one(&mut **c)
                .await
                .map_err(ex)?;
            Ok(Some(id.to_string()))
        }
        #[allow(unreachable_patterns)]
        _ => Ok(None),
    }
}

/// One statement in autocommit on a pinned connection.
async fn exec_on(c: &mut PoolConn, sql: &str) -> Result<(), RunError> {
    use crate::sql::PooledConn;
    let r = match c.conn() {
        #[cfg(feature = "postgres")]
        PooledConn::Postgres(c) => sqlx::query(sql).execute(&mut **c).await.map(|_| ()),
        #[cfg(feature = "mysql")]
        PooledConn::Mysql(c) => sqlx::query(sql).execute(&mut **c).await.map(|_| ()),
        #[cfg(feature = "sqlite")]
        PooledConn::Sqlite(c) => sqlx::query(sql).execute(&mut **c).await.map(|_| ()),
    };
    r.map_err(|e| RunError::Exec(format!("{sql}: {e}")))
}

/// Is a session recorded by a dead run still on the server?
///
/// # Errors
/// Driver errors.
pub async fn session_alive(pool: &Pool, session: &str) -> Result<bool, RunError> {
    let n: Vec<(i64,)> = match pool.dialect().kind() {
        DialectKind::Postgres => {
            let (pid, start) = session.split_once(':').ok_or_else(|| ex("bad session"))?;
            raw_query_pool(
                "SELECT COUNT(*) FROM pg_stat_activity WHERE pid::bigint = $1 AND backend_start::text = $2",
                vec![
                    SqlValue::from(pid.parse::<i64>().map_err(ex)?),
                    SqlValue::from(start.to_owned()),
                ],
                pool,
            )
            .await
        }
        DialectKind::Mysql => {
            raw_query_pool(
                "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE ID = ?",
                vec![SqlValue::from(session.parse::<i64>().map_err(ex)?)],
                pool,
            )
            .await
        }
        _ => return Ok(false),
    }
    .map_err(ex)?;
    Ok(n.first().is_some_and(|r| r.0 > 0))
}

/// Terminate a dead run's DDL session and wait until it is gone, so its
/// statement cannot land after the probe (a killed client does not stop it).
///
/// # Errors
/// Driver errors, or the session still alive after 30 s.
pub async fn reap(pool: &Pool, session: &str) -> Result<bool, RunError> {
    if !session_alive(pool, session).await? {
        return Ok(false);
    }
    match pool.dialect().kind() {
        DialectKind::Postgres => {
            let (pid, start) = session.split_once(':').ok_or_else(|| ex("bad session"))?;
            let _: Vec<(bool,)> = raw_query_pool(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE pid::bigint = $1 AND backend_start::text = $2",
                vec![
                    SqlValue::from(pid.parse::<i64>().map_err(ex)?),
                    SqlValue::from(start.to_owned()),
                ],
                pool,
            )
            .await
            .map_err(ex)?;
        }
        DialectKind::Mysql => {
            // KILL takes no placeholder; the id is an integer we stored.
            let id = session.parse::<u64>().map_err(ex)?;
            if let Err(e) = raw_execute_pool(pool, &format!("KILL {id}"), Vec::new()).await {
                // 1094: already gone between the check and the KILL.
                if !e.to_string().contains("1094") {
                    return Err(ex(e));
                }
            }
        }
        _ => return Ok(false),
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while session_alive(pool, session).await? {
        if tokio::time::Instant::now() > deadline {
            return Err(ex(format!("session {session} still alive after reap")));
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Ok(true)
}

impl Runner<'_> {
    async fn hook(&self, file: &str, unit: usize, p: Point) {
        if let Some(h) = self.hook {
            h(file, unit, p).await;
        }
    }

    /// Apply `files` in order from `state0`, resuming any `running` row.
    ///
    /// # Errors
    /// See [`RunError`].
    pub async fn migrate(&self, files: &[File], state0: &State) -> Result<Outcome, RunError> {
        let held = try_lock(self.pool, &self.scope)
            .await
            .map_err(RunError::Exec)?
            .ok_or(RunError::LockHeld)?;
        let r = self.migrate_locked(files, state0).await;
        held.release().await;
        r
    }

    async fn migrate_locked(&self, files: &[File], state0: &State) -> Result<Outcome, RunError> {
        ensure_ledger(self.pool).await?;
        let d = self.pool.dialect();
        let me = uuid::Uuid::new_v4().to_string();
        let mut out = Outcome::default();
        let mut state = state0.clone();
        for f in files {
            let (fresh, after) = plan(f, &state, d).map_err(RunError::Exec)?;
            let row = read_row(self.pool, &f.app, &f.id).await?;
            let (units, mut pos, mut started, effect_hi) = match row {
                Some(r) if r.state == "applied" => {
                    state = after;
                    continue;
                }
                Some(r) => {
                    if r.checksum != f.checksum() {
                        return Err(RunError::Checksum(f.id.clone()));
                    }
                    if sha(&r.plan) != r.plan_hash {
                        return Err(ex(format!("{}: stored plan hash mismatch", f.id)));
                    }
                    // Lock held, so a `running` row under another run_id is a crash.
                    fenced(
                        self.pool,
                        &[("run_id", Some(SqlValue::from(me.clone())))],
                        f,
                        &r.run_id,
                        r.pos,
                    )
                    .await?;
                    out.resumed.push(f.id.clone());
                    if let (Some(_), Some(s)) = (r.started, &r.session) {
                        if reap(self.pool, s).await? {
                            out.reaped.push(s.clone());
                        }
                    }
                    let units: Vec<PlanUnit> = serde_json::from_str(&r.plan).map_err(ex)?;
                    (units, r.pos, r.started, r.effect_hi)
                }
                None => {
                    let json = serde_json::to_string(&fresh).map_err(ex)?;
                    let sql = format!(
                        "INSERT INTO {} (app, id, state, checksum, unit_plan, unit_plan_hash, pos, run_id) \
                         VALUES ({}, {}, 'running', {}, {}, {}, 0, {})",
                        d.quote_ident(LEDGER),
                        d.placeholder(1),
                        d.placeholder(2),
                        d.placeholder(3),
                        d.placeholder(4),
                        d.placeholder(5),
                        d.placeholder(6)
                    );
                    raw_execute_pool(
                        self.pool,
                        &sql,
                        vec![
                            SqlValue::from(f.app.clone()),
                            SqlValue::from(f.id.clone()),
                            SqlValue::from(f.checksum()),
                            SqlValue::from(json.clone()),
                            SqlValue::from(sha(&json)),
                            SqlValue::from(me.clone()),
                        ],
                    )
                    .await
                    .map_err(ex)?;
                    (fresh, 0, None, None)
                }
            };
            let mut hi = effect_hi;
            while (pos as usize) < units.len() {
                let k = pos as usize;
                let u = &units[k];
                match u.kind {
                    PlanKind::Autocommit => {
                        self.autocommit(f, &me, k, u, started == Some(pos), &mut out)
                            .await?;
                    }
                    PlanKind::Tx | PlanKind::Rebuild => self.tx_unit(f, &me, k, u).await?,
                    PlanKind::Batched => self.batched(f, &me, k, u, hi).await?,
                }
                pos += 1;
                started = None;
                hi = None;
            }
            fenced(
                self.pool,
                &[("state", Some(SqlValue::from("applied".to_owned())))],
                f,
                &me,
                pos,
            )
            .await?;
            state = after;
        }
        Ok(out)
    }

    async fn autocommit(
        &self,
        f: &File,
        me: &str,
        k: usize,
        u: &PlanUnit,
        was_started: bool,
        out: &mut Outcome,
    ) -> Result<(), RunError> {
        let pos = k as i64;
        let mut action = Action::Run;
        if was_started {
            let op = u
                .probe
                .as_ref()
                .ok_or_else(|| ex("autocommit unit with no probe"))?;
            let verdict = probe(self.pool, op).await.map_err(ex)?.verdict;
            action = decide(op, &verdict);
            out.decisions.push(Decision {
                file: f.id.clone(),
                unit: k,
                verdict,
                action: action.clone(),
            });
        }
        let stmts: Vec<String> = match &action {
            Action::Skip => Vec::new(),
            Action::Run => u.sql.clone(),
            Action::Rebuild => {
                let Some(SpikeOp::AddIndex(ix)) = &u.probe else {
                    return Err(ex("rebuild without an index probe"));
                };
                let d = self.pool.dialect();
                let mut s = vec![format!(
                    "DROP INDEX CONCURRENTLY {}",
                    d.quote_ident(&ix.name)
                )];
                s.extend(u.sql.iter().cloned());
                s
            }
            Action::Validate(names) => {
                let table = match &u.probe {
                    Some(SpikeOp::AddCheck { table, .. } | SpikeOp::AddFk { table, .. }) => table,
                    _ => return Err(ex("validate without a constraint probe")),
                };
                let d = self.pool.dialect();
                names
                    .iter()
                    .map(|n| {
                        format!(
                            "ALTER TABLE {} VALIDATE CONSTRAINT {}",
                            d.quote_ident(table),
                            d.quote_ident(n)
                        )
                    })
                    .collect()
            }
            Action::Refuse(diff) => {
                return Err(RunError::Refused {
                    file: f.id.clone(),
                    unit: k,
                    diff: diff.clone(),
                })
            }
        };
        if !stmts.is_empty() {
            let mut conn = self.pool.acquire().await.map_err(ex)?;
            let session = session_of(&mut conn).await?;
            let mut sets = vec![("started_unit", Some(SqlValue::from(pos)))];
            sets.push(("session", session.map(SqlValue::from)));
            fenced(self.pool, &sets, f, me, pos).await?;
            self.hook(&f.id, k, Point::Started).await;
            for s in &stmts {
                exec_on(&mut conn, s).await?;
            }
            self.hook(&f.id, k, Point::Executed).await;
        }
        fenced(
            self.pool,
            &[
                ("pos", Some(SqlValue::from(pos + 1))),
                ("started_unit", None),
                ("session", None),
            ],
            f,
            me,
            pos,
        )
        .await?;
        self.hook(&f.id, k, Point::Committed).await;
        Ok(())
    }

    async fn tx_unit(&self, f: &File, me: &str, k: usize, u: &PlanUnit) -> Result<(), RunError> {
        let pos = k as i64;
        let (fence, binds) = fence_sql(
            self.pool.dialect(),
            &[("pos", Some(SqlValue::from(pos + 1)))],
            &f.app,
            &f.id,
            me,
            pos,
        );
        let n = if u.kind == PlanKind::Rebuild {
            self.rebuild_unit(f, k, u, &fence, &binds).await?
        } else {
            let mut conn = self.pool.acquire().await.map_err(ex)?;
            let mut tx = conn.begin().await.map_err(ex)?;
            for s in &u.sql {
                raw_execute_tx(&mut tx, s, Vec::new())
                    .await
                    .map_err(|e| RunError::Exec(format!("{s}: {e}")))?;
            }
            self.hook(&f.id, k, Point::Executed).await;
            let n = raw_execute_tx(&mut tx, &fence, binds).await.map_err(ex)?;
            if n == 1 {
                tx.commit().await.map_err(ex)?;
            }
            n
        };
        if n != 1 {
            return Err(RunError::LockLost {
                file: f.id.clone(),
                unit: k,
            });
        }
        self.hook(&f.id, k, Point::Committed).await;
        Ok(())
    }

    /// SQLite rebuild with the ledger update inside the rebuild tx (6.1 rule 4).
    #[cfg(feature = "sqlite")]
    async fn rebuild_unit(
        &self,
        f: &File,
        k: usize,
        u: &PlanUnit,
        fence: &str,
        binds: &[SqlValue],
    ) -> Result<u64, RunError> {
        #[allow(irrefutable_let_patterns)]
        let Pool::Sqlite(sq) = self.pool
        else {
            return Err(ex("a rebuild unit is SQLite only"));
        };
        let mut conn = crate::migrate::rebuild::RebuildConn::acquire(sq, true)
            .await
            .map_err(ex)?;
        let r: Result<u64, RunError> = async {
            let mut tx = conn.begin().await.map_err(ex)?;
            for s in &u.sql {
                sqlx::query(s)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| RunError::Exec(format!("{s}: {e}")))?;
            }
            let orphans: Vec<(String,)> =
                sqlx::query_as("SELECT \"table\" FROM pragma_foreign_key_check")
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(ex)?;
            if !orphans.is_empty() {
                return Err(ex(format!("rebuild left FK orphans in {orphans:?}")));
            }
            self.hook(&f.id, k, Point::Executed).await;
            let mut q = sqlx::query(fence);
            for b in binds {
                q = match b {
                    SqlValue::I64(v) => q.bind(*v),
                    SqlValue::String(v) => q.bind(v.clone()),
                    other => return Err(ex(format!("unexpected bind {other:?}"))),
                };
            }
            let n = q.execute(&mut *tx).await.map_err(ex)?.rows_affected();
            if n == 1 {
                tx.commit().await.map_err(ex)?;
            }
            Ok(n)
        }
        .await;
        let finished = conn.finish().await.map_err(ex);
        let n = r?;
        finished?;
        Ok(n)
    }

    #[cfg(not(feature = "sqlite"))]
    async fn rebuild_unit(
        &self,
        _f: &File,
        _k: usize,
        _u: &PlanUnit,
        _fence: &str,
        _binds: &[SqlValue],
    ) -> Result<u64, RunError> {
        Err(ex("a rebuild unit is SQLite only"))
    }

    /// Keyset batches over `(effect_hi, hi]`; progress commits with each batch.
    async fn batched(
        &self,
        f: &File,
        me: &str,
        k: usize,
        u: &PlanUnit,
        effect_hi: Option<i64>,
    ) -> Result<(), RunError> {
        let d = self.pool.dialect();
        let b = u
            .batch
            .as_ref()
            .ok_or_else(|| ex("batched unit without batch"))?;
        let (t, key) = (d.quote_ident(&b.table), d.quote_ident(&b.key));
        let pos = k as i64;
        let mut lo = effect_hi.unwrap_or(i64::MIN);
        loop {
            let nth = format!(
                "SELECT {key} FROM {t} WHERE {key} > {} ORDER BY {key} LIMIT 1 OFFSET {}",
                d.placeholder(1),
                b.size - 1
            );
            let mut hi: Option<i64> =
                raw_query_pool::<(i64,)>(&nth, vec![SqlValue::from(lo)], self.pool)
                    .await
                    .map_err(ex)?
                    .first()
                    .map(|r| r.0);
            if hi.is_none() {
                let max = format!(
                    "SELECT MAX({key}) FROM {t} WHERE {key} > {}",
                    d.placeholder(1)
                );
                hi = raw_query_pool::<(Option<i64>,)>(&max, vec![SqlValue::from(lo)], self.pool)
                    .await
                    .map_err(ex)?
                    .first()
                    .and_then(|r| r.0);
            }
            let mut conn = self.pool.acquire().await.map_err(ex)?;
            let mut tx = conn.begin().await.map_err(ex)?;
            let sets: Vec<(&str, Option<SqlValue>)> = match hi {
                Some(h) => {
                    if h <= lo {
                        return Err(ex("batch cursor did not advance"));
                    }
                    raw_execute_tx(
                        &mut tx,
                        &u.sql[0],
                        vec![SqlValue::from(lo), SqlValue::from(h)],
                    )
                    .await
                    .map_err(ex)?;
                    self.hook(&f.id, k, Point::Executed).await;
                    vec![("effect_hi", Some(SqlValue::from(h)))]
                }
                None => vec![("pos", Some(SqlValue::from(pos + 1))), ("effect_hi", None)],
            };
            let (fence, binds) = fence_sql(d, &sets, &f.app, &f.id, me, pos);
            if raw_execute_tx(&mut tx, &fence, binds).await.map_err(ex)? != 1 {
                return Err(RunError::LockLost {
                    file: f.id.clone(),
                    unit: k,
                });
            }
            tx.commit().await.map_err(ex)?;
            match hi {
                Some(h) => {
                    lo = h;
                    self.hook(&f.id, k, Point::Committed).await;
                }
                None => return Ok(()),
            }
        }
    }
}
