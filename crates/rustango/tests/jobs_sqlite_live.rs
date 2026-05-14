#![cfg(all(feature = "sqlite", feature = "jobs-postgres"))]
#![allow(irrefutable_let_patterns)] // Pool enum is single-variant in sqlite-only builds; pattern is refutable on multi-backend builds.
//! Live integration test for the tri-dialect job queue on SQLite.
//!
//! v0.38 slice 27 — the bundled queue (struct name kept as `PgJobQueue`
//! for back-compat) now runs on SQLite via a transaction-bounded
//! `UPDATE … RETURNING` pickup. SQLite serializes writers globally so
//! the pickup is implicitly mutually-exclusive without `FOR UPDATE
//! SKIP LOCKED`.
//!
//! Uses a temp file-backed SQLite database so multiple workers can
//! attach distinct connections (in-memory DBs are per-connection and
//! defeat the multi-worker test).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rustango::jobs::pg::PgJobQueue;
use rustango::jobs::{Job, JobError, JobQueue};
use rustango::sql::Pool;
use serde::{Deserialize, Serialize};

async fn sqlite_pool() -> Pool {
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    let url = format!("sqlite://{}?mode=rwc", tmp.path().display());
    std::mem::forget(tmp);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("sqlite connect");
    PgJobQueue::ensure_table_pool(&Pool::Sqlite(pool.clone()))
        .await
        .expect("ensure rustango_jobs");
    Pool::Sqlite(pool)
}

static RAN_DISPATCH: AtomicUsize = AtomicUsize::new(0);
static RAN_PENDING: AtomicUsize = AtomicUsize::new(0);

#[derive(Serialize, Deserialize)]
struct SqliteDispatchInc;

#[async_trait::async_trait]
impl Job for SqliteDispatchInc {
    const NAME: &'static str = "sqlite_live:dispatch_inc";
    async fn run(&self) -> Result<(), JobError> {
        RAN_DISPATCH.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct SqlitePendingInc;

#[async_trait::async_trait]
impl Job for SqlitePendingInc {
    const NAME: &'static str = "sqlite_live:pending_inc";
    async fn run(&self) -> Result<(), JobError> {
        RAN_PENDING.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn dispatch_persists_and_runs_on_sqlite() {
    RAN_DISPATCH.store(0, Ordering::SeqCst);
    let pool = sqlite_pool().await;

    let q = Arc::new(
        PgJobQueue::with_workers_pool(pool.clone(), 1).poll_interval(Duration::from_millis(50)),
    );
    q.register::<SqliteDispatchInc>().await;
    q.start().await;

    q.dispatch(&SqliteDispatchInc).await.expect("dispatch");

    // Poll until the handler runs, with a 5s timeout.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while RAN_DISPATCH.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    q.shutdown().await;
    assert_eq!(
        RAN_DISPATCH.load(Ordering::SeqCst),
        1,
        "job should have run exactly once on sqlite"
    );
}

#[tokio::test]
async fn reclaim_stuck_jobs_pool_resets_old_locks_on_sqlite() {
    // Coverage for `reclaim_stuck_jobs_pool` — slice 27. Resets
    // `locked_at = NULL` on any row whose lock is older than the
    // threshold. Used in production as a periodic sweep.
    let pool = sqlite_pool().await;
    // Manually insert a stuck row with locked_at well in the past.
    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };
    sqlx::query(
        "INSERT INTO rustango_jobs (name, payload, max_attempts, run_at, locked_at, locked_by) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind("stuck:demo")
    .bind("{}")
    .bind(5_i32)
    .bind("2026-01-01T00:00:00.000Z")
    .bind("2026-01-01T00:00:00.000Z")
    .bind("crashed_worker")
    .execute(sq)
    .await
    .expect("seed stuck row");

    // Sweep with a 1-second threshold — the row's lock is hours old,
    // so it should be reclaimed.
    let reclaimed = PgJobQueue::reclaim_stuck_jobs_pool(&pool, Duration::from_secs(1))
        .await
        .expect("reclaim_stuck_jobs_pool");
    assert_eq!(reclaimed, 1, "should have reclaimed the one stuck row");

    // Verify locked_at is now NULL.
    let locked_at: Option<String> =
        sqlx::query_scalar("SELECT locked_at FROM rustango_jobs WHERE name = ?")
            .bind("stuck:demo")
            .fetch_one(sq)
            .await
            .expect("fetch locked_at");
    assert!(
        locked_at.is_none(),
        "locked_at should be NULL after reclaim"
    );

    // Second call with the same threshold is a no-op (no rows left to
    // reclaim).
    let again = PgJobQueue::reclaim_stuck_jobs_pool(&pool, Duration::from_secs(1))
        .await
        .expect("second sweep");
    assert_eq!(again, 0, "no stuck rows on second sweep");
}

#[tokio::test]
async fn pending_count_decreases_as_workers_run() {
    RAN_PENDING.store(0, Ordering::SeqCst);
    let pool = sqlite_pool().await;
    let q = Arc::new(
        PgJobQueue::with_workers_pool(pool.clone(), 1).poll_interval(Duration::from_millis(50)),
    );
    q.register::<SqlitePendingInc>().await;

    // Dispatch 3 jobs before workers start, so pending_count() is deterministic.
    for _ in 0..3 {
        q.dispatch(&SqlitePendingInc).await.expect("dispatch");
    }
    let pending_before = q.pending_count().await;
    assert_eq!(pending_before, 3, "three pending jobs before start");

    q.start().await;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while q.pending_count().await > 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    q.shutdown().await;
    assert_eq!(
        q.pending_count().await,
        0,
        "all jobs drained from sqlite queue"
    );
    assert_eq!(
        RAN_PENDING.load(Ordering::SeqCst),
        3,
        "all three jobs should have run"
    );
}
