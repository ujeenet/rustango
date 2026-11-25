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
}

/// The ledger's table names, from the configured base.
#[derive(Debug, Clone)]
pub(crate) struct Ledger {
    pub(crate) table: String,
    pub(crate) events: String,
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
    /// Create the ledger and events tables if absent (the caller holds the lock).
    ///
    /// # Errors
    /// Driver failures.
    pub(crate) async fn ensure(&self, pool: &Pool) -> Result<(), MigrateError> {
        let d = pool.dialect();
        for t in [ledger_table(&self.table), events_table(&self.events)] {
            if !crate::migrate::try_table_exists_here(pool, &t.name).await? {
                raw_execute_pool(pool, &create_table_sql_for(d, &t), Vec::new()).await?;
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
        );
        let sql = format!(
            "SELECT app, id, checksum, applied_body, state, unit_plan, pos, started_unit, run_id \
             FROM {} ORDER BY seq",
            self.table
        );
        let raw: Vec<Raw> = raw_query_pool(&sql, Vec::new(), pool).await?;
        raw.into_iter()
            .map(
                |(app, id, checksum, applied_body, state, plan, pos, started_unit, run_id)| {
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
            &format!("SELECT COALESCE(MAX(seq), 0) FROM {}", self.table),
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
            self.table,
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
            self.table,
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
            self.table,
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

    /// [`Self::advance`] with literal values, for a SQLite rebuild's own
    /// transaction (it runs on a raw driver connection).
    pub(crate) fn advance_literal(
        &self,
        key: (&str, &str),
        run_id: &str,
        from: i64,
        to: i64,
    ) -> String {
        let lit = |v: &str| format!("'{}'", v.replace('\'', "''"));
        format!(
            "UPDATE {} SET pos = {to}, started_unit = NULL, session = NULL WHERE app = {} AND id = {} \
             AND run_id = {} AND pos = {from}",
            self.table,
            lit(key.0),
            lit(key.1),
            lit(run_id),
        )
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
    ) -> Result<(), MigrateError> {
        let sql = format!(
            "UPDATE {} SET started_unit = {}, heartbeat_at = {} \
             WHERE app = {} AND id = {} AND run_id = {} AND pos = {}",
            self.table,
            p(pool, 1),
            pool.dialect().utc_now_sql(),
            p(pool, 2),
            p(pool, 3),
            p(pool, 4),
            p(pool, 5),
        );
        let n = raw_execute_pool(
            pool,
            &sql,
            vec![
                SqlValue::I64(unit),
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
            self.table,
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
        let sql = format!(
            "UPDATE {} SET state = {}, unit_plan = {}, pos = -1, started_unit = NULL, run_id = {} \
             WHERE app = {} AND id = {} AND run_id = {}",
            self.table,
            p(pool, 1),
            p(pool, 2),
            p(pool, 3),
            p(pool, 4),
            p(pool, 5),
            p(pool, 6),
        );
        let n = raw_execute_pool(
            pool,
            &sql,
            vec![
                s(RowState::RollingBack.as_str()),
                s(serde_json::to_string(plan)?),
                s(run_id),
                s(key.0),
                s(key.1),
                s(old_run),
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
            self.table,
            p(pool, 1),
            p(pool, 2),
            p(pool, 3),
        );
        let n = raw_execute_pool(pool, &sql, vec![s(key.0), s(key.1), s(run_id)]).await?;
        fenced(n)
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
        let sql = format!(
            "INSERT INTO {} (kind, app, mig, checksum, run_id, os_user, host, app_version, reason, at) \
             VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {}, {})",
            self.events,
            p(pool, 1),
            p(pool, 2),
            p(pool, 3),
            p(pool, 4),
            p(pool, 5),
            p(pool, 6),
            p(pool, 7),
            p(pool, 8),
            p(pool, 9),
            pool.dialect().utc_now_sql(),
        );
        let env = |k: &str| std::env::var(k).ok().map_or(SqlValue::Null, s);
        raw_execute_pool(
            pool,
            &sql,
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
        .await?;
        Ok(())
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
            self.events,
            p(pool, 1),
            p(pool, 2),
        );
        let rows: Vec<(String,)> = raw_query_pool(&sql, vec![s(key.0), s(key.1)], pool).await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
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
