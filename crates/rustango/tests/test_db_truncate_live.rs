//! Live PG integration test for [`rustango::test_db::with_truncate_after`].
//!
//! Verifies the truncate-after semantic against a real Postgres
//! database: rows COMMIT during the closure (so handlers / signals
//! observing committed state work), and the named tables come back
//! to zero rows once the closure returns.
//!
//! Skipped silently when `DATABASE_URL` is unset (matches the rest
//! of the `_live` suite).

#![cfg(feature = "postgres")]

use rustango::sql::{ExecError, Pool};
use rustango::test_db::{truncate_tables, with_truncate_after};
use sqlx::Row as _;
use std::sync::OnceLock;
use tokio::sync::Mutex;

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
    let Pool::Postgres(pg) = pool else {
        return;
    };
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS "trb_truncate_check" (
            "id" BIGSERIAL PRIMARY KEY,
            "label" VARCHAR(64) NOT NULL
        )"#,
    )
    .execute(pg)
    .await
    .unwrap();
    // Start clean — prior runs may have left rows behind on Err.
    sqlx::query(r#"TRUNCATE "trb_truncate_check" RESTART IDENTITY"#)
        .execute(pg)
        .await
        .unwrap();
}

async fn row_count(pool: &Pool) -> i64 {
    let Pool::Postgres(pg) = pool else {
        return -1;
    };
    sqlx::query(r#"SELECT COUNT(*) AS n FROM "trb_truncate_check""#)
        .fetch_one(pg)
        .await
        .unwrap()
        .get::<i64, _>("n")
}

async fn insert(pool: &Pool, label: &str) {
    let Pool::Postgres(pg) = pool else {
        return;
    };
    sqlx::query(r#"INSERT INTO "trb_truncate_check" (label) VALUES ($1)"#)
        .bind(label)
        .execute(pg)
        .await
        .unwrap();
}

#[tokio::test]
async fn truncate_after_clears_table_when_closure_returns_ok() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    ensure_table(&pool).await;
    assert_eq!(row_count(&pool).await, 0);

    let r: Result<i32, ExecError> = with_truncate_after(&pool, &["trb_truncate_check"], || {
        let pool = pool.clone();
        async move {
            insert(&pool, "a").await;
            insert(&pool, "b").await;
            // Mid-closure read should see committed rows — proves
            // these are real INSERTs, not a transaction-scoped
            // illusion.
            assert_eq!(row_count(&pool).await, 2);
            Ok(99)
        }
    })
    .await;

    assert_eq!(r.unwrap(), 99);
    assert_eq!(
        row_count(&pool).await,
        0,
        "truncate ran on Ok closure return",
    );
}

#[tokio::test]
async fn truncate_after_clears_table_even_when_closure_errs() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    ensure_table(&pool).await;

    let r: Result<(), ExecError> = with_truncate_after(&pool, &["trb_truncate_check"], || {
        let pool = pool.clone();
        async move {
            insert(&pool, "c").await;
            insert(&pool, "d").await;
            assert_eq!(row_count(&pool).await, 2);
            Err::<(), _>(ExecError::EmptyReturning)
        }
    })
    .await;

    assert!(r.is_err(), "closure error propagated through helper");
    assert_eq!(
        row_count(&pool).await,
        0,
        "truncate ran on Err closure return too",
    );
}

#[tokio::test]
async fn truncate_tables_empty_slice_is_noop() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    ensure_table(&pool).await;
    insert(&pool, "survivor").await;
    assert_eq!(row_count(&pool).await, 1);

    truncate_tables(&pool, &[]).await.unwrap();
    assert_eq!(
        row_count(&pool).await,
        1,
        "empty slice is a no-op — row should still be there",
    );

    // Cleanup so the next test starts at zero.
    truncate_tables(&pool, &["trb_truncate_check"])
        .await
        .unwrap();
    assert_eq!(row_count(&pool).await, 0);
}
