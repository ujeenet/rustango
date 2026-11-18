//! `on_commit` callbacks of an `atomic` block, on every backend. Nesting
//! and cancellation live in `atomic_savepoints_tri.rs`.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rustango::sql::{on_commit, on_commit_pending, CounterPool as _, ExecError, Pool, SqlError};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "oc_widget")]
#[allow(dead_code)]
pub struct Widget {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 64)]
    pub label: String,
}

fn bail() -> ExecError {
    ExecError::Sql(SqlError::EmptyInList)
}

async fn widgets(pool: &Pool) -> i64 {
    Widget::objects().count(pool).await.unwrap()
}

/// A callback fires after the commit, never after a rollback.
async fn callback_fires_only_after_commit(pool: &Pool) {
    let fired = Arc::new(AtomicUsize::new(0));
    let (f, q) = (Arc::clone(&fired), pool.clone());
    let res: Result<(), ExecError> = rustango::atomic!(pool, |_tx| {
        Widget {
            id: 1,
            label: "beta".into(),
        }
        .insert(&q)
        .await?;
        on_commit(move || {
            f.fetch_add(1, Ordering::SeqCst);
        });
        Err(bail())
    })
    .await;
    assert!(res.is_err());
    assert_eq!(fired.load(Ordering::SeqCst), 0, "fired on rollback");
    assert_eq!(widgets(pool).await, 0);

    let (f, q) = (Arc::clone(&fired), pool.clone());
    rustango::atomic!(pool, |_tx| {
        Widget {
            id: 2,
            label: "alpha".into(),
        }
        .insert(&q)
        .await?;
        on_commit(move || {
            f.fetch_add(1, Ordering::SeqCst);
        });
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(fired.load(Ordering::SeqCst), 1);
    assert_eq!(widgets(pool).await, 1);
}

async fn callbacks_fire_in_registration_order(pool: &Pool) {
    let order = Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
    let seen = Arc::clone(&order);
    rustango::atomic!(pool, |_tx| {
        for i in 0..5_u32 {
            let order = Arc::clone(&seen);
            on_commit(move || order.lock().unwrap().push(i));
        }
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(*order.lock().unwrap(), vec![0, 1, 2, 3, 4]);
}

async fn on_commit_pending_reflects_queue_depth(pool: &Pool) {
    rustango::atomic!(pool, |_tx| {
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

/// A released nested block's callback joins the outer queue and drops with
/// the outer rollback.
async fn outer_rollback_drops_both_queues(pool: &Pool) {
    let fired = Arc::new(AtomicUsize::new(0));
    let (outer, inner) = (Arc::clone(&fired), Arc::clone(&fired));
    let q = pool.clone();
    let res: Result<(), ExecError> = rustango::atomic!(pool, |_tx| {
        on_commit(move || {
            outer.fetch_add(1, Ordering::SeqCst);
        });
        rustango::atomic!(&q, |_sp| {
            on_commit(move || {
                inner.fetch_add(1, Ordering::SeqCst);
            });
            Ok::<_, ExecError>(())
        })
        .await?;
        assert_eq!(on_commit_pending(), 2, "the inner callback moved out");
        Err(bail())
    })
    .await;
    assert!(res.is_err());
    assert_eq!(fired.load(Ordering::SeqCst), 0);
}

tri_dialect_test! {
    model: Widget,
    scenarios: [
        callback_fires_only_after_commit,
        callbacks_fire_in_registration_order,
        on_commit_pending_reflects_queue_depth,
        outer_rollback_drops_both_queues,
    ],
}

/// Outside a block there is nothing queued, and nothing to queue into.
#[tokio::test]
async fn on_commit_pending_outside_atomic_returns_zero() {
    assert_eq!(on_commit_pending(), 0);
}

#[tokio::test]
#[should_panic(expected = "called outside an `atomic` block")]
async fn on_commit_outside_atomic_panics() {
    on_commit(|| {});
}
