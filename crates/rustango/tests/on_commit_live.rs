#![cfg(feature = "postgres")]
//! Live PG tests for `OnCommitTx` — Django's `transaction.on_commit`.
//! Issue #44. Verifies the after-commit hook fires when (and only
//! when) the wrapping transaction commits.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rustango::sql::{on_commit_tx, sqlx, Pool, PoolTx};

async fn fresh_pool() -> Option<Pool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pg = sqlx::PgPool::connect(&url).await.ok()?;
    sqlx::query(r#"DROP TABLE IF EXISTS "oc_widget" CASCADE"#)
        .execute(&pg)
        .await
        .unwrap();
    sqlx::query(
        r#"
        CREATE TABLE "oc_widget" (
            id BIGSERIAL PRIMARY KEY,
            label VARCHAR(64) NOT NULL
        )
        "#,
    )
    .execute(&pg)
    .await
    .unwrap();
    Some(Pool::Postgres(pg))
}

async fn count_widgets(pool: &Pool) -> i64 {
    let Pool::Postgres(pg) = pool else {
        unreachable!("postgres-only test");
    };
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oc_widget")
        .fetch_one(pg)
        .await
        .unwrap()
}

#[tokio::test]
async fn callback_fires_after_commit() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = Arc::clone(&counter);

    let mut tx = on_commit_tx(&pool).await.unwrap();
    if let PoolTx::Postgres(t) = tx.tx() {
        sqlx::query("INSERT INTO oc_widget(label) VALUES ($1)")
            .bind("alpha")
            .execute(&mut **t)
            .await
            .unwrap();
    }
    tx.on_commit(move || {
        counter_clone.fetch_add(1, Ordering::SeqCst);
    });
    tx.commit().await.unwrap();

    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "callback should have fired"
    );
    assert_eq!(count_widgets(&pool).await, 1, "row should be committed");
}

#[tokio::test]
async fn callback_does_not_fire_after_rollback() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = Arc::clone(&counter);

    let mut tx = on_commit_tx(&pool).await.unwrap();
    if let PoolTx::Postgres(t) = tx.tx() {
        sqlx::query("INSERT INTO oc_widget(label) VALUES ($1)")
            .bind("beta")
            .execute(&mut **t)
            .await
            .unwrap();
    }
    tx.on_commit(move || {
        counter_clone.fetch_add(1, Ordering::SeqCst);
    });
    tx.rollback().await.unwrap();

    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "callback must NOT fire on rollback"
    );
    assert_eq!(count_widgets(&pool).await, 0, "row should be rolled back");
}

#[tokio::test]
async fn callbacks_fire_in_registration_order() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let order = Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));

    let mut tx = on_commit_tx(&pool).await.unwrap();
    for i in 0..5_u32 {
        let order = Arc::clone(&order);
        tx.on_commit(move || {
            order.lock().unwrap().push(i);
        });
    }
    tx.commit().await.unwrap();

    let observed = order.lock().unwrap().clone();
    assert_eq!(observed, vec![0, 1, 2, 3, 4], "registration order");
}

#[tokio::test]
async fn pending_count_reflects_queued_callbacks() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let mut tx = on_commit_tx(&pool).await.unwrap();
    assert_eq!(tx.pending(), 0);
    tx.on_commit(|| {});
    assert_eq!(tx.pending(), 1);
    tx.on_commit(|| {});
    tx.on_commit(|| {});
    assert_eq!(tx.pending(), 3);
    tx.rollback().await.unwrap();
}
