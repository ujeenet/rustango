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

/// Poll `fut` once, then drop it: a cancel at its first await.
async fn poll_once_then_drop<F: Future>(fut: F) {
    let mut fut = Box::pin(fut);
    std::future::poll_fn(|cx| {
        assert!(
            fut.as_mut().poll(cx).is_pending(),
            "finished before the cancel"
        );
        Poll::Ready(())
    })
    .await;
}

/// Poll `fut` until `flag` is set by its body, then drop it.
async fn drop_once_flagged<F: Future>(fut: F, flag: Arc<AtomicBool>) {
    let mut fut = Box::pin(fut);
    std::future::poll_fn(|cx| {
        assert!(
            fut.as_mut().poll(cx).is_pending(),
            "finished before the cancel"
        );
        if flag.load(Ordering::SeqCst) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}

fn dialect(pool: &Pool) -> &'static str {
    pool.dialect().name()
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
            async {
                // Start while `a` runs its body without holding the lock.
                tokio::time::sleep(Duration::from_millis(10)).await;
                rustango::atomic!(&b, |sp| { put(sp, 3).await }).await
            },
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
        PoolTx::Postgres(t) => (&mut **t).execute(sql).await.map(|_| ())?,
        #[cfg(feature = "mysql")]
        PoolTx::Mysql(t) => (&mut **t).execute(sql).await.map(|_| ())?,
        #[cfg(feature = "sqlite")]
        PoolTx::Sqlite(t) => (&mut **t).execute(sql).await.map(|_| ())?,
    }
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

async fn swallowed_statement_error_is_not_committed_on_pg(pool: &Pool) {
    let p = one_conn(pool).await;
    let fired = Arc::new(AtomicUsize::new(0));
    let f = Arc::clone(&fired);
    let res: Result<(), ExecError> = within(rustango::atomic!(&p, |tx| {
        put(tx, 1).await?;
        // Duplicate key; the closure ignores it.
        assert!(put(tx, 1).await.is_err());
        on_commit(move || {
            f.fetch_add(1, Ordering::SeqCst);
        });
        Ok(())
    }))
    .await;
    if dialect(pool) == "postgres" {
        // PG aborted the transaction; its COMMIT would silently roll back.
        assert!(matches!(res, Err(ExecError::AtomicAborted)), "got {res:?}");
        assert_eq!(
            fired.load(Ordering::SeqCst),
            0,
            "no hook for nothing written"
        );
        assert_eq!(ids(pool).await, Vec::<i64>::new());
    } else {
        // MySQL and SQLite undo only the failed statement.
        res.unwrap();
        assert_eq!(fired.load(Ordering::SeqCst), 1);
        assert_eq!(ids(pool).await, vec![1]);
    }
}

async fn failed_savepoint_open_poisons_on_pg(pool: &Pool) {
    if dialect(pool) != "postgres" {
        return; // Only PG refuses a SAVEPOINT (in an aborted transaction).
    }
    let p = one_conn(pool).await;
    let q = p.clone();
    let res: Result<(), ExecError> = within(rustango::atomic!(&p, |tx| {
        put(tx, 1).await?;
        assert!(put(tx, 1).await.is_err());
        let inner: Result<(), ExecError> = rustango::atomic!(&q, |_sp| { Ok(()) }).await;
        assert!(
            matches!(inner, Err(ExecError::Driver(_))),
            "SAVEPOINT fails: {inner:?}"
        );
        // Poisoned: our own error now, not PG's.
        assert!(matches!(
            tx.lock().await.map(drop),
            Err(ExecError::AtomicAborted)
        ));
        Ok(())
    }))
    .await;
    assert!(matches!(res, Err(ExecError::AtomicAborted)), "got {res:?}");
    assert_eq!(ids(pool).await, Vec::<i64>::new());
}

async fn cancel_while_savepoint_opens(pool: &Pool) {
    let p = one_conn(pool).await;
    let q = p.clone();
    within(rustango::atomic!(&p, |tx| {
        poll_once_then_drop(rustango::atomic!(&q, |sp| { put(sp, 2).await })).await;
        put(tx, 1).await?;
        put(tx, 3).await?;
        Ok(())
    }))
    .await
    .expect("the half-opened savepoint is rolled back, not left open");
    assert_eq!(ids(pool).await, vec![1, 3]);
}

async fn cancel_while_savepoint_releases_keeps_hooks(pool: &Pool) {
    let p = one_conn(pool).await;
    let q = p.clone();
    let fired = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let (f, d) = (Arc::clone(&fired), Arc::clone(&done));
    within(rustango::atomic!(&p, |tx| {
        let inner = rustango::atomic!(&q, |_sp| {
            on_commit(move || {
                f.fetch_add(1, Ordering::SeqCst);
            });
            d.store(true, Ordering::SeqCst);
            Ok::<_, ExecError>(())
        });
        // The body finished; drop the block while its RELEASE is pending.
        drop_once_flagged(inner, done).await;
        put(tx, 1).await?;
        Ok(())
    }))
    .await
    .unwrap();
    assert_eq!(
        fired.load(Ordering::SeqCst),
        1,
        "a released block keeps its hook"
    );
    assert_eq!(ids(pool).await, vec![1]);
}

async fn nesting_survives_set_connect_options(pool: &Pool) {
    let p = one_conn(pool).await;
    let q = p.clone();
    within(rustango::atomic!(&p, |tx| {
        put(tx, 1).await?;
        // Credential rotation swaps the options Arc on every clone.
        match &q {
            #[cfg(feature = "postgres")]
            Pool::Postgres(s) => s.set_connect_options((*s.connect_options()).clone()),
            #[cfg(feature = "mysql")]
            Pool::Mysql(s) => s.set_connect_options((*s.connect_options()).clone()),
            #[cfg(feature = "sqlite")]
            Pool::Sqlite(s) => s.set_connect_options((*s.connect_options()).clone()),
        }
        rustango::atomic!(&q, |sp| { put(sp, 2).await }).await
    }))
    .await
    .expect("still a savepoint, not a second connection");
    assert_eq!(ids(pool).await, vec![1, 2]);
}

async fn joined_blocks_on_other_pools_keep_their_own_hooks(pool: &Pool) {
    let p = one_conn(pool).await;
    let (q, r) = (one_conn(pool).await, one_conn(pool).await);
    let committed = Arc::new(AtomicUsize::new(0));
    let rolled_back = Arc::new(AtomicUsize::new(0));
    let (c, rb) = (Arc::clone(&committed), Arc::clone(&rolled_back));
    let (seen_c, seen_rb) = (Arc::clone(&committed), Arc::clone(&rolled_back));
    let go = Arc::new(tokio::sync::Notify::new());
    let go2 = Arc::clone(&go);
    within(rustango::atomic!(&p, |tx| {
        put(tx, 1).await?;
        let (rr, rq) = tokio::join!(
            async {
                // `r` registers its hook first, then waits for `q`.
                rustango::atomic!(&r, |_t| {
                    on_commit(move || {
                        rb.fetch_add(1, Ordering::SeqCst);
                    });
                    go2.notified().await;
                    Err::<(), _>(bail())
                })
                .await
            },
            rustango::atomic!(&q, |_t| {
                on_commit(move || {
                    c.fetch_add(1, Ordering::SeqCst);
                });
                go.notify_one();
                Ok::<_, ExecError>(())
            }),
        );
        assert!(rr.is_err() && rq.is_ok());
        assert_eq!(
            seen_c.load(Ordering::SeqCst),
            1,
            "q fired on its own commit"
        );
        assert_eq!(
            seen_rb.load(Ordering::SeqCst),
            0,
            "r rolled back: its hook drops"
        );
        Ok(())
    }))
    .await
    .unwrap();
    assert_eq!(committed.load(Ordering::SeqCst), 1);
    assert_eq!(rolled_back.load(Ordering::SeqCst), 0);
}

async fn on_commit_in_the_closures_sync_part_belongs_to_its_block(pool: &Pool) {
    let p = one_conn(pool).await;
    let q = p.clone();
    let fired = Arc::new(AtomicUsize::new(0));
    let f = Arc::clone(&fired);
    within(rustango::atomic!(&p, |tx| {
        put(tx, 1).await?;
        let inner = rustango::sql::atomic(&q, move |_sp| {
            // Runs before the returned future is polled.
            on_commit(move || {
                f.fetch_add(1, Ordering::SeqCst);
            });
            Box::pin(async move { Err::<(), _>(bail()) })
        })
        .await;
        assert!(inner.is_err());
        Ok(())
    }))
    .await
    .unwrap();
    assert_eq!(
        fired.load(Ordering::SeqCst),
        0,
        "the rolled-back block drops it"
    );
}

async fn sqlite_rollback_refuses_later_statements(pool: &Pool) {
    if dialect(pool) != "sqlite" {
        return; // The hook is SQLite's; PG and MySQL have their own checks.
    }
    let p = one_conn(pool).await;
    let res: Result<(), ExecError> = within(rustango::atomic!(&p, |tx| {
        put(tx, 1).await?;
        let mut guard = tx.lock().await?;
        #[cfg(feature = "sqlite")]
        #[allow(irrefutable_let_patterns)]
        if let rustango::sql::PoolTx::Sqlite(t) = &mut *guard {
            use rustango::sql::{sqlx, sqlx::Executor as _};
            // Stands in for SQLite rolling back on its own (SQLITE_FULL, IOERR).
            (&mut **t).execute(sqlx::raw_sql("ROLLBACK")).await?;
        }
        let row = Note {
            id: 2,
            label: "n2".into(),
        };
        let next = row.insert_tx(&mut guard).await;
        assert!(
            matches!(next, Err(ExecError::AtomicAborted)),
            "got {next:?}"
        );
        Ok(())
    }))
    .await;
    assert!(matches!(res, Err(ExecError::AtomicAborted)), "got {res:?}");
    assert_eq!(ids(pool).await, Vec::<i64>::new(), "nothing autocommitted");
}

async fn mysql_ddl_implicit_commit_is_reported(pool: &Pool) {
    if dialect(pool) != "mysql" {
        return; // Only MySQL commits implicitly on DDL.
    }
    use rustango::sql::raw_execute_tx;
    let p = one_conn(pool).await;
    let res: Result<(), ExecError> = within(rustango::atomic!(&p, |tx| {
        put(tx, 1).await?;
        raw_execute_tx(
            &mut *tx.lock().await?,
            "CREATE TABLE asp_ddl_probe (id INT)",
            vec![],
        )
        .await?;
        Ok(())
    }))
    .await;
    rustango::testkit::matrix::drop_table(pool, "asp_ddl_probe").await;
    assert!(
        matches!(res, Err(ExecError::AtomicEndedEarly)),
        "got {res:?}"
    );
    assert_eq!(
        ids(pool).await,
        vec![1],
        "the DDL committed the earlier row"
    );
}

async fn join_lock_during_background_settle_is_refused(pool: &Pool) {
    let p = one_conn(pool).await;
    let q = p.clone();
    within(rustango::atomic!(&p, |tx| {
        // Leaves a cancel mark, so the next lock settles in the background.
        poll_once_then_drop(rustango::atomic!(&q, |sp| { put(sp, 2).await })).await;
        let go = tokio::sync::Notify::new();
        // Whichever branch gets the lock holds it until the other is done.
        let branch = || async {
            match tx.lock().await {
                Ok(g) => {
                    go.notified().await;
                    drop(g);
                    Ok(())
                }
                Err(e) => {
                    go.notify_one();
                    Err(e)
                }
            }
        };
        let (a, b) = tokio::join!(branch(), branch());
        let refused = [&a, &b]
            .iter()
            .filter(|r| matches!(r, Err(ExecError::NestedAtomic)))
            .count();
        assert_eq!(refused, 1, "one branch is refused, none hangs: {a:?} {b:?}");
        put(tx, 1).await?;
        Ok(())
    }))
    .await
    .unwrap();
    assert_eq!(ids(pool).await, vec![1]);
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
        swallowed_statement_error_is_not_committed_on_pg,
        failed_savepoint_open_poisons_on_pg,
        cancel_while_savepoint_opens,
        cancel_while_savepoint_releases_keeps_hooks,
        nesting_survives_set_connect_options,
        joined_blocks_on_other_pools_keep_their_own_hooks,
        on_commit_in_the_closures_sync_part_belongs_to_its_block,
        sqlite_rollback_refuses_later_statements,
        mysql_ddl_implicit_commit_is_reported,
        join_lock_during_background_settle_is_refused,
    ],
}

/// A MySQL deadlock ends the whole transaction and later statements would
/// autocommit. The next `lock()` and the commit must both refuse.
#[cfg(feature = "mysql")]
#[tokio::test]
async fn mysql_deadlock_aborts_the_block() {
    use rustango::sql::{sqlx, update_tx};
    use rustango::testkit::matrix::{live_lock, Backend};

    let _guard = live_lock().lock().await;
    let Some(pool) = Backend::MySql.pool().await else {
        eprintln!("MYSQL_TEST_URL not set — skipping the MySQL deadlock test");
        return;
    };
    setup(&pool).await;
    for id in 1..=4 {
        put_pool(&pool, id).await;
    }
    let p = one_conn(&pool).await;
    let Pool::Mysql(other) = one_conn(&pool).await else {
        unreachable!()
    };
    let (a_locked, b_locked) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    // `b` changes three rows, so MySQL picks the lighter `a` as the victim.
    let b = {
        let (a_locked, b_locked) = (Arc::clone(&a_locked), Arc::clone(&b_locked));
        tokio::spawn(async move {
            let mut t = other.begin().await.unwrap();
            a_locked.notified().await;
            for id in [2, 3, 4] {
                sqlx::query("UPDATE asp_note SET label = 'b' WHERE id = ?")
                    .bind(id)
                    .execute(&mut *t)
                    .await
                    .unwrap();
            }
            b_locked.notify_one();
            tokio::time::sleep(Duration::from_millis(200)).await;
            sqlx::query("UPDATE asp_note SET label = 'b' WHERE id = 1")
                .execute(&mut *t)
                .await
                .unwrap();
            t.commit().await.unwrap();
        })
    };
    let relabel = |id: i64| {
        Note::objects()
            .filter("id", id)
            .update()
            .set("label", "a")
            .compile()
            .unwrap()
    };
    let res: Result<(), ExecError> = within(rustango::atomic!(&p, |tx| {
        update_tx(&mut *tx.lock().await?, &relabel(1)).await?;
        a_locked.notify_one();
        b_locked.notified().await;
        let mut guard = tx.lock().await?;
        let dead = update_tx(&mut guard, &relabel(2)).await;
        let Err(ExecError::Driver(e)) = &dead else {
            panic!("expected the deadlock victim, got {dead:?}");
        };
        assert!(e.to_string().contains("1213"), "deadlock: {e}");
        // The closure ignores it; a second statement on the SAME guard
        // must not autocommit.
        let row = Note {
            id: 9,
            label: "n9".into(),
        };
        let same_guard = row.insert_tx(&mut guard).await;
        assert!(
            matches!(same_guard, Err(ExecError::AtomicAborted)),
            "got {same_guard:?}"
        );
        drop(guard);
        let after = put(tx, 10).await;
        assert!(
            matches!(after, Err(ExecError::AtomicAborted)),
            "got {after:?}"
        );
        Ok(())
    }))
    .await;
    b.await.unwrap();
    assert!(matches!(res, Err(ExecError::AtomicAborted)), "got {res:?}");
    assert!(
        !ids(&pool).await.contains(&9),
        "nothing ran after the deadlock"
    );
}

#[cfg(feature = "mysql")]
async fn put_pool(pool: &Pool, id: i64) {
    Note {
        id,
        label: format!("n{id}"),
    }
    .insert_pool(pool)
    .await
    .unwrap();
}

/// #1460 premise: a `&Pool` read inside a block takes a second connection,
/// so on a one-connection pool it waits forever. S3 routes it into the block.
#[tokio::test]
#[ignore = "#1460 S3"]
async fn pool_read_inside_atomic_joins_the_block() {
    use rustango::sql::CounterPool as _;
    use rustango::testkit::matrix::{live_lock, Backend};
    for backend in [Backend::Postgres, Backend::MySql, Backend::Sqlite] {
        let _guard = live_lock().lock().await;
        let Some(pool) = backend.pool().await else {
            continue;
        };
        setup(&pool).await;
        let p = one_conn(&pool).await;
        let q = p.clone();
        let seen = within(rustango::atomic!(&p, |tx| {
            put(tx, 1).await?;
            Note::objects().count(&q).await
        }))
        .await
        .expect("atomic");
        assert_eq!(
            seen,
            1,
            "{}: the read saw the block's write",
            dialect(&pool)
        );
    }
}
