#![cfg(all(feature = "mysql", feature = "jobs-postgres"))]
//! v0.41 — live MySQL parity for the tri-dialect job queue.
//!
//! Mirrors `jobs_sqlite_live.rs`. MySQL 8.0+ uses `FOR UPDATE SKIP
//! LOCKED` for atomic multi-worker pickup (same path as Postgres);
//! this test proves the queue end-to-end against a real `mysql://`
//! pool.
//!
//! Reads `MYSQL_TEST_URL`. If unset, every test returns silently so
//! `cargo test` stays green offline. CI / local devs run
//!
//!   docker compose up -d mysql
//!   export MYSQL_TEST_URL=mysql://rustango:rustango@127.0.0.1:3406/rustango_test
//!
//! to actually exercise these paths.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustango::jobs::pg::PgJobQueue;
use rustango::jobs::{Job, JobError, JobQueue};
use rustango::sql::Pool;
use serde::{Deserialize, Serialize};

/// Serialize MySQL tests within this binary. `rustango_jobs` is a
/// shared table and `cargo test` would otherwise run these in
/// parallel — one test's `DROP TABLE` would yank rows another test
/// just dispatched. The PG/SQLite suites avoid this with file-backed
/// or schema-isolated DBs; MySQL doesn't have an equivalently cheap
/// per-test isolation, so a process-wide lock is the simplest fix.
fn serial_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

async fn mysql_pool() -> Option<Pool> {
    let url = std::env::var("MYSQL_TEST_URL").ok()?;
    let mp = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("mysql connect");
    let _ = sqlx::query("DROP TABLE IF EXISTS rustango_jobs")
        .execute(&mp)
        .await;
    let pool = Pool::Mysql(mp);
    PgJobQueue::ensure_table_pool(&pool)
        .await
        .expect("ensure rustango_jobs");
    Some(pool)
}

static RAN_DISPATCH: AtomicUsize = AtomicUsize::new(0);
static RAN_PENDING: AtomicUsize = AtomicUsize::new(0);

#[derive(Serialize, Deserialize)]
struct MysqlDispatchInc;

#[async_trait::async_trait]
impl Job for MysqlDispatchInc {
    const NAME: &'static str = "mysql_live:dispatch_inc";
    async fn run(&self) -> Result<(), JobError> {
        RAN_DISPATCH.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct MysqlPendingInc;

#[async_trait::async_trait]
impl Job for MysqlPendingInc {
    const NAME: &'static str = "mysql_live:pending_inc";
    async fn run(&self) -> Result<(), JobError> {
        RAN_PENDING.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn dispatch_persists_and_runs_on_mysql() {
    let _serial = serial_lock().lock().await;
    let Some(pool) = mysql_pool().await else {
        return;
    };
    RAN_DISPATCH.store(0, Ordering::SeqCst);

    let q = Arc::new(
        PgJobQueue::with_workers_pool(pool.clone(), 1).poll_interval(Duration::from_millis(50)),
    );
    q.register::<MysqlDispatchInc>().await;
    q.start().await;

    q.dispatch(&MysqlDispatchInc).await.expect("dispatch");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while RAN_DISPATCH.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    q.shutdown().await;
    assert_eq!(
        RAN_DISPATCH.load(Ordering::SeqCst),
        1,
        "job should have run exactly once on mysql"
    );
}

#[tokio::test]
async fn reclaim_stuck_jobs_pool_resets_old_locks_on_mysql() {
    let _serial = serial_lock().lock().await;
    let Some(pool) = mysql_pool().await else {
        return;
    };
    let Pool::Mysql(my) = &pool else {
        unreachable!()
    };
    // MySQL `rustango_jobs.locked_at` is `DATETIME(6)`; bind as
    // `chrono::DateTime<Utc>` which sqlx encodes cleanly. The
    // SQLite test uses ISO-8601 strings because SQLite stores the
    // column as TEXT.
    let stuck_time: chrono::DateTime<chrono::Utc> =
        chrono::Utc::now() - chrono::Duration::hours(24);
    sqlx::query(
        "INSERT INTO rustango_jobs \
         (name, payload, max_attempts, run_at, locked_at, locked_by) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind("stuck:demo")
    .bind("{}")
    .bind(5_i32)
    .bind(stuck_time)
    .bind(stuck_time)
    .bind("crashed_worker")
    .execute(my)
    .await
    .expect("seed stuck row");

    let reclaimed = PgJobQueue::reclaim_stuck_jobs_pool(&pool, Duration::from_secs(1))
        .await
        .expect("reclaim_stuck_jobs_pool");
    assert_eq!(reclaimed, 1, "should have reclaimed the one stuck row");

    let locked_at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT locked_at FROM rustango_jobs WHERE name = ?")
            .bind("stuck:demo")
            .fetch_one(my)
            .await
            .expect("fetch locked_at");
    assert!(
        locked_at.is_none(),
        "locked_at should be NULL after reclaim"
    );

    let again = PgJobQueue::reclaim_stuck_jobs_pool(&pool, Duration::from_secs(1))
        .await
        .expect("second sweep");
    assert_eq!(again, 0, "no stuck rows on second sweep");
}

#[tokio::test]
async fn pending_count_decreases_as_workers_run_on_mysql() {
    let _serial = serial_lock().lock().await;
    let Some(pool) = mysql_pool().await else {
        return;
    };
    RAN_PENDING.store(0, Ordering::SeqCst);

    let q = Arc::new(
        PgJobQueue::with_workers_pool(pool.clone(), 1).poll_interval(Duration::from_millis(50)),
    );
    q.register::<MysqlPendingInc>().await;

    for _ in 0..3 {
        q.dispatch(&MysqlPendingInc).await.expect("dispatch");
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
        "all jobs drained from mysql queue"
    );
    assert_eq!(
        RAN_PENDING.load(Ordering::SeqCst),
        3,
        "all three jobs should have run"
    );
}
