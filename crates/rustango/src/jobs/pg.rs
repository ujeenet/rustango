//! Database-backed job queue. Runs on PostgreSQL, MySQL 8.0+ or
//! SQLite through [`crate::sql::Pool`]. The type is named
//! `PgJobQueue` for back-compat; it is not PG-only.
//!
//! ## How a job is picked up
//!
//! | Backend | Strategy | Worker concurrency |
//! |---------|---|---|
//! | PostgreSQL | `WITH next AS (… FOR UPDATE SKIP LOCKED) UPDATE … RETURNING` | Many writers, no contention. |
//! | MySQL 8.0+ | The same `FOR UPDATE SKIP LOCKED` shape. | Many writers. |
//! | SQLite | `UPDATE … WHERE id = (SELECT id … LIMIT 1) RETURNING …` in a transaction. SQLite already serializes writers, so pickup is exclusive. | One writer at a time. Best for low or medium load. |
//!
//! Two replicas never grab the same row. A retry stays in the same
//! table with `run_at` pushed into the future by the backoff.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::jobs::{Job, JobQueue};
//! use rustango::jobs::pg::PgJobQueue;
//! use std::sync::Arc;
//!
//! // Once per deploy — creates `rustango_jobs` if it doesn't exist.
//! PgJobQueue::ensure_table_pool(&pool_enum).await?;
//!
//! let queue = Arc::new(
//!     PgJobQueue::with_workers_pool(pool_enum.clone(), 4)
//!         .poll_interval(std::time::Duration::from_secs(1)),
//! );
//! queue.register::<SendWelcomeEmail>().await;
//! queue.start().await;
//!
//! // From a handler:
//! queue.dispatch(&SendWelcomeEmail { user_id: 42 }).await?;
//! ```
//!
//! ## Schema (Postgres types shown; other backends use equivalents)
//!
//! ```sql
//! CREATE TABLE rustango_jobs (
//!     id           BIGSERIAL PRIMARY KEY,
//!     name         TEXT        NOT NULL,
//!     payload      JSONB       NOT NULL,
//!     attempt      INTEGER     NOT NULL DEFAULT 0,
//!     max_attempts INTEGER     NOT NULL,
//!     run_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
//!     locked_at    TIMESTAMPTZ,
//!     locked_by    TEXT,
//!     last_error   TEXT,
//!     created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
//!     context      JSONB  -- the enqueuer's audit source and timezone
//! );
//! ```
//!
//! Call [`PgJobQueue::ensure_table_pool`] at boot, or write your own
//! migration that emits the same DDL.
//!
//! ## Lock recovery
//!
//! [`PgJobQueue::reclaim_stuck_jobs_pool`] clears `locked_at` on rows
//! locked longer than a threshold. Run it on a schedule so jobs from a
//! crashed worker get picked up again, or let the queue do it with
//! [`PgJobQueue::reclaim_stuck_after`]. A running job refreshes its lock
//! every [`PgJobQueue::heartbeat_interval`], so keep the threshold well
//! above that. `attempt` counts at pickup: a job that crashes its
//! process still spends an attempt.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
#[cfg(feature = "postgres")]
use sqlx::PgPool;
use tokio::sync::{Mutex, Notify};

use super::{
    DeadLetterFn, HandlerRegistry, Job, JobDeadLetter, JobError, JobQueue, Running, StopSignal,
    DEFAULT_SHUTDOWN_GRACE,
};
use crate::sql::Pool;

/// Database-backed job queue over [`crate::sql::Pool`] (PostgreSQL,
/// MySQL 8+ or SQLite).
///
/// Cheap to clone; the state is all behind `Arc`. Workers start only
/// when you call [`PgJobQueue::start`].
pub struct PgJobQueue {
    pool: Pool,
    registry: Arc<Mutex<HandlerRegistry>>,
    run: Mutex<Option<Running>>,
    worker_count: usize,
    dead_letter: Arc<Mutex<Option<DeadLetterFn>>>,
    poll_interval: Duration,
    shutdown_grace: Duration,
    /// Used by `dispatch` to nudge workers out of their poll sleep so
    /// new jobs run with sub-second latency under low load.
    notify: Arc<Notify>,
    worker_id_prefix: String,
    heartbeat_interval: Duration,
    context_column: ContextColumn,
    reclaim_after: Option<Duration>,
    sweeper: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Whether `rustango_jobs` has a usable `context` column. A table from a
/// hand-written migration may lack it or give it another type; its jobs
/// then run with no context. Decided once per queue.
#[derive(Clone, Default)]
struct ContextColumn(Arc<tokio::sync::OnceCell<bool>>);

impl ContextColumn {
    async fn present(&self, pool: &Pool) -> bool {
        let probe = || async {
            let cols = crate::migrate::ensure::live_column_types(pool, "rustango_jobs")
                .await
                .map_err(|_| ())?;
            // No table yet: ask again once `ensure_table_pool` ran.
            if cols.is_empty() {
                return Err(());
            }
            let want = context_type(pool).to_ascii_lowercase();
            let usable = match cols.get("context") {
                Some(ty) if *ty == want => true,
                Some(ty) => {
                    tracing::warn!(
                        "rustango_jobs.context is `{ty}`, not `{want}`; jobs run as `system`. \
                         Fix the column type, then restart (#1229)."
                    );
                    false
                }
                None => {
                    tracing::warn!(
                        "rustango_jobs has no `context` column; jobs run as `system`. \
                         Call PgJobQueue::ensure_table_pool, then restart (#1229)."
                    );
                    false
                }
            };
            Ok(usable)
        };
        self.0.get_or_try_init(probe).await.is_ok_and(|p| *p)
    }
}

/// The `context` column's type on `pool`'s backend.
fn context_type(pool: &Pool) -> &'static str {
    match pool.dialect().name() {
        "mysql" => "JSON",
        "sqlite" => "TEXT",
        _ => "JSONB",
    }
}

const CREATE_JOBS_TABLE_SQL_PG: &str = "\
CREATE TABLE IF NOT EXISTS rustango_jobs (
    id           BIGSERIAL PRIMARY KEY,
    name         TEXT        NOT NULL,
    payload      JSONB       NOT NULL,
    attempt      INTEGER     NOT NULL DEFAULT 0,
    max_attempts INTEGER     NOT NULL,
    run_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    locked_at    TIMESTAMPTZ,
    locked_by    TEXT,
    last_error   TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    context      JSONB
);
CREATE INDEX IF NOT EXISTS rustango_jobs_pickup_idx
    ON rustango_jobs (run_at)
    WHERE locked_at IS NULL;
CREATE INDEX IF NOT EXISTS rustango_jobs_locked_idx
    ON rustango_jobs (locked_at)
    WHERE locked_at IS NOT NULL";

const CREATE_JOBS_TABLE_SQL_MYSQL: &str = "\
CREATE TABLE IF NOT EXISTS `rustango_jobs` (
    `id`           BIGINT      NOT NULL AUTO_INCREMENT PRIMARY KEY,
    `name`         VARCHAR(255) NOT NULL,
    `payload`      JSON         NOT NULL,
    `attempt`      INT          NOT NULL DEFAULT 0,
    `max_attempts` INT          NOT NULL,
    `run_at`       DATETIME(6)  NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    `locked_at`    DATETIME(6),
    `locked_by`    VARCHAR(255),
    `last_error`   TEXT,
    `created_at`   DATETIME(6)  NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    `context`      JSON
);
CREATE INDEX `rustango_jobs_pickup_idx` ON `rustango_jobs` (`run_at`);
CREATE INDEX `rustango_jobs_locked_idx` ON `rustango_jobs` (`locked_at`)";

/// `run_at` and `created_at` take their DEFAULT from the dialect, not
/// from a literal here, so the format cannot drift from the one the
/// reader expects. `dispatch` binds both columns; the default is only
/// a backstop for hand-written INSERTs.
fn create_jobs_table_sql_sqlite(dialect: &dyn crate::sql::Dialect) -> String {
    let now = dialect.current_timestamp_default();
    format!(
        "\
CREATE TABLE IF NOT EXISTS rustango_jobs (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    name         TEXT     NOT NULL,
    payload      TEXT     NOT NULL,
    attempt      INTEGER  NOT NULL DEFAULT 0,
    max_attempts INTEGER  NOT NULL,
    run_at       TEXT     NOT NULL DEFAULT {now},
    locked_at    TEXT,
    locked_by    TEXT,
    last_error   TEXT,
    created_at   TEXT     NOT NULL DEFAULT {now},
    context      TEXT
);
CREATE INDEX IF NOT EXISTS rustango_jobs_pickup_idx
    ON rustango_jobs (run_at) WHERE locked_at IS NULL;
CREATE INDEX IF NOT EXISTS rustango_jobs_locked_idx
    ON rustango_jobs (locked_at) WHERE locked_at IS NOT NULL"
    )
}

impl PgJobQueue {
    /// `(database, queue)`: the jobs table this queue claims from, and
    /// this queue across its clones.
    #[cfg(feature = "email")]
    pub(crate) fn identity(&self) -> (u64, usize) {
        (self.pool.scope_key(), Arc::as_ptr(&self.registry) as usize)
    }

    /// Build a queue from a [`crate::sql::Pool`] with `worker_count`
    /// worker tasks. Call [`Self::start`] to spawn them.
    #[must_use]
    pub fn with_workers_pool(pool: impl Into<Pool>, worker_count: usize) -> Self {
        // `q<n>` keeps two queues in one process from sharing a lease owner.
        static QUEUE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id_prefix = format!(
            "host:{}:pid:{}:q{}",
            hostname().unwrap_or_else(|| "unknown".into()),
            std::process::id(),
            QUEUE_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        Self {
            pool: pool.into(),
            registry: Arc::new(Mutex::new(HandlerRegistry::default())),
            run: Mutex::new(None),
            worker_count,
            dead_letter: Arc::new(Mutex::new(None)),
            poll_interval: Duration::from_secs(1),
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
            notify: Arc::new(Notify::new()),
            worker_id_prefix: id_prefix,
            heartbeat_interval: Duration::from_secs(10),
            context_column: ContextColumn::default(),
            reclaim_after: None,
            sweeper: Mutex::new(None),
        }
    }

    /// PG-typed shim around [`Self::with_workers_pool`].
    #[cfg(feature = "postgres")]
    #[must_use]
    pub fn with_workers(pool: PgPool, worker_count: usize) -> Self {
        Self::with_workers_pool(Pool::Postgres(pool), worker_count)
    }

    /// Default: 4 workers, 1-second poll interval. PG-typed shim.
    #[cfg(feature = "postgres")]
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self::with_workers(pool, 4)
    }

    /// Tri-dialect counterpart of [`Self::new`].
    #[must_use]
    pub fn new_pool(pool: impl Into<Pool>) -> Self {
        Self::with_workers_pool(pool, 4)
    }

    /// How long a worker waits between checks when the queue is empty.
    /// Default: 1 second. A lower value cuts latency but adds idle DB
    /// load. `dispatch` wakes workers directly, so this only matters
    /// when there is no work.
    #[must_use]
    pub fn poll_interval(mut self, d: Duration) -> Self {
        self.poll_interval = d;
        self
    }

    /// How often a running job refreshes its `locked_at`. Default: 10
    /// seconds, at least 1 ms. Reclaim only rows locked well longer than
    /// this, or a long job runs twice.
    #[must_use]
    pub fn heartbeat_interval(mut self, d: Duration) -> Self {
        // `tokio::time::interval` panics on zero.
        self.heartbeat_interval = d.max(Duration::from_millis(1));
        self
    }

    /// While running, unlock rows locked longer than `older_than`: at
    /// `start`, then every `min(older_than, 60s)`. Off by default (#2331).
    ///
    /// A worker killed mid-job never reaches `shutdown`, so this is what
    /// frees its rows. `older_than` is raised to three of this queue's
    /// heartbeats. A handler that blocks its thread past that loses its lease (#2374).
    #[must_use]
    pub fn reclaim_stuck_after(mut self, older_than: Duration) -> Self {
        self.reclaim_after = Some(older_than);
        self
    }

    /// How long `shutdown` waits for running jobs before aborting them
    /// and unlocking their rows. Default: [`DEFAULT_SHUTDOWN_GRACE`].
    #[must_use]
    pub fn shutdown_grace(mut self, grace: Duration) -> Self {
        self.shutdown_grace = grace;
        self
    }

    /// The `locked_by` of worker `n`.
    fn worker_id(&self, n: usize) -> String {
        format!("{}:w{n}", self.worker_id_prefix)
    }

    /// Set a callback invoked for jobs that exhaust retries or return
    /// [`JobError::Fatal`]. The job row is deleted after the callback
    /// returns — persist anything you need before that.
    pub async fn on_dead_letter<F, Fut>(&self, callback: F)
    where
        F: Fn(JobDeadLetter) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let boxed: DeadLetterFn = Arc::new(move |dl| Box::pin(callback(dl)));
        *self.dead_letter.lock().await = Some(boxed);
    }

    /// PG-typed shim around [`Self::ensure_table_pool`].
    ///
    /// # Errors
    /// Returns the underlying sqlx error if the DDL fails.
    #[cfg(feature = "postgres")]
    pub async fn ensure_table(pool: &PgPool) -> Result<(), sqlx::Error> {
        Self::ensure_table_pool(&Pool::Postgres(pool.clone())).await
    }

    /// Create the `rustango_jobs` table, its pickup index and its
    /// `locked_at` reclaim index if they are missing. Safe to call every
    /// boot. MySQL gets plain indexes because it has no partial indexes.
    ///
    /// # Errors
    /// Underlying sqlx DDL error.
    pub async fn ensure_table_pool(pool: &Pool) -> Result<(), sqlx::Error> {
        let ddl = match pool.dialect().name() {
            "mysql" => CREATE_JOBS_TABLE_SQL_MYSQL.to_owned(),
            "sqlite" => create_jobs_table_sql_sqlite(pool.dialect()),
            _ => CREATE_JOBS_TABLE_SQL_PG.to_owned(),
        };
        crate::sql::run_ddl_idempotent(pool, &ddl).await?;
        // A table from before 0.60.1 lacks the context column (#1229).
        // Look first: on PG even a no-op ADD COLUMN takes ACCESS EXCLUSIVE.
        let cols = crate::migrate::ensure::live_columns(pool, "rustango_jobs")
            .await
            .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
        if cols.contains("context") {
            return Ok(());
        }
        let add = format!(
            "ALTER TABLE rustango_jobs ADD COLUMN context {}",
            context_type(pool)
        );
        crate::migrate::ensure::run_statements(pool, &[add]).await
    }

    /// PG-typed shim around [`Self::reclaim_stuck_jobs_pool`].
    #[cfg(feature = "postgres")]
    pub async fn reclaim_stuck_jobs(
        pool: &PgPool,
        older_than: Duration,
    ) -> Result<u64, sqlx::Error> {
        Self::reclaim_stuck_jobs_pool(&Pool::Postgres(pool.clone()), older_than).await
    }

    /// Clear `locked_at` on any row locked longer than `older_than`,
    /// so jobs left reserved by a crashed worker run again. Call it on
    /// a schedule, for example once a minute.
    ///
    /// Returns the number of rows reclaimed.
    ///
    /// # Errors
    /// Underlying sqlx error.
    pub async fn reclaim_stuck_jobs_pool(
        pool: &Pool,
        older_than: Duration,
    ) -> Result<u64, sqlx::Error> {
        use crate::core::SqlValue;
        let cutoff: DateTime<Utc> = Utc::now()
            - chrono::Duration::from_std(older_than).unwrap_or(chrono::Duration::seconds(0));
        let p = pool.dialect().placeholder(1);
        let sql = format!(
            "UPDATE rustango_jobs \
                SET locked_at = NULL, locked_by = NULL \
              WHERE locked_at IS NOT NULL AND locked_at < {p}"
        );
        // The cutoff is computed in Rust and bound, so no backend
        // needs its own `NOW() - INTERVAL` SQL. `SqlValue::DateTime`
        // encodes as TIMESTAMPTZ on PG, DATETIME(6) on MySQL and
        // RFC3339 TEXT on SQLite.
        crate::sql::raw_execute_pool(pool, &sql, vec![SqlValue::DateTime(cutoff)])
            .await
            .map_err(|e| match e {
                crate::sql::ExecError::Driver(err) => err,
                other => sqlx::Error::Protocol(format!("{other}")),
            })
    }
}

#[async_trait::async_trait]
impl JobQueue for PgJobQueue {
    async fn register<T: Job>(&self) {
        self.registry.lock().await.register::<T>();
    }

    async fn register_with<T, F, Fut>(&self, run: F)
    where
        T: Job,
        F: Fn(T) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<(), JobError>> + Send + 'static,
    {
        self.registry.lock().await.register_with::<T, F, Fut>(run);
    }

    async fn dispatch<T: Job>(&self, payload: &T) -> Result<(), JobError> {
        use crate::core::SqlValue;
        let value = serde_json::to_value(payload).map_err(|e| JobError::Queue(e.to_string()))?;
        let max_attempts = i32::try_from(super::max_attempts::<T>()).unwrap_or(i32::MAX);
        let dialect = self.pool.dialect();
        let (p1, p2, p3, p4, p5) = (
            dialect.placeholder(1),
            dialect.placeholder(2),
            dialect.placeholder(3),
            dialect.placeholder(4),
            dialect.placeholder(5),
        );
        // Bind `run_at` / `created_at` instead of letting the column
        // default fill them. An older table may still default to
        // `CURRENT_TIMESTAMP`, and SQLite cannot ALTER a default. Such
        // a row sorts below every canonical one and would jump the
        // `ORDER BY run_at, id` pickup queue forever.
        let now = Utc::now();
        // `SqlValue::Json` stores as JSONB on PG, JSON on MySQL and
        // TEXT on SQLite, so one call covers all three backends.
        let mut binds = vec![
            SqlValue::String(T::NAME.to_owned()),
            SqlValue::Json(value),
            SqlValue::I32(max_attempts),
            SqlValue::DateTime(now),
            SqlValue::DateTime(now),
        ];
        let (mut cols, mut vals) = (String::new(), String::new());
        if let Some(ctx) = crate::task_context::TaskContext::capture().to_stored() {
            if self.context_column.present(&self.pool).await {
                cols = ", context".into();
                vals = format!(", {}", dialect.placeholder(6));
                binds.push(SqlValue::Json(ctx));
            }
        }
        let sql = format!(
            "INSERT INTO rustango_jobs (name, payload, max_attempts, run_at, created_at{cols}) \
             VALUES ({p1}, {p2}, {p3}, {p4}, {p5}{vals})"
        );
        crate::sql::raw_execute_pool(&self.pool, &sql, binds)
            .await
            .map_err(|e| JobError::Queue(e.to_string()))?;
        // Wake one waiting worker so the job starts before the next
        // poll tick.
        self.notify.notify_one();
        Ok(())
    }

    async fn start(&self) {
        let mut slot = self.run.lock().await;
        if slot.is_some() {
            return;
        }
        let (mut run, stop) = Running::new();
        for n in 0..self.worker_count {
            let pool = self.pool.clone();
            let registry = self.registry.clone();
            let dead_letter = self.dead_letter.clone();
            let stop = stop.clone();
            let notify = self.notify.clone();
            let poll = self.poll_interval;
            let worker = Worker {
                id: self.worker_id(n),
                heartbeat: self.heartbeat_interval,
                context_column: self.context_column.clone(),
            };
            let h = tokio::spawn(async move {
                worker_loop(pool, registry, dead_letter, stop, notify, poll, worker).await;
            });
            run.push(h);
        }
        if let Some(older_than) = self.reclaim_after {
            let older_than = older_than.max(self.heartbeat_interval * 3);
            let every = older_than.min(Duration::from_secs(60));
            let sweep = sweep_loop(self.pool.clone(), older_than, every, stop.clone());
            *self.sweeper.lock().await = Some(tokio::spawn(sweep));
        }
        *slot = Some(run);
    }

    async fn shutdown(&self) {
        let mut slot = self.run.lock().await;
        let Some(run) = slot.take() else { return };
        if let Some(sweeper) = self.sweeper.lock().await.take() {
            sweeper.abort();
        }
        for n in run.stop(self.shutdown_grace).await {
            // Hand the aborted job back now rather than at the next reclaim.
            release_worker_rows(&self.pool, &self.worker_id(n)).await;
        }
    }

    async fn pending_count(&self) -> usize {
        let sql = "SELECT COUNT(*) AS n FROM rustango_jobs WHERE locked_at IS NULL";
        crate::sql::raw_query_pool::<(i64,)>(sql, Vec::new(), &self.pool)
            .await
            .ok()
            .and_then(|rows| rows.into_iter().next())
            .map_or(0, |(n,)| usize::try_from(n).unwrap_or(0))
    }
}

// --------------------------------------------------------------------- worker loop

/// One worker's identity: the `locked_by` it writes and checks.
struct Worker {
    id: String,
    heartbeat: Duration,
    context_column: ContextColumn,
}

async fn worker_loop(
    pool: Pool,
    registry: Arc<Mutex<HandlerRegistry>>,
    dead_letter: Arc<Mutex<Option<DeadLetterFn>>>,
    mut stop: StopSignal,
    notify: Arc<Notify>,
    poll_interval: Duration,
    worker: Worker,
) {
    while !stop.is_set() {
        let with_context = worker.context_column.present(&pool).await;
        match pick_one(&pool, &worker.id, with_context).await {
            // Stop fired during the pick: hand the row back unrun.
            Ok(Some(row)) if stop.is_set() => unpick(&pool, &worker, row.id).await,
            Ok(Some(row)) => {
                run_one(&pool, &registry, &dead_letter, &worker, row).await;
                // Loop again immediately — there might be more.
            }
            Ok(None) => {
                // No work — wait for either a poll tick or a notify.
                tokio::select! {
                    () = tokio::time::sleep(poll_interval) => {}
                    () = notify.notified() => {}
                    () = stop.wait() => {}
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "job queue pickup failed");
                tokio::select! {
                    () = tokio::time::sleep(poll_interval) => {}
                    () = stop.wait() => {}
                }
            }
        }
    }
}

/// [`PgJobQueue::reclaim_stuck_after`]'s sweep, until the run stops.
async fn sweep_loop(pool: Pool, older_than: Duration, every: Duration, mut stop: StopSignal) {
    while !stop.is_set() {
        match PgJobQueue::reclaim_stuck_jobs_pool(&pool, older_than).await {
            Ok(0) => {}
            Ok(n) => tracing::warn!(reclaimed = n, "unlocked jobs a dead worker left locked"),
            Err(e) => tracing::error!(error = %e, "stuck-job sweep failed"),
        }
        tokio::select! {
            () = tokio::time::sleep(every) => {}
            () = stop.wait() => {}
        }
    }
}

#[derive(Debug)]
struct PickedJob {
    id: i64,
    name: String,
    payload: Value,
    attempt: i32,
    max_attempts: i32,
    /// The enqueuer's context, empty on a table without the column.
    context: crate::task_context::TaskContext,
}

async fn pick_one(
    pool: &Pool,
    worker_id: &str,
    with_context: bool,
) -> Result<Option<PickedJob>, sqlx::Error> {
    use crate::task_context::TaskContext;
    let now: DateTime<Utc> = Utc::now();
    let cols = if with_context {
        "id, name, payload, attempt, max_attempts, context"
    } else {
        "id, name, payload, attempt, max_attempts"
    };
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            use sqlx::Row as _;
            let sql = format!(
                "WITH next AS (
                     SELECT id FROM rustango_jobs
                      WHERE locked_at IS NULL AND run_at <= $2
                      ORDER BY run_at, id
                      FOR UPDATE SKIP LOCKED
                      LIMIT 1
                 )
                 UPDATE rustango_jobs
                    SET locked_at = $3, locked_by = $1, attempt = attempt + 1
                  WHERE id IN (SELECT id FROM next)
                 RETURNING {cols}"
            );
            let row = sqlx::query(&sql)
                .bind(worker_id)
                .bind(now)
                .bind(now)
                .fetch_optional(pg)
                .await?;
            let Some(row) = row else { return Ok(None) };
            Ok(Some(PickedJob {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
                payload: row.try_get("payload")?,
                attempt: row.try_get("attempt")?,
                max_attempts: row.try_get("max_attempts")?,
                context: TaskContext::from_stored(if with_context {
                    decode_context(row.try_get("context"))
                } else {
                    None
                }),
            }))
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            use sqlx::Row as _;
            // MySQL has no `UPDATE … RETURNING`, so do it in three
            // steps inside one transaction: reserve an id with
            // `FOR UPDATE SKIP LOCKED`, update it, then read the row.
            let mut tx = my.begin().await?;
            // `LIMIT` must come before `FOR UPDATE SKIP LOCKED`.
            // MySQL rejects the other order with a 1064 syntax error,
            // even though PG and SQLite accept both.
            let id_row: Option<(i64,)> = sqlx::query_as(
                "SELECT id FROM `rustango_jobs`
                  WHERE locked_at IS NULL AND run_at <= ?
                  ORDER BY run_at, id
                  LIMIT 1
                  FOR UPDATE SKIP LOCKED",
            )
            .bind(now)
            .fetch_optional(&mut *tx)
            .await?;
            let Some((id,)) = id_row else {
                tx.commit().await?;
                return Ok(None);
            };
            sqlx::query(
                "UPDATE `rustango_jobs` \
                    SET locked_at = ?, locked_by = ?, attempt = attempt + 1 WHERE id = ?",
            )
            .bind(now)
            .bind(worker_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
            let sql = format!("SELECT {cols} FROM `rustango_jobs` WHERE id = ?");
            let row = sqlx::query(&sql).bind(id).fetch_one(&mut *tx).await?;
            tx.commit().await?;
            let payload_json: sqlx::types::Json<Value> = row.try_get("payload")?;
            let context: Option<sqlx::types::Json<Value>> = if with_context {
                decode_context(row.try_get("context"))
            } else {
                None
            };
            Ok(Some(PickedJob {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
                payload: payload_json.0,
                attempt: row.try_get("attempt")?,
                max_attempts: row.try_get("max_attempts")?,
                context: TaskContext::from_stored(context.map(|c| c.0)),
            }))
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            use sqlx::Row as _;
            // sqlx's `begin()` takes a write lock on SQLite, and
            // SQLite serializes writers anyway, so this pickup is
            // exclusive on its own.
            let mut tx = sq.begin().await?;
            // Use the canonical encoding, not `to_rfc3339()`. This
            // string is compared against `run_at`, which has a fixed
            // six-digit fraction. chrono's RFC3339 width varies with
            // the value, and `+` sorts below `.`, so a due job could
            // read as not yet due.
            let now_str = crate::sql::encode_datetime(now);
            let sql = format!(
                "UPDATE rustango_jobs
                    SET locked_at = ?, locked_by = ?, attempt = attempt + 1
                  WHERE id = (
                      SELECT id FROM rustango_jobs
                       WHERE locked_at IS NULL AND run_at <= ?
                       ORDER BY run_at, id
                       LIMIT 1
                  )
                  RETURNING {cols}"
            );
            let row = sqlx::query(&sql)
                .bind(&now_str)
                .bind(worker_id)
                .bind(&now_str)
                .fetch_optional(&mut *tx)
                .await?;
            tx.commit().await?;
            let Some(row) = row else { return Ok(None) };
            let payload_text: String = row.try_get("payload")?;
            let payload: Value = serde_json::from_str(&payload_text).unwrap_or(Value::Null);
            let context: Option<String> = if with_context {
                decode_context(row.try_get("context"))
            } else {
                None
            };
            Ok(Some(PickedJob {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
                payload,
                attempt: row.try_get("attempt")?,
                max_attempts: row.try_get("max_attempts")?,
                context: TaskContext::from_stored(
                    context.and_then(|c| serde_json::from_str(&c).ok()),
                ),
            }))
        }
    }
}

/// The row is already locked here: a bad `context` value must not stop
/// the job, so it runs with no context instead.
fn decode_context<T>(got: Result<Option<T>, sqlx::Error>) -> Option<T> {
    got.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "unreadable job context; running as system");
        None
    })
}

async fn run_one(
    pool: &Pool,
    registry: &Arc<Mutex<HandlerRegistry>>,
    dead_letter: &Arc<Mutex<Option<DeadLetterFn>>>,
    worker: &Worker,
    job: PickedJob,
) {
    let entry = registry.lock().await.lookup(&job.name);
    let Some(entry) = entry else {
        // This process cannot run it; the pickup must not spend an attempt.
        tracing::warn!(job = %job.name, id = job.id, "no handler registered — leaving locked");
        give_back_attempt(pool, worker, job.id).await;
        return;
    };
    let static_name = entry.name;

    // `attempt` already counts this run. Past the cap means earlier runs
    // died with their worker; running it again could crash the next one.
    // Rows queued before #2333 may still hold 0.
    let max_attempts = job.max_attempts.max(1);
    if job.attempt > max_attempts {
        let msg = "no attempts left: an earlier run stopped without finishing";
        handle_dead_letter(pool, dead_letter, worker, &job, static_name, msg).await;
        return;
    }

    // The enqueuer's context, as `InMemoryJobQueue` does (#1229).
    let run = job
        .context
        .clone()
        .install((entry.handler)(job.payload.clone()));
    let (result, held) = run_with_heartbeat(pool, worker, job.id, run).await;
    if !held {
        // Another worker may own the row now; its outcome is not ours to write.
        tracing::warn!(id = job.id, worker = %worker.id, "job lease lost; result dropped");
        return;
    }

    match result {
        Ok(()) => {
            finish_job(pool, worker, job.id).await;
        }
        Err(JobError::Retryable(msg)) => {
            if job.attempt >= max_attempts {
                handle_dead_letter(pool, dead_letter, worker, &job, static_name, &msg).await;
            } else {
                let failed = u32::try_from(job.attempt - 1).unwrap_or(0);
                let backoff = chrono::Duration::from_std(entry.backoff(failed)).unwrap_or_default();
                let next_run: DateTime<Utc> = Utc::now() + backoff;
                schedule_retry(pool, worker, job.id, next_run, &msg).await;
            }
        }
        Err(e @ (JobError::Fatal(_) | JobError::Queue(_))) => {
            let msg = e.to_string();
            handle_dead_letter(pool, dead_letter, worker, &job, static_name, &msg).await;
        }
    }
}

/// Drive `run`, refreshing the row's `locked_at` every heartbeat so a
/// reclaim sweep does not hand a live job to a second worker. The flag
/// is `false` once a heartbeat found the lease gone.
async fn run_with_heartbeat<F>(pool: &Pool, worker: &Worker, id: i64, run: F) -> (F::Output, bool)
where
    F: std::future::Future,
{
    let mut run = std::pin::pin!(run);
    // Its own future, so a heartbeat waiting on a connection never stops `run` (#1961).
    let lease = std::pin::pin!(async {
        let mut beat = tokio::time::interval(worker.heartbeat);
        beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        beat.tick().await; // the first tick is immediate
        loop {
            beat.tick().await;
            if !heartbeat(pool, worker, id).await {
                return;
            }
        }
    });
    tokio::select! {
        out = &mut run => (out, true),
        () = lease => (run.await, false),
    }
}

/// Refresh the lease. `false` only when the row is no longer this
/// worker's; a failed write is logged and treated as still held.
async fn heartbeat(pool: &Pool, worker: &Worker, id: i64) -> bool {
    use crate::core::SqlValue;
    let d = pool.dialect();
    let sql = format!(
        "UPDATE rustango_jobs SET locked_at = {} WHERE id = {} AND locked_by = {}",
        d.placeholder(1),
        d.placeholder(2),
        d.placeholder(3),
    );
    let binds = vec![
        SqlValue::DateTime(Utc::now()),
        SqlValue::I64(id),
        SqlValue::String(worker.id.clone()),
    ];
    match crate::sql::raw_execute_pool(pool, &sql, binds).await {
        Ok(0) => {
            tracing::warn!(id, worker = %worker.id, "job lease lost while running");
            false
        }
        Ok(_) => true,
        Err(e) => {
            tracing::error!(id, error = %e, "job heartbeat failed");
            true
        }
    }
}

/// Undo the pickup's `attempt + 1`, keeping the lock.
async fn give_back_attempt(pool: &Pool, worker: &Worker, id: i64) {
    use crate::core::SqlValue;
    let d = pool.dialect();
    let sql = format!(
        "UPDATE rustango_jobs SET attempt = attempt - 1 WHERE id = {} AND locked_by = {}",
        d.placeholder(1),
        d.placeholder(2),
    );
    let binds = vec![SqlValue::I64(id), SqlValue::String(worker.id.clone())];
    log_finish(
        "attempt give-back",
        id,
        worker,
        crate::sql::raw_execute_pool(pool, &sql, binds).await,
    );
}

/// Unlock every row `worker_id` still holds.
async fn release_worker_rows(pool: &Pool, worker_id: &str) {
    use crate::core::SqlValue;
    let sql = format!(
        "UPDATE rustango_jobs SET locked_at = NULL, locked_by = NULL WHERE locked_by = {}",
        pool.dialect().placeholder(1),
    );
    let binds = vec![SqlValue::String(worker_id.to_owned())];
    if let Err(e) = crate::sql::raw_execute_pool(pool, &sql, binds).await {
        tracing::error!(worker = worker_id, error = %e, "releasing an aborted job failed");
    }
}

/// Unlock a row this worker picked but did not run, refunding the attempt.
async fn unpick(pool: &Pool, worker: &Worker, id: i64) {
    use crate::core::SqlValue;
    let d = pool.dialect();
    let sql = format!(
        "UPDATE rustango_jobs SET locked_at = NULL, locked_by = NULL, attempt = attempt - 1 \
         WHERE id = {} AND locked_by = {}",
        d.placeholder(1),
        d.placeholder(2),
    );
    let binds = vec![SqlValue::I64(id), SqlValue::String(worker.id.clone())];
    if let Err(e) = crate::sql::raw_execute_pool(pool, &sql, binds).await {
        tracing::error!(id, worker = %worker.id, error = %e, "releasing an unrun job failed");
    }
}

/// Log a finishing write that failed or found the lease gone.
fn log_finish(what: &str, id: i64, worker: &Worker, res: Result<u64, crate::sql::ExecError>) {
    match res {
        Ok(0) => tracing::warn!(id, worker = %worker.id, "job lease lost; {what} skipped"),
        Ok(_) => {}
        Err(e) => tracing::error!(id, error = %e, "job {what} failed"),
    }
}

/// Delete a finished job — only while this worker still holds it.
async fn finish_job(pool: &Pool, worker: &Worker, id: i64) {
    use crate::core::SqlValue;
    let d = pool.dialect();
    let sql = format!(
        "DELETE FROM rustango_jobs WHERE id = {} AND locked_by = {}",
        d.placeholder(1),
        d.placeholder(2),
    );
    let binds = vec![SqlValue::I64(id), SqlValue::String(worker.id.clone())];
    log_finish(
        "delete",
        id,
        worker,
        crate::sql::raw_execute_pool(pool, &sql, binds).await,
    );
}

async fn schedule_retry(
    pool: &Pool,
    worker: &Worker,
    id: i64,
    next_run: DateTime<Utc>,
    last_error: &str,
) {
    use crate::core::SqlValue;
    let d = pool.dialect();
    let sql = format!(
        "UPDATE rustango_jobs \
            SET run_at = {p1}, locked_at = NULL, locked_by = NULL, last_error = {p2} \
          WHERE id = {p3} AND locked_by = {p4}",
        p1 = d.placeholder(1),
        p2 = d.placeholder(2),
        p3 = d.placeholder(3),
        p4 = d.placeholder(4),
    );
    let binds = vec![
        SqlValue::DateTime(next_run),
        SqlValue::String(last_error.to_owned()),
        SqlValue::I64(id),
        SqlValue::String(worker.id.clone()),
    ];
    log_finish(
        "retry",
        id,
        worker,
        crate::sql::raw_execute_pool(pool, &sql, binds).await,
    );
}

async fn handle_dead_letter(
    pool: &Pool,
    dead_letter: &Arc<Mutex<Option<DeadLetterFn>>>,
    worker: &Worker,
    job: &PickedJob,
    static_name: &'static str,
    error: &str,
) {
    // Confirm the lease first: the callback must not fire for a row
    // another worker now owns.
    if !heartbeat(pool, worker, job.id).await {
        return;
    }
    let cb = dead_letter.lock().await.clone();
    if let Some(cb) = cb {
        let dl = JobDeadLetter {
            name: static_name,
            payload: job.payload.clone(),
            attempts: u32::try_from(job.attempt).unwrap_or(0),
            error: error.to_owned(),
        };
        // The callback sees who enqueued the job, as the job did.
        job.context
            .clone()
            .install(super::deliver_dead_letter(cb, dl))
            .await;
    } else {
        tracing::error!(
            job = static_name,
            attempts = job.attempt,
            error,
            "job queue dead-letter (no callback configured)"
        );
    }
    finish_job(pool, worker, job.id).await;
}

/// Best-effort hostname with no extra dependency: read the env var
/// most container runtimes set.
fn hostname() -> Option<String> {
    std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    //! Pure-Rust tests that need no database. Live PG / MySQL /
    //! SQLite coverage lives in `tests/jobs_*_live.rs`.

    use super::*;

    fn dummy_pool() -> Pool {
        // Never connects. Only lets us build a `PgJobQueue` for
        // assertions that do no I/O.
        #[cfg(feature = "postgres")]
        {
            Pool::Postgres(
                sqlx::postgres::PgPoolOptions::new()
                    .max_connections(1)
                    .connect_lazy("postgres://localhost:1/none")
                    .expect("lazy pool"),
            )
        }
        #[cfg(all(not(feature = "postgres"), feature = "sqlite"))]
        {
            Pool::Sqlite(
                sqlx::sqlite::SqlitePoolOptions::new()
                    .max_connections(1)
                    .connect_lazy("sqlite::memory:")
                    .expect("lazy pool"),
            )
        }
        #[cfg(all(not(feature = "postgres"), not(feature = "sqlite"), feature = "mysql"))]
        {
            Pool::Mysql(
                sqlx::mysql::MySqlPoolOptions::new()
                    .max_connections(1)
                    .connect_lazy("mysql://localhost:1/none")
                    .expect("lazy pool"),
            )
        }
    }

    #[tokio::test]
    async fn worker_id_prefix_includes_pid() {
        let q = PgJobQueue::with_workers_pool(dummy_pool(), 0);
        assert!(q.worker_id_prefix.contains("pid:"));
    }

    #[tokio::test]
    async fn poll_interval_is_overridable() {
        let q = PgJobQueue::with_workers_pool(dummy_pool(), 0)
            .poll_interval(Duration::from_millis(250));
        assert_eq!(q.poll_interval, Duration::from_millis(250));
    }

    /// A zero interval would panic in `tokio::time::interval`.
    #[tokio::test]
    async fn heartbeat_interval_is_at_least_a_millisecond() {
        let q = PgJobQueue::with_workers_pool(dummy_pool(), 0).heartbeat_interval(Duration::ZERO);
        assert_eq!(q.heartbeat_interval, Duration::from_millis(1));
    }

    #[tokio::test]
    async fn dead_letter_callback_can_be_set() {
        let q = PgJobQueue::with_workers_pool(dummy_pool(), 0);
        q.on_dead_letter(|_| async {}).await;
        assert!(q.dead_letter.lock().await.is_some());
    }

    #[tokio::test]
    async fn register_lookups_handler_under_static_name() {
        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize)]
        struct Demo;

        #[async_trait::async_trait]
        impl Job for Demo {
            const NAME: &'static str = "demo:job";
            async fn run(&self) -> Result<(), JobError> {
                Ok(())
            }
        }

        let q = PgJobQueue::with_workers_pool(dummy_pool(), 0);
        q.register::<Demo>().await;
        let r = q.registry.lock().await.lookup("demo:job");
        assert_eq!(r.expect("registered").name, "demo:job");
    }

    /// #1961: a job holding the only connection must not stall its heartbeat.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn heartbeat_waits_beside_a_job_holding_the_connection() {
        let sqlite = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect("sqlite::memory:")
            .await
            .expect("pool");
        let pool = Pool::Sqlite(sqlite.clone());
        PgJobQueue::ensure_table_pool(&pool).await.expect("table");
        let worker = Worker {
            id: "w".into(),
            heartbeat: Duration::from_millis(20),
            context_column: ContextColumn::default(),
        };
        let job = async {
            let _conn = sqlite.acquire().await.expect("conn");
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        let started = std::time::Instant::now();
        let ((), held) = run_with_heartbeat(&pool, &worker, 1, job).await;
        assert!(held);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "stalled for {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn register_lookup_returns_none_for_unknown_name() {
        let q = PgJobQueue::with_workers_pool(dummy_pool(), 0);
        assert!(q.registry.lock().await.lookup("unknown").is_none());
    }
}
