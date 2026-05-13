//! Database-backed job queue.
//!
//! v0.38 — tri-dialect. Originally PG-only via `SELECT … FOR UPDATE
//! SKIP LOCKED`. The struct is still called `PgJobQueue` for back-
//! compat (every existing import `use rustango::jobs::pg::PgJobQueue;`
//! keeps working), but its internal pool is now
//! [`crate::sql::Pool`] — the queue runs on PostgreSQL, MySQL 8.0+,
//! or SQLite.
//!
//! ## Per-backend pickup semantics
//!
//! | Backend | Atomicity strategy | Worker concurrency |
//! |---------|---|---|
//! | PostgreSQL | `WITH next AS (… FOR UPDATE SKIP LOCKED) UPDATE … RETURNING` | Many writers, no contention. |
//! | MySQL 8.0+ | Same `FOR UPDATE SKIP LOCKED` shape via `WITH … UPDATE`. | Many writers (requires MySQL 8). |
//! | SQLite | `UPDATE … WHERE id = (SELECT id … LIMIT 1) RETURNING …` inside a transaction. SQLite serializes writers globally, so the pickup is implicitly mutually-exclusive. | One writer at a time — best for low/medium throughput. |
//!
//! Worker tasks pick the next ready job so two replicas never grab
//! the same row (PG/MySQL) or only one writer is active at a time
//! (SQLite). Retries land back in the same table with `run_at` pushed
//! into the future by exponential backoff.
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
//! ## Schema (Postgres flavor — other backends use equivalent types)
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
//! Use [`PgJobQueue::ensure_table_pool`] at boot, or roll your own
//! migration that emits the same DDL.
//!
//! ## Lock recovery
//!
//! [`PgJobQueue::reclaim_stuck_jobs_pool`] resets `locked_at = NULL`
//! for any row whose lock is older than the threshold. Call it on a
//! scheduler to recover from worker crashes.

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

/// Database-backed job queue. Internally generic over the backend
/// (PostgreSQL / MySQL 8+ / SQLite) via [`crate::sql::Pool`].
///
/// Cheap to clone (everything is `Arc`-wrapped internally). Workers
/// are not spawned until [`PgJobQueue::start`] is called.
///
/// Name kept as `PgJobQueue` for back-compat with v0.37 imports.
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

const CREATE_JOBS_TABLE_SQL_SQLITE: &str = "\
CREATE TABLE IF NOT EXISTS rustango_jobs (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    name         TEXT     NOT NULL,
    payload      TEXT     NOT NULL,
    attempt      INTEGER  NOT NULL DEFAULT 0,
    max_attempts INTEGER  NOT NULL,
    run_at       TEXT     NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    locked_at    TEXT,
    locked_by    TEXT,
    last_error   TEXT,
    created_at   TEXT     NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS rustango_jobs_pickup_idx
    ON rustango_jobs (run_at) WHERE locked_at IS NULL";

impl PgJobQueue {
    /// Build a queue from a [`crate::sql::Pool`] with `worker_count`
    /// worker tasks. Call [`Self::start`] to spawn them. v0.38 —
    /// tri-dialect entry point.
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

    /// PG-typed back-compat shim around [`Self::with_workers_pool`].
    #[cfg(feature = "postgres")]
    #[must_use]
    pub fn with_workers(pool: PgPool, worker_count: usize) -> Self {
        Self::with_workers_pool(Pool::Postgres(pool), worker_count)
    }

    /// Default: 4 workers, 1-second poll interval. PG back-compat shim.
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

    /// How long workers wait between checks when the queue is empty.
    /// Lower = lower latency, higher idle DB load. Default: 1 second.
    /// New jobs nudge the workers via [`tokio::sync::Notify`] so this
    /// only matters when the queue is dry.
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

    /// PG-typed back-compat shim around [`Self::ensure_table_pool`].
    ///
    /// # Errors
    /// Returns the underlying sqlx error if the DDL fails.
    #[cfg(feature = "postgres")]
    pub async fn ensure_table(pool: &PgPool) -> Result<(), sqlx::Error> {
        Self::ensure_table_pool(&Pool::Postgres(pool.clone())).await
    }

    /// Create the `rustango_jobs` table + pickup index if absent.
    /// Idempotent. v0.38 — tri-dialect: per-dialect DDL strings.
    /// MySQL skips the partial index `WHERE locked_at IS NULL` (no
    /// partial-index support); PG + SQLite keep it.
    ///
    /// # Errors
    /// Underlying sqlx DDL error.
    pub async fn ensure_table_pool(pool: &Pool) -> Result<(), sqlx::Error> {
        let ddl = match pool.dialect().name() {
            "postgres" => CREATE_JOBS_TABLE_SQL_PG,
            "mysql" => CREATE_JOBS_TABLE_SQL_MYSQL,
            "sqlite" => CREATE_JOBS_TABLE_SQL_SQLITE,
            _ => CREATE_JOBS_TABLE_SQL_PG,
        };
        for stmt in ddl.split(';') {
            let trimmed = stmt.trim();
            if trimmed.is_empty() {
                continue;
            }
            match pool {
                #[cfg(feature = "postgres")]
                Pool::Postgres(pg) => {
                    sqlx::query(trimmed).execute(pg).await?;
                }
                #[cfg(feature = "mysql")]
                Pool::Mysql(my) => {
                    if let Err(e) = sqlx::query(trimmed).execute(my).await {
                        if !is_mysql_dup_index_error(&e) {
                            return Err(e);
                        }
                    }
                }
                #[cfg(feature = "sqlite")]
                Pool::Sqlite(sq) => {
                    sqlx::query(trimmed).execute(sq).await?;
                }
            }
        }
        Ok(())
    }

    /// PG-typed back-compat shim around [`Self::reclaim_stuck_jobs_pool`].
    #[cfg(feature = "postgres")]
    pub async fn reclaim_stuck_jobs(
        pool: &PgPool,
        older_than: Duration,
    ) -> Result<u64, sqlx::Error> {
        Self::reclaim_stuck_jobs_pool(&Pool::Postgres(pool.clone()), older_than).await
    }

    /// Reset `locked_at = NULL` on any row locked longer than
    /// `older_than`. Call from a scheduled task (e.g. every minute) to
    /// recover from worker crashes that left rows reserved but not
    /// finished. v0.38 — tri-dialect; the cutoff timestamp is computed
    /// in Rust and bound as a parameter so no per-backend
    /// `NOW() - INTERVAL` SQL is needed.
    ///
    /// Returns the number of rows reclaimed.
    ///
    /// # Errors
    /// Underlying sqlx error.
    pub async fn reclaim_stuck_jobs_pool(
        pool: &Pool,
        older_than: Duration,
    ) -> Result<u64, sqlx::Error> {
        let cutoff: DateTime<Utc> = Utc::now()
            - chrono::Duration::from_std(older_than).unwrap_or(chrono::Duration::seconds(0));
        let p = pool.dialect().placeholder(1);
        let sql = format!(
            "UPDATE rustango_jobs \
                SET locked_at = NULL, locked_by = NULL \
              WHERE locked_at IS NOT NULL AND locked_at < {p}"
        );
        match pool {
            #[cfg(feature = "postgres")]
            Pool::Postgres(pg) => Ok(sqlx::query(&sql)
                .bind(cutoff)
                .execute(pg)
                .await?
                .rows_affected()),
            #[cfg(feature = "mysql")]
            Pool::Mysql(my) => Ok(sqlx::query(&sql)
                .bind(cutoff)
                .execute(my)
                .await?
                .rows_affected()),
            #[cfg(feature = "sqlite")]
            Pool::Sqlite(sq) => {
                // SQLite stores TEXT-ISO-8601; bind as RFC3339 string.
                Ok(sqlx::query(&sql)
                    .bind(cutoff.to_rfc3339())
                    .execute(sq)
                    .await?
                    .rows_affected())
            }
        }
    }
}

#[async_trait::async_trait]
impl JobQueue for PgJobQueue {
    async fn register<T: Job>(&self) {
        self.registry.lock().await.register::<T>();
    }

    async fn dispatch<T: Job>(&self, payload: &T) -> Result<(), JobError> {
        let value = serde_json::to_value(payload).map_err(|e| JobError::Queue(e.to_string()))?;
        let max_attempts = i32::try_from(T::MAX_ATTEMPTS).unwrap_or(i32::MAX);
        let dialect = self.pool.dialect();
        let (p1, p2, p3) = (
            dialect.placeholder(1),
            dialect.placeholder(2),
            dialect.placeholder(3),
        );
        let sql = format!(
            "INSERT INTO rustango_jobs (name, payload, max_attempts) VALUES ({p1}, {p2}, {p3})"
        );
        match &self.pool {
            #[cfg(feature = "postgres")]
            Pool::Postgres(pg) => {
                sqlx::query(&sql)
                    .bind(T::NAME)
                    .bind(&value)
                    .bind(max_attempts)
                    .execute(pg)
                    .await
                    .map_err(|e| JobError::Queue(e.to_string()))?;
            }
            #[cfg(feature = "mysql")]
            Pool::Mysql(my) => {
                sqlx::query(&sql)
                    .bind(T::NAME)
                    .bind(sqlx::types::Json(&value))
                    .bind(max_attempts)
                    .execute(my)
                    .await
                    .map_err(|e| JobError::Queue(e.to_string()))?;
            }
            #[cfg(feature = "sqlite")]
            Pool::Sqlite(sq) => {
                // SQLite stores payload as TEXT-as-JSON.
                let json_text =
                    serde_json::to_string(&value).map_err(|e| JobError::Queue(e.to_string()))?;
                sqlx::query(&sql)
                    .bind(T::NAME)
                    .bind(json_text)
                    .bind(max_attempts)
                    .execute(sq)
                    .await
                    .map_err(|e| JobError::Queue(e.to_string()))?;
            }
        }
        // Wake up to one waiting worker so the new job starts running
        // before the next poll tick.
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
        match &self.pool {
            #[cfg(feature = "postgres")]
            Pool::Postgres(pg) => sqlx::query_scalar::<_, i64>(sql)
                .fetch_one(pg)
                .await
                .ok()
                .map_or(0, |n| usize::try_from(n).unwrap_or(0)),
            #[cfg(feature = "mysql")]
            Pool::Mysql(my) => sqlx::query_scalar::<_, i64>(sql)
                .fetch_one(my)
                .await
                .ok()
                .map_or(0, |n| usize::try_from(n).unwrap_or(0)),
            #[cfg(feature = "sqlite")]
            Pool::Sqlite(sq) => sqlx::query_scalar::<_, i64>(sql)
                .fetch_one(sq)
                .await
                .ok()
                .map_or(0, |n| usize::try_from(n).unwrap_or(0)),
        }
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
            // MySQL 8.0+ supports SKIP LOCKED. Two-step in a transaction:
            // 1) SELECT … FOR UPDATE SKIP LOCKED to atomically reserve
            //    a row id; 2) UPDATE that id; 3) SELECT the full row.
            // MySQL doesn't support UPDATE … RETURNING.
            let mut tx = my.begin().await?;
            // MySQL clause order: LIMIT must come *before*
            // `FOR UPDATE SKIP LOCKED` (PG/SQLite accept either order;
            // MySQL is strict and rejects the reverse with a 1064
            // syntax error — that bug silently shipped in v0.38 and
            // is why MySQL workers never picked up dispatched jobs).
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
            // SQLite serializes writers globally, so the pickup is
            // implicitly mutually-exclusive. Use a transaction with
            // BEGIN IMMEDIATE (sqlx's `begin()` for sqlite acquires
            // a write lock) + UPDATE … WHERE id = (SELECT id …) RETURNING.
            let mut tx = sq.begin().await?;
            let now_str = now.to_rfc3339();
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
                let backoff_ms = 1000u64.saturating_mul(1u64 << (next_attempt as u32).min(10));
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
    let p = pool.dialect().placeholder(1);
    let sql = format!("DELETE FROM rustango_jobs WHERE id = {p}");
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let _ = sqlx::query(&sql).bind(id).execute(pg).await;
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let _ = sqlx::query(&sql).bind(id).execute(my).await;
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let _ = sqlx::query(&sql).bind(id).execute(sq).await;
        }
    }
}

async fn schedule_retry(
    pool: &Pool,
    id: i64,
    next_attempt: i32,
    next_run: DateTime<Utc>,
    last_error: &str,
) {
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
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let _ = sqlx::query(&sql)
                .bind(next_attempt)
                .bind(next_run)
                .bind(last_error)
                .bind(id)
                .execute(pg)
                .await;
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let _ = sqlx::query(&sql)
                .bind(next_attempt)
                .bind(next_run)
                .bind(last_error)
                .bind(id)
                .execute(my)
                .await;
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let _ = sqlx::query(&sql)
                .bind(next_attempt)
                .bind(next_run.to_rfc3339())
                .bind(last_error)
                .bind(id)
                .execute(sq)
                .await;
        }
    }
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
    /// Variant of `lookup` that returns the registered &'static str
    /// alongside the handler. Workers need the static name to feed
    /// JobDeadLetter without losing &'static-ness.
    fn lookup_owned(&self, name: &str) -> Option<(super::HandlerFn, &'static str)> {
        let (handler, _) = self.handlers.get(name)?;
        let static_name = self.handlers.keys().find(|k| **k == name).copied()?;
        Some((handler.clone(), static_name))
    }
}

#[cfg(feature = "mysql")]
fn is_mysql_dup_index_error(e: &sqlx::Error) -> bool {
    if let sqlx::Error::Database(db) = e {
        return db.code().as_deref() == Some("42000")
            || db.message().contains("Duplicate key name");
    }
    false
}

/// Best-effort `gethostname` without pulling a dep — read the env var
/// most container runtimes set.
fn hostname() -> Option<String> {
    std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    //! No-DB unit tests. Live PG / SQLite / MySQL coverage lives in
    //! `tests/jobs_*_live.rs`. These tests just cover the pure-Rust bits
    //! that compile on every backend.

    use super::*;

    fn dummy_pool() -> Pool {
        // A pool that never actually connects — used only so we can
        // construct `PgJobQueue` for non-IO assertions.
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
