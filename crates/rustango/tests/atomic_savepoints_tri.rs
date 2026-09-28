//! A nested `atomic()` on the same pool runs in a savepoint on the outer
//! connection, and `on_commit` waits for the outermost commit (#1666).
//! Every scenario uses one-connection pools, so a second transaction on
//! the same pool would deadlock; `within` turns that into a failure.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use rustango::sql::{on_commit, AtomicTx, ExecError, FetcherPool as _, Pool, SqlError};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "asp_note")]
#[allow(dead_code)]
pub struct Note {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 20)]
    pub label: String,
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Note>(pool).await;
}

/// A second, one-connection pool on the same database.
async fn one_conn(pool: &Pool) -> Pool {
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(p) => Pool::Postgres(
            rustango::sql::sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect_with((*p.connect_options()).clone())
                .await
                .unwrap(),
        ),
        #[cfg(feature = "mysql")]
        Pool::Mysql(p) => Pool::Mysql(
            rustango::sql::sqlx::mysql::MySqlPoolOptions::new()
                .max_connections(1)
                .connect_with((*p.connect_options()).clone())
                .await
                .unwrap(),
        ),
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(p) => Pool::Sqlite(
            rustango::sql::sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with((*p.connect_options()).clone())
                .await
                .unwrap(),
        ),
    }
}

async fn put(tx: &AtomicTx, id: i64) -> Result<(), ExecError> {
    let row = Note {
        id,
        label: format!("n{id}"),
    };
    row.insert_tx(&mut *tx.lock().await?).await
}

fn bail() -> ExecError {
    ExecError::Sql(SqlError::EmptyInList)
}

async fn ids(pool: &Pool) -> Vec<i64> {
    let rows: Vec<Note> = Note::objects()
        .order_by(&[("id", false)])
        .fetch(pool)
        .await
        .unwrap();
    rows.into_iter().map(|r| r.id).collect()
}

/// Bound a scenario so a deadlock fails instead of hanging.
async fn within<F: Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .expect("deadlock: a nested block waited for a second connection")
}

/// `catch_unwind` for a future, without the `futures` crate.
struct CatchUnwind<F>(Pin<Box<F>>);

impl<F: Future> Future for CatchUnwind<F> {
    type Output = std::thread::Result<F::Output>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = self.0.as_mut();
        match std::panic::catch_unwind(AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(v)) => Poll::Ready(Ok(v)),
            Err(p) => Poll::Ready(Err(p)),
        }
    }
}

async fn outer_rollback_drops_inner_write(pool: &Pool) {
    let p = one_conn(pool).await;
    let q = p.clone();
    let res: Result<(), ExecError> = within(rustango::atomic!(&p, |tx| {
        put(tx, 1).await?;
        rustango::atomic!(&q, |sp| { put(sp, 2).await })
            .await
            .expect("savepoint releases");
        Err(bail())
    }))
    .await;
    assert!(res.is_err());
    assert_eq!(
        ids(pool).await,
        Vec::<i64>::new(),
        "outer rollback undoes the inner write"
    );
}

async fn inner_rollback_keeps_outer_write(pool: &Pool) {
    let p = one_conn(pool).await;
    let q = p.clone();
    within(rustango::atomic!(&p, |tx| {
        put(tx, 1).await?;
        let inner: Result<(), ExecError> = rustango::atomic!(&q, |sp| {
            put(sp, 2).await?;
            Err(bail())
        })
        .await;
        assert!(inner.is_err());
        put(tx, 3).await?;
        Ok(())
    }))
    .await
    .expect("outer commits");
    assert_eq!(ids(pool).await, vec![1, 3]);
}

async fn on_commit_waits_for_outermost(pool: &Pool) {
    let p = one_conn(pool).await;
    let (p1, p2, p3) = (p.clone(), p.clone(), p.clone());
    let fired = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let (f1, f2, d1, seen) = (
        Arc::clone(&fired),
        Arc::clone(&fired),
        Arc::clone(&dropped),
        Arc::clone(&fired),
    );
    within(rustango::atomic!(&p, |_tx| {
        rustango::atomic!(&p1, |_sp| {
            on_commit(move || {
                f1.fetch_add(1, Ordering::SeqCst);
            });
            rustango::atomic!(&p2, |sp2| {
                put(sp2, 1).await?;
                on_commit(move || {
                    f2.fetch_add(1, Ordering::SeqCst);
                });
                Ok(())
            })
            .await
        })
        .await?;
        let _ = rustango::atomic!(&p3, |_sp| {
            on_commit(move || {
                d1.fetch_add(1, Ordering::SeqCst);
            });
            Err::<(), _>(bail())
        })
        .await;
        assert_eq!(
            seen.load(Ordering::SeqCst),
            0,
            "nothing fires before the outer commit"
        );
        Ok(())
    }))
    .await
    .unwrap();
    assert_eq!(
        fired.load(Ordering::SeqCst),
        2,
        "released callbacks fire at the end"
    );
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        0,
        "rolled-back block drops its callback"
    );
    assert_eq!(ids(pool).await, vec![1]);
}

async fn cancelled_nested_block_rolls_back(pool: &Pool) {
    let p = one_conn(pool).await;
    let q = p.clone();
    let wrote = Arc::new(AtomicBool::new(false));
    let fired = Arc::new(AtomicUsize::new(0));
    let (w, f) = (Arc::clone(&wrote), Arc::clone(&fired));
    within(rustango::atomic!(&p, |tx| {
        put(tx, 1).await?;
        let slow = rustango::atomic!(&q, |sp| {
            put(sp, 2).await?;
            w.store(true, Ordering::SeqCst);
            on_commit(move || {
                f.fetch_add(1, Ordering::SeqCst);
            });
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok::<_, ExecError>(())
        });
        assert!(tokio::time::timeout(Duration::from_millis(300), slow)
            .await
            .is_err());
        put(tx, 3).await?;
        Ok(())
    }))
    .await
    .unwrap();
    assert!(
        wrote.load(Ordering::SeqCst),
        "the inner write ran before the cancel"
    );
    assert_eq!(
        ids(pool).await,
        vec![1, 3],
        "the cancelled block's write is gone"
    );
    assert_eq!(fired.load(Ordering::SeqCst), 0);
}

async fn cancelled_nested_block_last_is_not_committed(pool: &Pool) {
    let p = one_conn(pool).await;
    let q = p.clone();
    let wrote = Arc::new(AtomicBool::new(false));
    let w = Arc::clone(&wrote);
    within(rustango::atomic!(&p, |tx| {
        put(tx, 1).await?;
        let slow = rustango::atomic!(&q, |sp| {
            put(sp, 2).await?;
            w.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok::<_, ExecError>(())
        });
        let _ = tokio::time::timeout(Duration::from_millis(300), slow).await;
        // Nothing touches `tx` again before the commit.
        Ok(())
    }))
    .await
    .unwrap();
    assert!(wrote.load(Ordering::SeqCst));
    assert_eq!(ids(pool).await, vec![1]);
}

async fn panicking_nested_block_rolls_back(pool: &Pool) {
    let p = one_conn(pool).await;
    let q = p.clone();
    within(rustango::atomic!(&p, |tx| {
        put(tx, 1).await?;
        let boom = rustango::atomic!(&q, |sp| {
            put(sp, 2).await?;
            if true {
                panic!("boom inside a nested block");
            }
            Ok::<_, ExecError>(())
        });
        assert!(CatchUnwind(Box::pin(boom)).await.is_err());
        put(tx, 3).await?;
        Ok(())
    }))
    .await
    .unwrap();
    assert_eq!(ids(pool).await, vec![1, 3]);
}

async fn guard_held_across_nested_is_an_error(pool: &Pool) {
    let p = one_conn(pool).await;
    let q = p.clone();
    within(rustango::atomic!(&p, |tx| {
        let guard = tx.lock().await?;
        let inner: Result<(), ExecError> = rustango::atomic!(&q, |_sp| { Ok(()) }).await;
        assert!(
            matches!(inner, Err(ExecError::NestedAtomic)),
            "got {inner:?}"
        );
        drop(guard);
        put(tx, 1).await?;
        Ok(())
    }))
    .await
    .unwrap();
    assert_eq!(ids(pool).await, vec![1]);
}

async fn concurrent_nested_blocks_are_refused(pool: &Pool) {
    let p = one_conn(pool).await;
    let (a, b) = (p.clone(), p.clone());
    let res: Result<(), ExecError> = within(rustango::atomic!(&p, |tx| {
        put(tx, 1).await?;
        let (ra, rb) = tokio::join!(
            rustango::atomic!(&a, |sp| {
                put(sp, 2).await?;
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok::<_, ExecError>(())
            }),
            rustango::atomic!(&b, |sp| { put(sp, 3).await }),
        );
        let refused = [&ra, &rb]
            .iter()
            .filter(|r| matches!(r, Err(ExecError::NestedAtomic)))
            .count();
        assert_eq!(
            refused, 1,
            "one of two concurrent blocks is refused: {ra:?} {rb:?}"
        );
        assert!(ra.is_ok() || rb.is_ok());
        Ok(())
    }))
    .await;
    res.unwrap();
    let got = ids(pool).await;
    assert!(
        got == vec![1, 2] || got == vec![1, 3],
        "only the accepted block commits: {got:?}"
    );
}

async fn other_pools_in_try_join_stay_independent(pool: &Pool) {
    let p = one_conn(pool).await;
    let (q, r) = (one_conn(pool).await, one_conn(pool).await);
    let fired = Arc::new(AtomicUsize::new(0));
    let (fq, fr, seen) = (Arc::clone(&fired), Arc::clone(&fired), Arc::clone(&fired));
    within(rustango::atomic!(&p, |tx| {
        put(tx, 1).await?;
        tokio::try_join!(
            rustango::atomic!(&q, |_t| {
                on_commit(move || {
                    fq.fetch_add(1, Ordering::SeqCst);
                });
                tokio::task::yield_now().await;
                Ok::<_, ExecError>(())
            }),
            rustango::atomic!(&r, |_t| {
                on_commit(move || {
                    fr.fetch_add(1, Ordering::SeqCst);
                });
                Ok::<_, ExecError>(())
            }),
        )?;
        assert_eq!(
            seen.load(Ordering::SeqCst),
            2,
            "each fired on its own commit"
        );
        Ok(())
    }))
    .await
    .unwrap();
    assert_eq!(ids(pool).await, vec![1]);
}

async fn same_pool_found_from_inside_another_pool(pool: &Pool) {
    let a = one_conn(pool).await;
    let (a2, b) = (a.clone(), one_conn(pool).await);
    within(rustango::atomic!(&a, |tx| {
        put(tx, 1).await?;
        rustango::atomic!(&b, |_t| {
            // Back on pool `a`: a savepoint, not a second connection.
            rustango::atomic!(&a2, |sp| { put(sp, 2).await }).await
        })
        .await?;
        Ok(())
    }))
    .await
    .unwrap();
    assert_eq!(ids(pool).await, vec![1, 2]);
}

/// Release the block's savepoint behind its back, so its own RELEASE fails.
async fn sabotage(sp: &AtomicTx) -> Result<(), ExecError> {
    use rustango::sql::{sqlx, sqlx::Executor as _, PoolTx};
    let sql = sqlx::raw_sql("RELEASE SAVEPOINT rustango_sp_1");
    match &mut *sp.lock().await? {
        #[cfg(feature = "postgres")]
        PoolTx::Postgres(t) => (&mut **t).execute(sql).await?,
        #[cfg(feature = "mysql")]
        PoolTx::Mysql(t) => (&mut **t).execute(sql).await.map(|_| Default::default())?,
        #[cfg(feature = "sqlite")]
        PoolTx::Sqlite(t) => (&mut **t).execute(sql).await.map(|_| Default::default())?,
    };
    Ok(())
}

async fn failed_savepoint_aborts_the_outer_commit(pool: &Pool) {
    let p = one_conn(pool).await;
    let q = p.clone();
    let res: Result<(), ExecError> = within(rustango::atomic!(&p, |tx| {
        put(tx, 1).await?;
        let inner: Result<(), ExecError> = rustango::atomic!(&q, |sp| {
            put(sp, 2).await?;
            sabotage(sp).await
        })
        .await;
        assert!(inner.is_err(), "its RELEASE fails");
        // The caller ignores the failure; the commit must not.
        Ok(())
    }))
    .await;
    assert!(matches!(res, Err(ExecError::AtomicAborted)), "got {res:?}");
    assert_eq!(ids(pool).await, Vec::<i64>::new(), "nothing commits");
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        outer_rollback_drops_inner_write,
        inner_rollback_keeps_outer_write,
        on_commit_waits_for_outermost,
        cancelled_nested_block_rolls_back,
        cancelled_nested_block_last_is_not_committed,
        panicking_nested_block_rolls_back,
        guard_held_across_nested_is_an_error,
        concurrent_nested_blocks_are_refused,
        other_pools_in_try_join_stay_independent,
        same_pool_found_from_inside_another_pool,
        failed_savepoint_aborts_the_outer_commit,
    ],
}
