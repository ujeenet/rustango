#![cfg(feature = "postgres")]
//! Live PG test for `select_for_update` runtime semantic (issue #21).
//! Verifies `SKIP LOCKED` produces the canonical "claim next available
//! row" pattern: two concurrent transactions each grab a different
//! row, neither blocks. Skips silently when `DATABASE_URL` is unset.

use std::sync::OnceLock;

use rustango::core::Column as _;
use rustango::sql::{sqlx, Auto};
use rustango::Model;
use tokio::sync::Mutex;

#[derive(Model, Debug, Clone)]
#[rustango(table = "sfu_live_job")]
#[allow(dead_code)]
pub struct Job {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 20)]
    pub status: String,
    pub priority: i32,
}

fn lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn fresh_pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::PgPool::connect(&url).await.ok()?;
    sqlx::query(r#"DROP TABLE IF EXISTS "sfu_live_job" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        r#"
        CREATE TABLE "sfu_live_job" (
            id BIGSERIAL PRIMARY KEY,
            status VARCHAR(20) NOT NULL,
            priority INTEGER NOT NULL
        )
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    // Seed 3 pending jobs at distinct priorities.
    for (status, priority) in [("pending", 10), ("pending", 20), ("pending", 30)] {
        let mut j = Job {
            id: Auto::default(),
            status: status.into(),
            priority,
        };
        j.insert_pool(&(&pool).clone().into()).await.unwrap();
    }
    Some(pool)
}

/// Canonical "claim next available row" pattern: two concurrent
/// transactions each call `SELECT … FOR UPDATE SKIP LOCKED LIMIT 1`.
/// First transaction grabs row 1; second SKIP LOCKED transaction
/// skips row 1 (held by tx1) and grabs row 2 instead — they don't
/// block each other.
#[tokio::test]
async fn skip_locked_lets_two_workers_claim_different_rows() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let mut tx1 = pool.begin().await.unwrap();
    let mut tx2 = pool.begin().await.unwrap();

    // Worker 1 grabs the lowest-priority pending row.
    let claim1: Vec<Job> = Job::objects()
        .where_(Job::status.eq("pending"))
        .order_by(&[("priority", false)])
        .limit(1)
        .select_for_update()
        .skip_locked()
        .fetch_on(&mut *tx1)
        .await
        .unwrap();
    assert_eq!(claim1.len(), 1, "tx1 grabbed one row");
    let first_id = match claim1[0].id {
        Auto::Set(v) => v,
        Auto::Unset => unreachable!(),
    };

    // Worker 2 should skip the row locked by tx1 and grab the next.
    let claim2: Vec<Job> = Job::objects()
        .where_(Job::status.eq("pending"))
        .order_by(&[("priority", false)])
        .limit(1)
        .select_for_update()
        .skip_locked()
        .fetch_on(&mut *tx2)
        .await
        .unwrap();
    assert_eq!(claim2.len(), 1, "tx2 also grabbed a row (no block)");
    let second_id = match claim2[0].id {
        Auto::Set(v) => v,
        Auto::Unset => unreachable!(),
    };

    assert_ne!(
        first_id, second_id,
        "tx1 and tx2 claimed different rows (SKIP LOCKED)"
    );

    tx1.rollback().await.unwrap();
    tx2.rollback().await.unwrap();
}

/// `NOWAIT` on a row already locked by another transaction surfaces a
/// driver error immediately (Postgres SQLSTATE 55P03 "lock_not_available")
/// instead of waiting for the lock holder. Verifies the writer's
/// `NOWAIT` keyword reaches PG and triggers the right behaviour.
#[tokio::test]
async fn nowait_errors_immediately_on_contended_row() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let mut tx1 = pool.begin().await.unwrap();
    let mut tx2 = pool.begin().await.unwrap();

    // tx1 locks all three rows.
    let _claim1: Vec<Job> = Job::objects()
        .select_for_update()
        .fetch_on(&mut *tx1)
        .await
        .unwrap();

    // tx2 with NOWAIT should error rather than block.
    let r2: Result<Vec<Job>, _> = Job::objects()
        .select_for_update()
        .nowait()
        .fetch_on(&mut *tx2)
        .await;
    assert!(r2.is_err(), "tx2 NOWAIT should error on contended rows");
    let msg = format!("{:?}", r2.err().unwrap());
    // PG reports SQLSTATE 55P03 (lock_not_available); the sqlx error
    // surfaces the underlying message. Don't pin the exact wording,
    // just confirm it mentioned lock acquisition.
    assert!(
        msg.to_lowercase().contains("lock") || msg.contains("55P03"),
        "error mentions lock contention: {msg}"
    );

    tx1.rollback().await.unwrap();
    let _ = tx2.rollback().await;
}

/// `select_for_update` against a bare `&PgPool` — the SQL is
/// well-formed and the statement executes, but PG treats each
/// standalone statement as an implicit transaction that ends the
/// moment the SELECT returns. The lock IS acquired for the duration
/// of the statement, then immediately released — so this shape is
/// only useful for testing the writer / smoke-checking the query.
/// **Production "claim next available row" workflows must use an
/// explicit `pool.begin()` transaction** (see the SKIP LOCKED test
/// above for the canonical pattern), otherwise the lock is released
/// before any follow-up `UPDATE` runs.
#[tokio::test]
async fn select_for_update_against_bare_pool_runs_cleanly() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    // Fetcher trait deleted in T1.8 wave 3 — fetch_on is inherent on QuerySet.
    let rows: Vec<Job> = Job::objects()
        .where_(Job::status.eq("pending"))
        .select_for_update()
        .skip_locked()
        .fetch_on(&pool)
        .await
        .unwrap();
    // No other tx is competing — all 3 pending rows come back.
    assert_eq!(rows.len(), 3);

    // Tear down.
    sqlx::query(r#"DROP TABLE "sfu_live_job""#)
        .execute(&pool)
        .await
        .unwrap();
}

/// Sanity check on the SQL string: when JOINs are present, `OF` only
/// locks the named table — without it the lock applies to both.
/// (The actual JOIN test would need a second table; here we just pin
/// the emitted shape and confirm it executes.)
#[tokio::test]
async fn for_update_with_of_executes_cleanly() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let mut tx = pool.begin().await.unwrap();
    let rows: Vec<Job> = Job::objects()
        .select_for_update()
        .of(&["sfu_live_job"])
        .fetch_on(&mut *tx)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
    tx.rollback().await.unwrap();
}
