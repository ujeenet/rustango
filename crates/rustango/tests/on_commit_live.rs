#![cfg(feature = "postgres")]
//! Live PG tests for `rustango::sql::atomic` + `on_commit` —
//! closure-scoped transactions with after-commit hooks. Issue #44.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rustango::sql::{on_commit, on_commit_pending, sqlx, ExecError, Pool, PoolTx};

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

    rustango::atomic!(&pool, |tx| {
        if let PoolTx::Postgres(t) = tx {
            sqlx::query("INSERT INTO oc_widget(label) VALUES ($1)")
                .bind("alpha")
                .execute(&mut **t)
                .await
                .map_err(ExecError::from)?;
        }
        on_commit(move || {
            counter_clone.fetch_add(1, Ordering::SeqCst);
        });
        Ok(())
    })
    .await
    .unwrap();

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

    let result: Result<(), ExecError> = rustango::atomic!(&pool, |tx| {
        if let PoolTx::Postgres(t) = tx {
            sqlx::query("INSERT INTO oc_widget(label) VALUES ($1)")
                .bind("beta")
                .execute(&mut **t)
                .await
                .map_err(ExecError::from)?;
        }
        on_commit(move || {
            counter_clone.fetch_add(1, Ordering::SeqCst);
        });
        // Intentional bailout — rolls back.
        Err(ExecError::Sql(rustango::sql::SqlError::EmptyInList))
    })
    .await;

    assert!(result.is_err(), "atomic should propagate the closure's Err");
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
    let order_for_closure = Arc::clone(&order);

    rustango::atomic!(&pool, |_tx| {
        for i in 0..5_u32 {
            let order = Arc::clone(&order_for_closure);
            on_commit(move || {
                order.lock().unwrap().push(i);
            });
        }
        Ok(())
    })
    .await
    .unwrap();

    let observed = order.lock().unwrap().clone();
    assert_eq!(observed, vec![0, 1, 2, 3, 4], "registration order");
}

#[tokio::test]
async fn on_commit_pending_reflects_queue_depth() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    rustango::atomic!(&pool, |_tx| {
        assert_eq!(on_commit_pending(), 0);
        on_commit(|| {});
        assert_eq!(on_commit_pending(), 1);
        on_commit(|| {});
        on_commit(|| {});
        assert_eq!(on_commit_pending(), 3);
        Ok::<_, ExecError>(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn on_commit_pending_outside_atomic_returns_zero() {
    // `on_commit_pending` is safe to call outside an atomic scope —
    // returns 0 rather than panicking. (Only `on_commit` itself
    // panics outside scope, since calling it would otherwise drop
    // the callback into the void.)
    assert_eq!(on_commit_pending(), 0);
}

#[tokio::test]
#[should_panic(expected = "called outside an `atomic` block")]
async fn on_commit_outside_atomic_panics() {
    on_commit(|| {});
}

// ---------- nested atomic blocks ----------
//
// Each `atomic!` call sets its own task-local queue, so callbacks
// registered inside an inner block belong to the INNER atomic and
// follow ITS commit/rollback decision. The outer block's queue is
// shielded — restored when the inner scope ends. These tests pin
// the property since it's load-bearing for any nested-tx user code.

#[tokio::test]
async fn nested_atomic_inner_rollback_isolated_from_outer() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let outer = Arc::new(AtomicUsize::new(0));
    let inner = Arc::new(AtomicUsize::new(0));
    let outer_clone = Arc::clone(&outer);
    let inner_clone = Arc::clone(&inner);

    rustango::atomic!(&pool, |_outer_tx| {
        on_commit(move || {
            outer_clone.fetch_add(1, Ordering::SeqCst);
        });
        // Inner block fails → rollback → inner callback dropped.
        let inner_res: Result<(), ExecError> = rustango::atomic!(&pool, |_inner_tx| {
            on_commit(move || {
                inner_clone.fetch_add(1, Ordering::SeqCst);
            });
            Err(ExecError::Sql(rustango::sql::SqlError::EmptyInList))
        })
        .await;
        assert!(inner_res.is_err(), "inner should roll back");
        // Outer continues + commits successfully.
        Ok(())
    })
    .await
    .unwrap();

    assert_eq!(
        outer.load(Ordering::SeqCst),
        1,
        "outer callback should fire — outer committed"
    );
    assert_eq!(
        inner.load(Ordering::SeqCst),
        0,
        "inner callback must NOT fire — inner rolled back"
    );
}

#[tokio::test]
async fn nested_atomic_outer_rollback_drops_both_queues() {
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let outer = Arc::new(AtomicUsize::new(0));
    let inner = Arc::new(AtomicUsize::new(0));
    let outer_clone = Arc::clone(&outer);
    let inner_clone = Arc::clone(&inner);

    let result: Result<(), ExecError> = rustango::atomic!(&pool, |_outer_tx| {
        on_commit(move || {
            outer_clone.fetch_add(1, Ordering::SeqCst);
        });
        // Inner commits successfully — its callback fires.
        rustango::atomic!(&pool, |_inner_tx| {
            on_commit(move || {
                inner_clone.fetch_add(1, Ordering::SeqCst);
            });
            Ok::<_, ExecError>(())
        })
        .await
        .unwrap();
        // Outer then bails out — its OWN callback drops.
        Err(ExecError::Sql(rustango::sql::SqlError::EmptyInList))
    })
    .await;

    assert!(result.is_err(), "outer should roll back");
    assert_eq!(
        inner.load(Ordering::SeqCst),
        1,
        "inner already fired before outer rolled back (correct — inner had its own scope)"
    );
    assert_eq!(
        outer.load(Ordering::SeqCst),
        0,
        "outer callback must NOT fire — outer rolled back"
    );
}
