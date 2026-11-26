//! The v2 ledger (6.2) and its events table. The ledger tables are v2
//! tables themselves: their DDL comes from the renderer, so the column
//! types follow the one type mapping.

use super::render::{create_table_sql_for, Unit};
use super::state::{ColumnDef, TableState, Target};
use crate::core::SqlValue;
use crate::migrate::MigrateError;
use crate::sql::{raw_execute_pool, raw_query_pool, Pool};

/// Where a migration stands in the ledger. Pending = no row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowState {
    Running,
    Applied,
    Failed,
    RollingBack,
}

impl RowState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Applied => "applied",
            Self::Failed => "failed",
            Self::RollingBack => "rolling_back",
        }
    }

    fn parse(s: &str) -> Result<Self, MigrateError> {
        Ok(match s {
            "running" | "partial" => Self::Running,
            "applied" => Self::Applied,
            "failed" => Self::Failed,
            "rolling_back" => Self::RollingBack,
            other => {
                return Err(MigrateError::Validation(format!(
                    "the v2 ledger holds an unknown state `{other}`"
                )))
            }
        })
    }
}

/// One ledger row, as the runner reads it.
#[derive(Debug, Clone)]
pub(crate) struct Row {
    pub(crate) app: String,
    pub(crate) id: String,
    pub(crate) checksum: String,
    pub(crate) applied_body: String,
    pub(crate) state: RowState,
    pub(crate) unit_plan: Vec<Unit>,
    pub(crate) pos: i64,
    pub(crate) started_unit: Option<i64>,
    pub(crate) run_id: String,
    /// Why the row failed (class only); `stopped` for a planned stop.
    pub(crate) error_class: Option<String>,
    /// The DDL session of `started_unit`, reaped before its probe (6.5).
    pub(crate) session: Option<super::session::Session>,
}

/// Which way a batched unit walks its key range (6.2 `direction`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Walk {
    Up,
    Down,
}

/// The ledger's table names, from the configured base.
#[derive(Debug, Clone)]
pub(crate) struct Ledger {
    pub(crate) table: String,
    pub(crate) events: String,
    /// Batch progress per `(app, id, op_index)` (6.2).
    pub(crate) progress: String,
    /// Undo rows of data steps that capture them (7.4).
    pub(crate) undo: String,
}

impl Ledger {
    /// The default ledger: `__rustango_migrations_v2` and `_v2_events`.
    pub(crate) fn default_names() -> Self {
        Self::for_base(crate::migrate::LEDGER_TABLE)
    }

    /// The v2 ledger of the v1 ledger name `base`: one per app sharing a database.
    pub(crate) fn for_base(base: &str) -> Self {
        let table = crate::migrate::v2_ledger(base);
        Self {
            events: format!("{table}_events"),
            progress: format!("{table}_progress"),
            undo: format!("{table}_undo"),
            table,
        }
    }

    /// The ledger `opts` names, else the default.
    pub(crate) fn of(opts: &super::run::Options) -> Self {
        opts.ledger
            .as_deref()
            .map_or_else(Self::default_names, Self::for_base)
    }
}

fn col(name: &str, ty: &str, nullable: bool, max_length: Option<u32>) -> ColumnDef {
    ColumnDef {
        column: name.into(),
        ty: ty.into(),
        nullable,
        max_length,
        primary_key: false,
        auto: false,
        default: None,
        generated_as: None,
        comment: None,
        case_insensitive: false,
        sealed: false,
    }
}

fn table(name: &str, columns: Vec<ColumnDef>) -> TableState {
    TableState {
        name: name.into(),
        scope: Target::Tenant,
        columns,
        indexes: Vec::new(),
        checks: Vec::new(),
        fks: Vec::new(),
        comment: None,
        pg_pkey: None,
        pg_sequence: None,
    }
}

fn ledger_table(name: &str) -> TableState {
    let mut app = col("app", "string", false, Some(100));
    app.primary_key = true;
    let mut id = col("id", "string", false, Some(200));
    id.primary_key = true;
    let mut adopted = col("adopted_from_legacy", "bool", false, None);
    adopted.default = Some("FALSE".into());
    table(
        name,
        vec![
            app,
            id,
            col("seq", "i64", false, None),
            col("checksum", "string", false, Some(64)),
            col("checksum_alg", "i32", false, None),
            col("applied_body", "string", false, None),
            col("state", "string", false, Some(16)),
            col("unit_plan", "string", false, None),
            col("unit_plan_hash", "string", false, Some(64)),
            col("engine_version", "string", false, Some(32)),
            col("pos", "i64", false, None),
            col("started_unit", "i64", true, None),
            col("session", "string", true, None),
            col("run_id", "string", false, Some(64)),
            col("heartbeat_at", "datetime", true, None),
            col("started_at", "datetime", true, None),
            col("finished_at", "datetime", true, None),
            col("state_before_hash", "string", true, Some(64)),
            col("state_after_hash", "string", true, Some(64)),
            col("step_hashes", "string", true, None),
            col("error_class", "string", true, None),
            col("undo_pruned_at", "datetime", true, None),
            adopted,
        ],
    )
}

fn progress_table(name: &str) -> TableState {
    let mut app = col("app", "string", false, Some(100));
    app.primary_key = true;
    let mut id = col("id", "string", false, Some(200));
    id.primary_key = true;
    let mut op = col("op_index", "i32", false, None);
    op.primary_key = true;
    table(
        name,
        vec![
            app,
            id,
            op,
            // The cursor as JSON, typed by the key's `ty`.
            col("effect_hi", "string", true, None),
            col("done_hi", "string", true, None),
            col("rows_done", "i64", false, None),
            col("batches", "i64", false, None),
            col("direction", "string", false, Some(8)),
            col("overrides", "string", true, None),
            col("updated_at", "datetime", true, None),
        ],
    )
}

fn undo_table(name: &str) -> TableState {
    let mut id = col("id", "i64", false, None);
    id.primary_key = true;
    id.auto = true;
    table(
        name,
        vec![
            id,
            col("app", "string", false, Some(100)),
            col("mig", "string", false, Some(200)),
            col("op_index", "i32", false, None),
            // insert | update | delete
            col("kind", "string", false, Some(8)),
            col("tbl", "string", false, Some(63)),
            col("key_col", "string", false, Some(63)),
            // The key as JSON, decoded by the key column's type.
            col("key_json", "string", false, None),
            col("key_hash", "string", false, Some(64)),
            col("col", "string", true, Some(63)),
            col("old_json", "string", true, None),
            // The written value's canonical hash: the restore skips a row
            // whose value no longer matches (the user edited it).
            col("new_hash", "string", true, Some(64)),
            col("row_json", "string", true, None),
            col("at", "datetime", true, None),
        ],
    )
}

/// One captured change, as the restore reads it.
#[derive(Debug, Clone)]
pub(crate) struct UndoRow {
    pub(crate) id: i64,
    pub(crate) kind: String,
    pub(crate) table: String,
    pub(crate) key_col: String,
    pub(crate) key_json: String,
    pub(crate) col: Option<String>,
    pub(crate) old_json: Option<String>,
    pub(crate) new_hash: Option<String>,
    pub(crate) row_json: Option<String>,
}

fn events_table(name: &str) -> TableState {
    let mut id = col("id", "i64", false, None);
    id.primary_key = true;
    id.auto = true;
    table(
        name,
        vec![
            id,
            col("kind", "string", false, Some(32)),
            col("app", "string", false, Some(100)),
            col("mig", "string", false, Some(200)),
            col("checksum", "string", true, Some(64)),
            col("run_id", "string", false, Some(64)),
            col("os_user", "string", true, Some(100)),
            col("host", "string", true, Some(200)),
            col("app_version", "string", true, Some(32)),
            col("reason", "string", true, None),
            col("at", "datetime", true, None),
        ],
    )
}

fn p(pool: &Pool, n: usize) -> String {
    pool.dialect().placeholder(n)
}

fn s(v: impl Into<String>) -> SqlValue {
    SqlValue::String(v.into())
}

impl Ledger {
    /// The ledger table's name, quoted: a configured base may need it.
    fn qt(&self, pool: &Pool) -> String {
        pool.dialect().quote_ident(&self.table)
    }

    /// The events table's name, quoted.
    fn qe(&self, pool: &Pool) -> String {
        pool.dialect().quote_ident(&self.events)
    }

    /// Create the ledger and events tables if absent (the caller holds the lock).
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn ensure(&self, pool: &Pool) -> Result<(), MigrateError> {
        let d = pool.dialect();
        for t in [
            ledger_table(&self.table),
            events_table(&self.events),
            progress_table(&self.progress),
            undo_table(&self.undo),
        ] {
            if !crate::migrate::try_table_exists_here(pool, &t.name).await? {
                raw_execute_pool(pool, &create_table_sql_for(d, &t), Vec::new()).await?;
                // The restore finds a key's captured changes by its hash (7.4).
                if t.name == self.undo {
                    let ix = format!(
                        "CREATE INDEX {} ON {} (key_hash)",
                        d.quote_ident(&format!("{}_kh", self.undo)),
                        d.quote_ident(&self.undo)
                    );
                    raw_execute_pool(pool, &ix, Vec::new()).await?;
                }
            }
        }
        Ok(())
    }

    /// Every row.
    ///
    /// # Errors
    /// Driver failures, or a row that does not decode.
    pub(crate) async fn rows(&self, pool: &Pool) -> Result<Vec<Row>, MigrateError> {
        if !crate::migrate::try_table_exists_here(pool, &self.table).await? {
            return Ok(Vec::new());
        }
        type Raw = (
            String,
            String,
            String,
            String,
            String,
            String,
            i64,
            Option<i64>,
            String,
            Option<String>,
            Option<String>,
        );
        let sql = format!(
            "SELECT app, id, checksum, applied_body, state, unit_plan, pos, started_unit, run_id, \
             error_class, session FROM {} ORDER BY seq",
            self.qt(pool)
        );
        let raw: Vec<Raw> = raw_query_pool(&sql, Vec::new(), pool).await?;
        raw.into_iter()
            .map(
                |(
                    app,
                    id,
                    checksum,
                    applied_body,
                    state,
                    plan,
                    pos,
                    started_unit,
                    run_id,
                    error_class,
                    session,
                )| {
                    Ok(Row {
                        unit_plan: serde_json::from_str(&plan)?,
                        state: RowState::parse(&state)?,
                        app,
                        id,
                        checksum,
                        applied_body,
                        pos,
                        started_unit,
                        run_id,
                        error_class,
                        session: session.as_deref().map(serde_json::from_str).transpose()?,
                    })
                },
            )
            .collect()
    }

    /// Insert a `running` row (or `rolling_back` for a down plan's resume
    /// marker), position before the first unit.
    ///
    /// # Errors
    /// Driver failures.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn insert(
        &self,
        pool: &Pool,
        key: (&str, &str),
        checksum: &str,
        body: &str,
        state: RowState,
        plan: &[Unit],
        run_id: &str,
        hashes: (&str, Option<&str>),
        adopted: bool,
    ) -> Result<(), MigrateError> {
        let next: Vec<(i64,)> = raw_query_pool(
            &format!("SELECT COALESCE(MAX(seq), 0) FROM {}", self.qt(pool)),
            Vec::new(),
            pool,
        )
        .await?;
        let seq = next.first().map_or(0, |r| r.0) + 1;
        let plan_json = serde_json::to_string(plan)?;
        let plan_hash = super::checksum::hex(&<sha2::Sha256 as sha2::Digest>::digest(
            plan_json.as_bytes(),
        ));
        let now = pool.dialect().utc_now_sql();
        let sql = format!(
            "INSERT INTO {} (app, id, seq, checksum, checksum_alg, applied_body, state, unit_plan, \
             unit_plan_hash, engine_version, pos, run_id, heartbeat_at, started_at, \
             state_before_hash, state_after_hash, adopted_from_legacy) VALUES ({}, {}, {}, {}, {}, \
             {}, {}, {}, {}, {}, -1, {}, {now}, {now}, {}, {}, {})",
            self.qt(pool),
            p(pool, 1),
            p(pool, 2),
            p(pool, 3),
            p(pool, 4),
            p(pool, 5),
            p(pool, 6),
            p(pool, 7),
            p(pool, 8),
            p(pool, 9),
            p(pool, 10),
            p(pool, 11),
            p(pool, 12),
            p(pool, 13),
            p(pool, 14),
        );
        raw_execute_pool(
            pool,
            &sql,
            vec![
                s(key.0),
                s(key.1),
                SqlValue::I64(seq),
                s(checksum),
                SqlValue::I32(super::checksum::CHECKSUM_ALG as i32),
                s(body),
                s(state.as_str()),
                s(plan_json),
                s(plan_hash),
                s(env!("CARGO_PKG_VERSION")),
                s(run_id),
                s(hashes.0),
                hashes.1.map_or(SqlValue::Null, s),
                SqlValue::Bool(adopted),
            ],
        )
        .await?;
        Ok(())
    }

    /// Take over a crashed run's row: fenced on its old `run_id`.
    ///
    /// # Errors
    /// Driver failures, or the row moved meanwhile.
    pub(crate) async fn claim(
        &self,
        pool: &Pool,
        key: (&str, &str),
        old: &str,
        new: &str,
    ) -> Result<(), MigrateError> {
        let sql = format!(
            "UPDATE {} SET run_id = {}, heartbeat_at = {} WHERE app = {} AND id = {} AND run_id = {}",
            self.qt(pool),
            p(pool, 1),
            pool.dialect().utc_now_sql(),
            p(pool, 2),
            p(pool, 3),
            p(pool, 4),
        );
        let n = raw_execute_pool(pool, &sql, vec![s(new), s(key.0), s(key.1), s(old)]).await?;
        fenced(n)
    }

    /// The fenced position update, with its binds: `pos = to` where the row
    /// is ours and still at `from`. Run inside a unit's transaction, or alone.
    pub(crate) fn advance(
        &self,
        pool: &Pool,
        key: (&str, &str),
        run_id: &str,
        from: i64,
        to: i64,
    ) -> (String, Vec<SqlValue>) {
        let sql = format!(
            "UPDATE {} SET pos = {}, started_unit = NULL, session = NULL, heartbeat_at = {} \
             WHERE app = {} AND id = {} AND run_id = {} AND pos = {}",
            self.qt(pool),
            p(pool, 1),
            pool.dialect().utc_now_sql(),
            p(pool, 2),
            p(pool, 3),
            p(pool, 4),
            p(pool, 5),
        );
        (
            sql,
            vec![
                SqlValue::I64(to),
                s(key.0),
                s(key.1),
                s(run_id),
                SqlValue::I64(from),
            ],
        )
    }

    /// Refresh the heartbeat of every row this run holds.
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn touch(&self, pool: &Pool, run_id: &str) -> Result<(), MigrateError> {
        let sql = format!(
            "UPDATE {} SET heartbeat_at = {} WHERE run_id = {} AND state IN ({}, {})",
            self.qt(pool),
            pool.dialect().utc_now_sql(),
            p(pool, 1),
            p(pool, 2),
            p(pool, 3),
        );
        raw_execute_pool(
            pool,
            &sql,
            vec![
                s(run_id),
                s(RowState::Running.as_str()),
                s(RowState::RollingBack.as_str()),
            ],
        )
        .await?;
        Ok(())
    }

    /// A `running` row whose heartbeat is older than `stale`, by the DB clock:
    /// `(app, id, age in seconds)`.
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn stale_running(
        &self,
        pool: &Pool,
        stale: std::time::Duration,
    ) -> Result<Option<(String, String, i64)>, MigrateError> {
        let age = match pool.dialect().kind() {
            crate::sql::DialectKind::Postgres => {
                "CAST(EXTRACT(EPOCH FROM (now() - heartbeat_at)) AS BIGINT)"
            }
            crate::sql::DialectKind::Mysql => {
                "TIMESTAMPDIFF(SECOND, heartbeat_at, UTC_TIMESTAMP(6))"
            }
            _ => "CAST((julianday('now') - julianday(heartbeat_at)) * 86400 AS INTEGER)",
        };
        let sql = format!(
            "SELECT app, id, {age} FROM {} WHERE state = {} AND heartbeat_at IS NOT NULL",
            self.qt(pool),
            p(pool, 1),
        );
        let rows: Vec<(String, String, i64)> =
            raw_query_pool(&sql, vec![s(RowState::Running.as_str())], pool).await?;
        let limit = i64::try_from(stale.as_secs()).unwrap_or(i64::MAX);
        Ok(rows.into_iter().find(|r| r.2 >= limit))
    }

    /// Mark autocommit unit `unit` as started (fenced on `pos = unit - 1`).
    ///
    /// # Errors
    /// Driver failures, or the lock was lost.
    pub(crate) async fn start_unit(
        &self,
        pool: &Pool,
        key: (&str, &str),
        run_id: &str,
        unit: i64,
        session: Option<&super::session::Session>,
    ) -> Result<(), MigrateError> {
        let sql = format!(
            "UPDATE {} SET started_unit = {}, session = {}, heartbeat_at = {} \
             WHERE app = {} AND id = {} AND run_id = {} AND pos = {}",
            self.qt(pool),
            p(pool, 1),
            p(pool, 2),
            pool.dialect().utc_now_sql(),
            p(pool, 3),
            p(pool, 4),
            p(pool, 5),
            p(pool, 6),
        );
        let session = match session {
            Some(x) => s(&serde_json::to_string(x)?),
            None => SqlValue::Null,
        };
        let n = raw_execute_pool(
            pool,
            &sql,
            vec![
                SqlValue::I64(unit),
                session,
                s(key.0),
                s(key.1),
                s(run_id),
                SqlValue::I64(unit - 1),
            ],
        )
        .await?;
        fenced(n)
    }

    /// Set the row's state (and, when given, its after-hash), fenced.
    ///
    /// # Errors
    /// Driver failures, or the lock was lost.
    pub(crate) async fn set_state(
        &self,
        pool: &Pool,
        key: (&str, &str),
        run_id: &str,
        state: RowState,
        error_class: Option<&str>,
    ) -> Result<(), MigrateError> {
        let finished = if state == RowState::Applied {
            format!(", finished_at = {}", pool.dialect().utc_now_sql())
        } else {
            String::new()
        };
        let sql = format!(
            "UPDATE {} SET state = {}, error_class = {}{finished} WHERE app = {} AND id = {} AND run_id = {}",
            self.qt(pool),
            p(pool, 1),
            p(pool, 2),
            p(pool, 3),
            p(pool, 4),
            p(pool, 5),
        );
        let n = raw_execute_pool(
            pool,
            &sql,
            vec![
                s(state.as_str()),
                error_class.map_or(SqlValue::Null, s),
                s(key.0),
                s(key.1),
                s(run_id),
            ],
        )
        .await?;
        fenced(n)
    }

    /// Replace a row's plan for a rollback: `rolling_back`, position reset.
    ///
    /// # Errors
    /// Driver failures, or the lock was lost.
    pub(crate) async fn begin_rollback(
        &self,
        pool: &Pool,
        key: (&str, &str),
        old_run: &str,
        run_id: &str,
        plan: &[Unit],
    ) -> Result<(), MigrateError> {
        self.replan(
            pool,
            key,
            (old_run, run_id),
            RowState::RollingBack,
            plan,
            -1,
        )
        .await
    }

    /// Take the row from `runs.0` for `runs.1` with a new plan, state and
    /// position (fenced): a rollback begins, or goes forward again.
    ///
    /// # Errors
    /// Driver failures, or the lock was lost.
    pub(crate) async fn replan(
        &self,
        pool: &Pool,
        key: (&str, &str),
        runs: (&str, &str),
        state: RowState,
        plan: &[Unit],
        pos: i64,
    ) -> Result<(), MigrateError> {
        let sql = format!(
            "UPDATE {} SET state = {}, unit_plan = {}, pos = {}, started_unit = NULL, \
             session = NULL, run_id = {} WHERE app = {} AND id = {} AND run_id = {}",
            self.qt(pool),
            p(pool, 1),
            p(pool, 2),
            p(pool, 3),
            p(pool, 4),
            p(pool, 5),
            p(pool, 6),
            p(pool, 7),
        );
        let n = raw_execute_pool(
            pool,
            &sql,
            vec![
                s(state.as_str()),
                s(serde_json::to_string(plan)?),
                SqlValue::I64(pos),
                s(runs.1),
                s(key.0),
                s(key.1),
                s(runs.0),
            ],
        )
        .await?;
        fenced(n)
    }

    /// Delete a rolled-back row (fenced) and record the event.
    ///
    /// # Errors
    /// Driver failures, or the lock was lost.
    pub(crate) async fn remove(
        &self,
        pool: &Pool,
        key: (&str, &str),
        run_id: &str,
    ) -> Result<(), MigrateError> {
        let sql = format!(
            "DELETE FROM {} WHERE app = {} AND id = {} AND run_id = {}",
            self.qt(pool),
            p(pool, 1),
            p(pool, 2),
            p(pool, 3),
        );
        let n = raw_execute_pool(pool, &sql, vec![s(key.0), s(key.1), s(run_id)]).await?;
        fenced(n)?;
        if crate::migrate::try_table_exists_here(pool, &self.progress).await? {
            let del = format!(
                "DELETE FROM {} WHERE app = {} AND id = {}",
                self.qp(pool),
                p(pool, 1),
                p(pool, 2),
            );
            raw_execute_pool(pool, &del, vec![s(key.0), s(key.1)]).await?;
        }
        if crate::migrate::try_table_exists_here(pool, &self.undo).await? {
            let del = format!(
                "DELETE FROM {} WHERE app = {} AND mig = {}",
                pool.dialect().quote_ident(&self.undo),
                p(pool, 1),
                p(pool, 2),
            );
            raw_execute_pool(pool, &del, vec![s(key.0), s(key.1)]).await?;
        }
        Ok(())
    }

    /// The INSERT of one undo row; binds: app, mig, op_index, kind, tbl,
    /// key_col, key_json, key_hash, col, old_json, new_hash, row_json.
    pub(crate) fn undo_record_sql(&self, d: &dyn crate::sql::Dialect) -> String {
        let marks: Vec<String> = (1..=12).map(|n| d.placeholder(n)).collect();
        format!(
            "INSERT INTO {} (app, mig, op_index, kind, tbl, key_col, key_json, key_hash, col, \
             old_json, new_hash, row_json, at) VALUES ({}, {})",
            d.quote_ident(&self.undo),
            marks.join(", "),
            d.utc_now_sql()
        )
    }

    /// Up to `limit` undo rows of forward unit `op`, newest first.
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn undo_rows_tx(
        &self,
        tx: &mut crate::sql::PoolTx<'_>,
        key: (&str, &str),
        op: usize,
        limit: u32,
    ) -> Result<Vec<UndoRow>, MigrateError> {
        let d = tx.dialect();
        let sql = format!(
            "SELECT id, kind, tbl, key_col, key_json, col, old_json, new_hash, row_json FROM {} \
             WHERE app = {} AND mig = {} AND op_index = {} ORDER BY id DESC LIMIT {limit}",
            d.quote_ident(&self.undo),
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
        );
        type Raw = (
            i64,
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        let op = i32::try_from(op).unwrap_or(i32::MAX);
        let rows: Vec<Raw> =
            crate::sql::raw_query_tx(tx, &sql, vec![s(key.0), s(key.1), SqlValue::I32(op)]).await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, kind, table, key_col, key_json, col, old_json, new_hash, row_json)| UndoRow {
                    id,
                    kind,
                    table,
                    key_col,
                    key_json,
                    col,
                    old_json,
                    new_hash,
                    row_json,
                },
            )
            .collect())
    }

    /// Delete restored undo rows, in the restore's transaction.
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn undo_delete_tx(
        &self,
        tx: &mut crate::sql::PoolTx<'_>,
        ids: &[i64],
    ) -> Result<(), MigrateError> {
        if ids.is_empty() {
            return Ok(());
        }
        let d = tx.dialect();
        let marks: Vec<String> = (1..=ids.len()).map(|n| d.placeholder(n)).collect();
        let sql = format!(
            "DELETE FROM {} WHERE id IN ({})",
            d.quote_ident(&self.undo),
            marks.join(", ")
        );
        crate::sql::raw_execute_tx(tx, &sql, ids.iter().map(|i| SqlValue::I64(*i)).collect())
            .await?;
        Ok(())
    }

    fn qp(&self, pool: &Pool) -> String {
        pool.dialect().quote_ident(&self.progress)
    }

    /// A batched op's progress: `(effect_hi as JSON, rows_done, batches)`.
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn progress_tx(
        &self,
        tx: &mut crate::sql::PoolTx<'_>,
        key: (&str, &str),
        op: usize,
    ) -> Result<Option<(Option<String>, i64, i64)>, MigrateError> {
        let d = tx.dialect();
        let sql = format!(
            "SELECT effect_hi, rows_done, batches FROM {} WHERE app = {} AND id = {} AND op_index = {}",
            d.quote_ident(&self.progress),
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
        );
        let op = i32::try_from(op).unwrap_or(i32::MAX);
        let rows: Vec<(Option<String>, i64, i64)> =
            crate::sql::raw_query_tx(tx, &sql, vec![s(key.0), s(key.1), SqlValue::I32(op)]).await?;
        Ok(rows.into_iter().next())
    }

    /// Replace a batched op's progress inside the batch's transaction.
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn put_progress_tx(
        &self,
        tx: &mut crate::sql::PoolTx<'_>,
        key: (&str, &str),
        op: usize,
        effect_hi: Option<&str>,
        rows_done: i64,
        batches: i64,
        walk: Walk,
    ) -> Result<(), MigrateError> {
        let d = tx.dialect();
        let op = i32::try_from(op).unwrap_or(i32::MAX);
        if walk == Walk::Down {
            // `done_hi` keeps the forward high-water; `effect_hi` shrinks.
            let up = format!(
                "UPDATE {} SET effect_hi = {}, rows_done = {}, batches = {}, direction = {}, \
                 updated_at = {} WHERE app = {} AND id = {} AND op_index = {}",
                d.quote_ident(&self.progress),
                d.placeholder(1),
                d.placeholder(2),
                d.placeholder(3),
                d.placeholder(4),
                d.utc_now_sql(),
                d.placeholder(5),
                d.placeholder(6),
                d.placeholder(7),
            );
            crate::sql::raw_execute_tx(
                tx,
                &up,
                vec![
                    effect_hi.map_or(SqlValue::Null, s),
                    SqlValue::I64(rows_done),
                    SqlValue::I64(batches),
                    s("down"),
                    s(key.0),
                    s(key.1),
                    SqlValue::I32(op),
                ],
            )
            .await?;
            return Ok(());
        }
        let del = format!(
            "DELETE FROM {} WHERE app = {} AND id = {} AND op_index = {}",
            d.quote_ident(&self.progress),
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
        );
        crate::sql::raw_execute_tx(tx, &del, vec![s(key.0), s(key.1), SqlValue::I32(op)]).await?;
        let ins = format!(
            "INSERT INTO {} (app, id, op_index, effect_hi, done_hi, rows_done, batches, direction, updated_at) \
             VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {})",
            d.quote_ident(&self.progress),
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.placeholder(8),
            d.utc_now_sql(),
        );
        let hi = effect_hi.map_or(SqlValue::Null, s);
        crate::sql::raw_execute_tx(
            tx,
            &ins,
            vec![
                s(key.0),
                s(key.1),
                SqlValue::I32(op),
                hi.clone(),
                hi,
                SqlValue::I64(rows_done),
                SqlValue::I64(batches),
                s("up"),
            ],
        )
        .await?;
        Ok(())
    }

    /// The fenced heartbeat of a batch that does not finish its unit.
    pub(crate) fn heartbeat(
        &self,
        pool: &Pool,
        key: (&str, &str),
        run_id: &str,
        pos: i64,
    ) -> (String, Vec<SqlValue>) {
        let sql = format!(
            "UPDATE {} SET heartbeat_at = {} WHERE app = {} AND id = {} AND run_id = {} AND pos = {}",
            self.qt(pool),
            pool.dialect().utc_now_sql(),
            p(pool, 1),
            p(pool, 2),
            p(pool, 3),
            p(pool, 4),
        );
        (sql, vec![s(key.0), s(key.1), s(run_id), SqlValue::I64(pos)])
    }

    /// Append an event (the history; rows are deleted on rollback).
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn event(
        &self,
        pool: &Pool,
        kind: &str,
        key: (&str, &str),
        checksum: Option<&str>,
        run_id: &str,
        reason: Option<&str>,
    ) -> Result<(), MigrateError> {
        let (sql, binds) = self.event_stmt(pool.dialect(), kind, key, checksum, run_id, reason);
        raw_execute_pool(pool, &sql, binds).await?;
        Ok(())
    }

    /// [`Self::event`] as a statement, to run in a batch's transaction.
    pub(crate) fn event_stmt(
        &self,
        d: &dyn crate::sql::Dialect,
        kind: &str,
        key: (&str, &str),
        checksum: Option<&str>,
        run_id: &str,
        reason: Option<&str>,
    ) -> (String, Vec<SqlValue>) {
        let ph = |n| d.placeholder(n);
        let sql = format!(
            "INSERT INTO {} (kind, app, mig, checksum, run_id, os_user, host, app_version, reason, at) \
             VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {}, {})",
            d.quote_ident(&self.events),
            ph(1),
            ph(2),
            ph(3),
            ph(4),
            ph(5),
            ph(6),
            ph(7),
            ph(8),
            ph(9),
            d.utc_now_sql(),
        );
        let env = |k: &str| std::env::var(k).ok().map_or(SqlValue::Null, s);
        (
            sql,
            vec![
                s(kind),
                s(key.0),
                s(key.1),
                checksum.map_or(SqlValue::Null, s),
                s(run_id),
                env("USER"),
                env("HOSTNAME"),
                s(env!("CARGO_PKG_VERSION")),
                reason.map_or(SqlValue::Null, s),
            ],
        )
    }

    /// The last code hash recorded per `(app, step id)`.
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn step_hashes(
        &self,
        pool: &Pool,
    ) -> Result<std::collections::BTreeMap<(String, String), String>, MigrateError> {
        let mut out = std::collections::BTreeMap::new();
        if !crate::migrate::try_table_exists_here(pool, &self.events).await? {
            return Ok(out);
        }
        let sql = format!(
            "SELECT app, reason FROM {} WHERE kind = {} ORDER BY id",
            self.qe(pool),
            p(pool, 1),
        );
        let rows: Vec<(String, Option<String>)> =
            raw_query_pool(&sql, vec![s(super::run::STEP_HASH)], pool).await?;
        for (app, reason) in rows {
            if let Some((id, hash)) = reason.as_deref().and_then(|r| r.split_once(' ')) {
                out.insert((app, id.to_owned()), hash.to_owned());
            }
        }
        Ok(out)
    }

    /// The kinds of events written for `key`, oldest first (tests and status).
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn event_kinds(
        &self,
        pool: &Pool,
        key: (&str, &str),
    ) -> Result<Vec<String>, MigrateError> {
        let sql = format!(
            "SELECT kind FROM {} WHERE app = {} AND mig = {} ORDER BY id",
            self.qe(pool),
            p(pool, 1),
            p(pool, 2),
        );
        let rows: Vec<(String,)> = raw_query_pool(&sql, vec![s(key.0), s(key.1)], pool).await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    /// The reason of the latest `kind` event of a migration.
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn last_reason(
        &self,
        pool: &Pool,
        key: (&str, &str),
        kind: &str,
    ) -> Result<Option<String>, MigrateError> {
        let sql = format!(
            "SELECT reason FROM {} WHERE app = {} AND mig = {} AND kind = {} ORDER BY id DESC",
            self.qe(pool),
            p(pool, 1),
            p(pool, 2),
            p(pool, 3),
        );
        let rows: Vec<(Option<String>,)> =
            raw_query_pool(&sql, vec![s(key.0), s(key.1), s(kind)], pool).await?;
        Ok(rows.into_iter().next().and_then(|r| r.0))
    }
}

/// One row must have changed, or another run took the migration over.
pub(crate) fn fenced(n: u64) -> Result<(), MigrateError> {
    if n == 1 {
        Ok(())
    } else {
        Err(MigrateError::Validation(
            "lock lost: another run owns this migration now; nothing more was written".into(),
        ))
    }
}
