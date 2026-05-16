//! Live PG integration test for [`rustango::test_db::with_rollback`].
//!
//! Verifies the rollback semantic against a real Postgres database:
//! rows inserted inside the closure are visible to the closure but
//! disappear after it returns.
//!
//! Skipped silently when `DATABASE_URL` is unset (matches the rest
//! of the `_live` suite). Run with:
//!
//! ```text
//! DATABASE_URL=postgres://... cargo test --test test_db_rollback_live
//! ```

#![cfg(feature = "postgres")]

use rustango::sql::{ExecError, Pool};
use rustango::test_db::with_rollback;
use sqlx::Row as _;
use std::sync::OnceLock;
use tokio::sync::Mutex;

/// Suite-wide mutex — the table is shared across tests in this file
/// and we want one test at a time to mutate it (process-global state
/// per the project's testing convention).
fn live_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<Pool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pg = sqlx::PgPool::connect(&url).await.ok()?;
    Some(Pool::Postgres(pg))
}

async fn ensure_table(pool: &Pool) {
    // Idempotent — re-runs are no-ops.
    let Pool::Postgres(pg) = pool else {
        return;
    };
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS "trb_rollback_check" (
            "id" BIGSERIAL PRIMARY KEY,
            "label" VARCHAR(64) NOT NULL
        )"#,
    )
    .execute(pg)
    .await
    .unwrap();
    sqlx::query(r#"TRUNCATE "trb_rollback_check""#)
        .execute(pg)
        .await
        .unwrap();
}

async fn row_count(pool: &Pool) -> i64 {
    let Pool::Postgres(pg) = pool else {
        return -1;
    };
    sqlx::query(r#"SELECT COUNT(*) AS n FROM "trb_rollback_check""#)
        .fetch_one(pg)
        .await
        .unwrap()
        .get::<i64, _>("n")
}

#[tokio::test]
async fn rollback_discards_inserts_on_ok_return() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    ensure_table(&pool).await;
    assert_eq!(row_count(&pool).await, 0);

    let inner: Result<i64, ExecError> = with_rollback(&pool, |tx| {
        Box::pin(async move {
            // Use sqlx directly through the PoolTx to insert rows.
            // The PoolTx variants expose the inner sqlx Transaction.
            let rustango::sql::PoolTx::Postgres(t) = tx else {
                panic!("PG pool variant expected");
            };
            sqlx::query(r#"INSERT INTO "trb_rollback_check" (label) VALUES ('a'), ('b')"#)
                .execute(&mut **t)
                .await?;
            let n: i64 = sqlx::query(r#"SELECT COUNT(*) AS n FROM "trb_rollback_check""#)
                .fetch_one(&mut **t)
                .await?
                .get("n");
            assert_eq!(n, 2, "rows visible inside closure");
            Ok::<i64, ExecError>(n)
        })
    })
    .await;

    assert!(inner.is_ok());
    assert_eq!(inner.unwrap(), 2, "closure return value preserved");
    // The two rows are gone — rollback fired on Ok.
    assert_eq!(row_count(&pool).await, 0);
}

#[tokio::test]
async fn rollback_fires_on_closure_err_too() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    ensure_table(&pool).await;

    let r: Result<(), ExecError> = with_rollback(&pool, |tx| {
        Box::pin(async move {
            let rustango::sql::PoolTx::Postgres(t) = tx else {
                panic!("PG pool variant expected");
            };
            sqlx::query(r#"INSERT INTO "trb_rollback_check" (label) VALUES ('c')"#)
                .execute(&mut **t)
                .await?;
            // Synthesize an error — should still roll back the insert.
            Err::<(), _>(ExecError::EmptyReturning)
        })
    })
    .await;

    assert!(r.is_err(), "closure error propagated");
    assert_eq!(row_count(&pool).await, 0, "rollback fired even on Err");
}
