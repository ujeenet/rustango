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
//!     created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
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
//! crashed worker get picked up again.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
#[cfg(feature = "postgres")]
use sqlx::PgPool;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use super::{DeadLetterFn, HandlerRegistry, Job, JobDeadLetter, JobError, JobQueue};
use crate::sql::Pool;

/// Database-backed job queue over [`crate::sql::Pool`] (PostgreSQL,
/// MySQL 8+ or SQLite).
///
/// Cheap to clone; the state is all behind `Arc`. Workers start only
/// when you call [`PgJobQueue::start`].
pub struct PgJobQueue {
    pool: Pool,
    registry: Arc<Mutex<HandlerRegistry>>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    worker_count: usize,
    dead_letter: Arc<Mutex<Option<DeadLetterFn>>>,
    poll_interval: Duration,
    shutdown: Arc<AtomicBool>,
    /// Used by `dispatch` to nudge workers out of their poll sleep so
    /// new jobs run with sub-second latency under low load.
    notify: Arc<Notify>,
    worker_id_prefix: String,
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
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS rustango_jobs_pickup_idx
    ON rustango_jobs (run_at)
    WHERE locked_at IS NULL";

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
    `created_at`   DATETIME(6)  NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
);
CREATE INDEX `rustango_jobs_pickup_idx` ON `rustango_jobs` (`run_at`)";

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
    created_at   TEXT     NOT NULL DEFAULT {now}
);
CREATE INDEX IF NOT EXISTS rustango_jobs_pickup_idx
    ON rustango_jobs (run_at) WHERE locked_at IS NULL"
    )
}

impl PgJobQueue {
    /// Build a queue from a [`crate::sql::Pool`] with `worker_count`
    /// worker tasks. Call [`Self::start`] to spawn them.
    #[must_use]
    pub fn with_workers_pool(pool: impl Into<Pool>, worker_count: usize) -> Self {
        let id_prefix = format!(
            "host:{}:pid:{}",
            hostname().unwrap_or_else(|| "unknown".into()),
            std::process::id()
        );
        Self {
            pool: pool.into(),
            registry: Arc::new(Mutex::new(HandlerRegistry::default())),
            workers: Mutex::new(Vec::new()),
            worker_count,
            dead_letter: Arc::new(Mutex::new(None)),
            poll_interval: Duration::from_secs(1),
            shutdown: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
            worker_id_prefix: id_prefix,
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

    /// Create the `rustango_jobs` table and its pickup index if they
    /// are missing. Safe to call every boot. MySQL gets a plain index
    /// because it has no partial indexes; PG and SQLite get the
    /// `WHERE locked_at IS NULL` one.
    ///
    /// # Errors
    /// Underlying sqlx DDL error.
    pub async fn ensure_table_pool(pool: &Pool) -> Result<(), sqlx::Error> {
        let ddl = match pool.dialect().name() {
            "mysql" => CREATE_JOBS_TABLE_SQL_MYSQL.to_owned(),
            "sqlite" => create_jobs_table_sql_sqlite(pool.dialect()),
            _ => CREATE_JOBS_TABLE_SQL_PG.to_owned(),
        };
        crate::sql::run_ddl_idempotent(pool, &ddl).await
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

    async fn dispatch<T: Job>(&self, payload: &T) -> Result<(), JobError> {
        use crate::core::SqlValue;
        let value = serde_json::to_value(payload).map_err(|e| JobError::Queue(e.to_string()))?;
        let max_attempts = i32::try_from(T::MAX_ATTEMPTS).unwrap_or(i32::MAX);
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
        let sql = format!(
            "INSERT INTO rustango_jobs (name, payload, max_attempts, run_at, created_at) \
             VALUES ({p1}, {p2}, {p3}, {p4}, {p5})"
        );
        // `SqlValue::Json` stores as JSONB on PG, JSON on MySQL and
        // TEXT on SQLite, so one call covers all three backends.
        crate::sql::raw_execute_pool(
            &self.pool,
            &sql,
            vec![
                SqlValue::String(T::NAME.to_owned()),
                SqlValue::Json(value),
                SqlValue::I32(max_attempts),
                SqlValue::DateTime(now),
                SqlValue::DateTime(now),
            ],
        )
        .await
        .map_err(|e| JobError::Queue(e.to_string()))?;
        // Wake one waiting worker so the job starts before the next
        // poll tick.
        self.notify.notify_one();
        Ok(())
    }

    async fn start(&self) {
        let mut workers = self.workers.lock().await;
        if !workers.is_empty() {
            return;
        }
        for n in 0..self.worker_count {
            let pool = self.pool.clone();
            let registry = self.registry.clone();
            let dead_letter = self.dead_letter.clone();
            let shutdown = self.shutdown.clone();
            let notify = self.notify.clone();
            let poll = self.poll_interval;
            let worker_id = format!("{}:w{}", self.worker_id_prefix, n);
            let h = tokio::spawn(async move {
                worker_loop(
                    pool,
                    registry,
                    dead_letter,
                    shutdown,
                    notify,
                    poll,
                    worker_id,
                )
                .await;
            });
            workers.push(h);
        }
    }

    async fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Wake every worker so they all observe the shutdown flag and exit.
        self.notify.notify_waiters();
        let mut workers = self.workers.lock().await;
        for h in workers.drain(..) {
            // Give in-flight jobs ~5 seconds to finish before aborting.
            let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
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

#[allow(clippy::too_many_arguments)]
async fn worker_loop(
    pool: Pool,
    registry: Arc<Mutex<HandlerRegistry>>,
    dead_letter: Arc<Mutex<Option<DeadLetterFn>>>,
    shutdown: Arc<AtomicBool>,
    notify: Arc<Notify>,
    poll_interval: Duration,
    worker_id: String,
) {
    while !shutdown.load(Ordering::SeqCst) {
        match pick_one(&pool, &worker_id).await {
            Ok(Some(row)) => {
                run_one(&pool, &registry, &dead_letter, row).await;
                // Loop again immediately — there might be more.
            }
            Ok(None) => {
                // No work — wait for either a poll tick or a notify.
                tokio::select! {
                    () = tokio::time::sleep(poll_interval) => {}
                    () = notify.notified() => {}
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "job queue pickup failed");
                tokio::time::sleep(poll_interval).await;
            }
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
}

async fn pick_one(pool: &Pool, worker_id: &str) -> Result<Option<PickedJob>, sqlx::Error> {
    let now: DateTime<Utc> = Utc::now();
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            use sqlx::Row as _;
            let row = sqlx::query(
                "WITH next AS (
                     SELECT id FROM rustango_jobs
                      WHERE locked_at IS NULL AND run_at <= $2
                      ORDER BY run_at, id
                      FOR UPDATE SKIP LOCKED
                      LIMIT 1
                 )
                 UPDATE rustango_jobs
                    SET locked_at = $3, locked_by = $1
                  WHERE id IN (SELECT id FROM next)
                 RETURNING id, name, payload, attempt, max_attempts",
            )
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
            sqlx::query("UPDATE `rustango_jobs` SET locked_at = ?, locked_by = ? WHERE id = ?")
                .bind(now)
                .bind(worker_id)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            let row = sqlx::query(
                "SELECT id, name, payload, attempt, max_attempts \
                 FROM `rustango_jobs` WHERE id = ?",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            tx.commit().await?;
            let payload_json: sqlx::types::Json<Value> = row.try_get("payload")?;
            Ok(Some(PickedJob {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
                payload: payload_json.0,
                attempt: row.try_get("attempt")?,
                max_attempts: row.try_get("max_attempts")?,
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
            let row = sqlx::query(
                "UPDATE rustango_jobs
                    SET locked_at = ?, locked_by = ?
                  WHERE id = (
                      SELECT id FROM rustango_jobs
                       WHERE locked_at IS NULL AND run_at <= ?
                       ORDER BY run_at, id
                       LIMIT 1
                  )
                  RETURNING id, name, payload, attempt, max_attempts",
            )
            .bind(&now_str)
            .bind(worker_id)
            .bind(&now_str)
            .fetch_optional(&mut *tx)
            .await?;
            tx.commit().await?;
            let Some(row) = row else { return Ok(None) };
            let payload_text: String = row.try_get("payload")?;
            let payload: Value = serde_json::from_str(&payload_text).unwrap_or(Value::Null);
            Ok(Some(PickedJob {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
                payload,
                attempt: row.try_get("attempt")?,
                max_attempts: row.try_get("max_attempts")?,
            }))
        }
    }
}

async fn run_one(
    pool: &Pool,
    registry: &Arc<Mutex<HandlerRegistry>>,
    dead_letter: &Arc<Mutex<Option<DeadLetterFn>>>,
    job: PickedJob,
) {
    let handler = registry.lock().await.lookup_owned(&job.name);
    let Some((handler, static_name)) = handler else {
        tracing::warn!(job = %job.name, id = job.id, "no handler registered — leaving locked");
        return;
    };

    let result = handler(job.payload.clone()).await;

    match result {
        Ok(()) => {
            delete_job(pool, job.id).await;
        }
        Err(JobError::Retryable(msg)) => {
            let next_attempt = job.attempt + 1;
            if next_attempt >= job.max_attempts {
                handle_dead_letter(pool, dead_letter, &job, static_name, &msg).await;
            } else {
                let backoff_ms = super::retry_backoff_ms(u32::try_from(job.attempt).unwrap_or(0));
                let next_run: DateTime<Utc> = Utc::now()
                    + chrono::Duration::milliseconds(i64::try_from(backoff_ms).unwrap_or(i64::MAX));
                schedule_retry(pool, job.id, next_attempt, next_run, &msg).await;
            }
        }
        Err(e @ (JobError::Fatal(_) | JobError::Queue(_))) => {
            let msg = e.to_string();
            handle_dead_letter(pool, dead_letter, &job, static_name, &msg).await;
        }
    }
}

async fn delete_job(pool: &Pool, id: i64) {
    use crate::core::SqlValue;
    let p = pool.dialect().placeholder(1);
    let sql = format!("DELETE FROM rustango_jobs WHERE id = {p}");
    let _ = crate::sql::raw_execute_pool(pool, &sql, vec![SqlValue::I64(id)]).await;
}

async fn schedule_retry(
    pool: &Pool,
    id: i64,
    next_attempt: i32,
    next_run: DateTime<Utc>,
    last_error: &str,
) {
    use crate::core::SqlValue;
    let d = pool.dialect();
    let sql = format!(
        "UPDATE rustango_jobs \
            SET attempt = {p1}, run_at = {p2}, \
                locked_at = NULL, locked_by = NULL, \
                last_error = {p3} \
          WHERE id = {p4}",
        p1 = d.placeholder(1),
        p2 = d.placeholder(2),
        p3 = d.placeholder(3),
        p4 = d.placeholder(4),
    );
    let _ = crate::sql::raw_execute_pool(
        pool,
        &sql,
        vec![
            SqlValue::I32(next_attempt),
            SqlValue::DateTime(next_run),
            SqlValue::String(last_error.to_owned()),
            SqlValue::I64(id),
        ],
    )
    .await;
}

async fn handle_dead_letter(
    pool: &Pool,
    dead_letter: &Arc<Mutex<Option<DeadLetterFn>>>,
    job: &PickedJob,
    static_name: &'static str,
    error: &str,
) {
    let cb = dead_letter.lock().await.clone();
    if let Some(cb) = cb {
        cb(JobDeadLetter {
            name: static_name,
            payload: job.payload.clone(),
            attempts: u32::try_from(job.attempt + 1).unwrap_or(0),
            error: error.to_owned(),
        })
        .await;
    } else {
        tracing::error!(
            job = static_name,
            attempts = job.attempt + 1,
            error,
            "job queue dead-letter (no callback configured)"
        );
    }
    delete_job(pool, job.id).await;
}

// --------------------------------------------------------------------- helpers

impl HandlerRegistry {
    /// Like `lookup`, but also returns the registered `&'static str`.
    /// `JobDeadLetter` needs a static name.
    fn lookup_owned(&self, name: &str) -> Option<(super::HandlerFn, &'static str)> {
        let (handler, _) = self.handlers.get(name)?;
        let static_name = self.handlers.keys().find(|k| **k == name).copied()?;
        Some((handler.clone(), static_name))
    }
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
        let r = q.registry.lock().await.lookup_owned("demo:job");
        assert!(r.is_some());
        let (_, name) = r.unwrap();
        assert_eq!(name, "demo:job");
    }

    #[tokio::test]
    async fn register_lookup_returns_none_for_unknown_name() {
        let q = PgJobQueue::with_workers_pool(dummy_pool(), 0);
        assert!(q.registry.lock().await.lookup_owned("unknown").is_none());
    }
}
