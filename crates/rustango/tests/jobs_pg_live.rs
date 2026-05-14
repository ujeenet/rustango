#![cfg(feature = "postgres")]
//! Live integration test for the Postgres-backed job queue.
//!
//! Reads `DATABASE_URL`. If unset, every test returns silently so
//! `cargo test` stays green offline. CI exercises the actual queue.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rustango::jobs::pg::PgJobQueue;
use rustango::jobs::{Job, JobError, JobQueue};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tokio::sync::Mutex;

/// Suite-wide lock. Every test in this file `DELETE FROM rustango_jobs`
/// + dispatches against the shared `DATABASE_URL` pool, and the
/// `RAN_INC` static counter is read by assertions; under cargo's
/// default parallel harness two tests would clobber each other's rows
/// and counter resets.
fn live_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = PgPool::connect(&url)
        .await
        .expect("connect to DATABASE_URL");
    // Reset the table between runs so tests are deterministic.
    PgJobQueue::ensure_table(&pool)
        .await
        .expect("ensure rustango_jobs");
    sqlx::query("DELETE FROM rustango_jobs")
        .execute(&pool)
        .await
        .expect("clear rustango_jobs");
    Some(pool)
}

static RAN_INC: AtomicUsize = AtomicUsize::new(0);

#[derive(Serialize, Deserialize)]
struct PgInc;

#[async_trait::async_trait]
impl Job for PgInc {
    const NAME: &'static str = "pg_live:increment";
    async fn run(&self) -> Result<(), JobError> {
        RAN_INC.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn dispatch_persists_and_runs() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    RAN_INC.store(0, Ordering::SeqCst);

    let q = Arc::new(
        PgJobQueue::with_workers(pool.clone(), 2).poll_interval(Duration::from_millis(100)),
    );
    q.register::<PgInc>().await;
    q.start().await;

    q.dispatch(&PgInc).await.expect("dispatch");

    // Wait briefly for a worker to pick it up.
    for _ in 0..40 {
        if RAN_INC.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(RAN_INC.load(Ordering::SeqCst), 1);

    // Row should be deleted on success.
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rustango_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0, "row should be deleted after successful run");

    q.shutdown().await;
}

#[derive(Serialize, Deserialize)]
struct PgRetry;

static RETRY_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);

#[async_trait::async_trait]
impl Job for PgRetry {
    const NAME: &'static str = "pg_live:retry";
    const MAX_ATTEMPTS: u32 = 3;
    async fn run(&self) -> Result<(), JobError> {
        let n = RETRY_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        if n < 1 {
            Err(JobError::Retryable(format!("attempt {n}")))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn retryable_failure_reschedules_with_backoff() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    RETRY_ATTEMPTS.store(0, Ordering::SeqCst);

    let q = Arc::new(
        PgJobQueue::with_workers(pool.clone(), 1).poll_interval(Duration::from_millis(100)),
    );
    q.register::<PgRetry>().await;
    q.start().await;

    q.dispatch(&PgRetry).await.unwrap();

    // First attempt fails immediately, then a 2-second backoff before retry,
    // so wait ~3.5s.
    tokio::time::sleep(Duration::from_millis(3500)).await;

    assert!(
        RETRY_ATTEMPTS.load(Ordering::SeqCst) >= 2,
        "expected at least 2 attempts, got {}",
        RETRY_ATTEMPTS.load(Ordering::SeqCst)
    );

    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rustango_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0, "row should be cleared after eventual success");

    q.shutdown().await;
}

#[tokio::test]
async fn reclaim_stuck_jobs_resets_lock() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };

    // Insert a row that's been "locked" for 10 minutes.
    sqlx::query(
        "INSERT INTO rustango_jobs (name, payload, max_attempts, locked_at, locked_by)
         VALUES ('zombie', '{}'::JSONB, 3, NOW() - INTERVAL '10 minutes', 'crashed-worker')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let reclaimed = PgJobQueue::reclaim_stuck_jobs(&pool, Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(reclaimed, 1);

    let still_locked: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM rustango_jobs WHERE locked_at IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(still_locked, 0);
}

#[tokio::test]
async fn pending_count_reflects_unlocked_rows() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };

    // No workers — nothing picks them up.
    let q = PgJobQueue::with_workers(pool.clone(), 0);
    q.register::<PgInc>().await;
    for _ in 0..3 {
        q.dispatch(&PgInc).await.unwrap();
    }
    assert_eq!(q.pending_count().await, 3);
}
