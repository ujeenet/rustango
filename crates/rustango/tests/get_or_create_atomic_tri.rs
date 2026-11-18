//! `get_or_create` that loses a race inside `atomic` returns the winner's
//! row and leaves the block usable: on PostgreSQL the failed insert would
//! otherwise abort the whole transaction. Its reads join the block, so on
//! MySQL the re-read must lock to see past the block's snapshot.
//!
//! SQLite's block holds the write lock from `BEGIN IMMEDIATE`, so there is
//! no race to lose: the other writer waits for the block, then collides.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinHandle;

use rustango::sql::{AtomicTx, ExecError, FetcherPool as _, Pool};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "goc_atomic_note")]
pub struct Note {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 20)]
    pub label: String,
}

/// A second pool on the same database: its writes commit on their own.
async fn other_pool(pool: &Pool) -> Pool {
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

/// The other writer's insert of id 5, for SQLite where it waits for the block.
type Waiting = Arc<Mutex<Option<JoinHandle<Result<(), ExecError>>>>>;

/// The winner commits id 5 first, so `create_fn`'s insert collides. On
/// SQLite it waits for the block's lock instead, parked in `waiting`.
async fn lose_the_race(other: &Pool, waiting: &Waiting) {
    let row = Note {
        id: 5,
        label: "theirs".into(),
    };
    if sqlite(other) {
        let other = other.clone();
        let insert = tokio::spawn(async move { row.insert(&other).await });
        *waiting.lock().unwrap() = Some(insert);
        return;
    }
    row.insert(other).await.unwrap();
}

/// After the block: SQLite's waiting writer collided with the block's row.
async fn loser_collided(waiting: &Waiting) {
    let insert = waiting.lock().unwrap().take();
    if let Some(insert) = insert {
        let r = insert.await.unwrap();
        assert!(
            r.as_ref().is_err_and(ExecError::is_unique_violation),
            "got {r:?}"
        );
    }
}

fn sqlite(pool: &Pool) -> bool {
    pool.dialect().name() == "sqlite"
}

async fn put(tx: &AtomicTx, id: i64) -> Result<(), ExecError> {
    let row = Note {
        id,
        label: format!("n{id}"),
    };
    row.insert_tx(&mut *tx.lock().await?).await
}

async fn ids(pool: &Pool) -> Vec<i64> {
    let rows: Vec<Note> = Note::objects()
        .order_by(&[("id", false)])
        .fetch(pool)
        .await
        .unwrap();
    rows.into_iter().map(|r| r.id).collect()
}

async fn within<F: std::future::Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .expect("the block hung")
}

/// `create_fn` writes through the pool, which joins the block.
async fn lost_race_through_the_pool(pool: &Pool) {
    let other = other_pool(pool).await;
    let q = pool.clone();
    let waiting = Waiting::default();
    let w = Arc::clone(&waiting);
    let (row, created) = within(rustango::atomic!(pool, |_tx| {
        let got = Note::objects()
            .filter("id", 5_i64)
            .get_or_create(
                |p| async move {
                    lose_the_race(&other, &w).await;
                    let row = Note {
                        id: 5,
                        label: "mine".into(),
                    };
                    row.insert(&p).await?;
                    Ok(row)
                },
                &q,
            )
            .await?;
        Note {
            id: 6,
            label: "n6".into(),
        }
        .insert(&q)
        .await?;
        Ok(got)
    }))
    .await
    .expect("the block commits");
    loser_collided(&waiting).await;
    if sqlite(pool) {
        assert!(created);
        assert_eq!(row.label, "mine");
    } else {
        assert!(!created);
        assert_eq!(row.label, "theirs");
    }
    assert_eq!(ids(pool).await, vec![5, 6]);
}

/// `create_fn` writes through the block's own guard.
async fn lost_race_through_the_guard(pool: &Pool) {
    let other = other_pool(pool).await;
    let q = pool.clone();
    let waiting = Waiting::default();
    let w = Arc::clone(&waiting);
    let (row, created) = within(rustango::atomic!(pool, |tx| {
        let got = Note::objects()
            .filter("id", 5_i64)
            .get_or_create(
                |_p| async move {
                    lose_the_race(&other, &w).await;
                    put(tx, 7).await?;
                    put(tx, 5).await?;
                    Ok(Note {
                        id: 5,
                        label: "n5".into(),
                    })
                },
                &q,
            )
            .await?;
        put(tx, 6).await?;
        Ok(got)
    }))
    .await
    .expect("the block commits");
    loser_collided(&waiting).await;
    if sqlite(pool) {
        assert!(created);
        assert_eq!(ids(pool).await, vec![5, 6, 7]);
    } else {
        assert!(!created);
        assert_eq!(row.label, "theirs");
        // The failed `create_fn`'s own write (7) rolls back with it.
        assert_eq!(ids(pool).await, vec![5, 6]);
    }
}

/// `update_or_create` takes the same path to the winner's row.
async fn update_or_create_lost_race_through_the_guard(pool: &Pool) {
    let other = other_pool(pool).await;
    let q = pool.clone();
    let waiting = Waiting::default();
    let w = Arc::clone(&waiting);
    let (row, created) = within(rustango::atomic!(pool, |tx| {
        let got = Note::objects()
            .filter("id", 5_i64)
            .update_or_create(
                |p, mut row| async move {
                    row.label = "updated".into();
                    row.save(&p).await?;
                    Ok(row)
                },
                |_p| async move {
                    lose_the_race(&other, &w).await;
                    put(tx, 5).await?;
                    Ok(Note {
                        id: 5,
                        label: "n5".into(),
                    })
                },
                &q,
            )
            .await?;
        put(tx, 6).await?;
        Ok(got)
    }))
    .await
    .expect("the block commits");
    loser_collided(&waiting).await;
    if sqlite(pool) {
        assert!(created);
        assert_eq!(row.label, "n5");
    } else {
        assert!(!created);
        assert_eq!(row.label, "updated");
    }
    assert_eq!(ids(pool).await, vec![5, 6]);
}

/// A `create_fn` that succeeds keeps its writes.
async fn created_inside_the_block_commits(pool: &Pool) {
    let q = pool.clone();
    let (_, created) = within(rustango::atomic!(pool, |tx| {
        let got = Note::objects()
            .filter("id", 5_i64)
            .get_or_create(
                |_p| async move {
                    put(tx, 5).await?;
                    Ok(Note {
                        id: 5,
                        label: "n5".into(),
                    })
                },
                &q,
            )
            .await?;
        put(tx, 6).await?;
        Ok(got)
    }))
    .await
    .expect("the block commits");
    assert!(created);
    assert_eq!(ids(pool).await, vec![5, 6]);
}

/// Its read joins the block, so with the guard held it is refused, not run
/// on a second connection that cannot see the block's rows.
async fn refused_with_the_guard_held(pool: &Pool) {
    let q = pool.clone();
    within(rustango::atomic!(pool, |tx| {
        let g = tx.lock().await?;
        let got = Note::objects()
            .filter("id", 5_i64)
            .get_or_create(
                |_p| async {
                    Ok(Note {
                        id: 5,
                        label: "n5".into(),
                    })
                },
                &q,
            )
            .await;
        assert!(matches!(got, Err(ExecError::NestedAtomic)), "got {got:?}");
        drop(g);
        put(tx, 6).await?;
        Ok(())
    }))
    .await
    .expect("the block commits");
    assert_eq!(ids(pool).await, vec![6]);
}

/// A `get_or_create` cancelled mid-`create_fn` rolls its writes back; the
/// block's other writes still commit.
async fn cancelled_create_rolls_back(pool: &Pool) {
    let q = pool.clone();
    within(rustango::atomic!(pool, |tx| {
        let cancelled = tokio::time::timeout(
            Duration::from_secs(2),
            Note::objects().filter("id", 5_i64).get_or_create(
                |_p| async move {
                    put(tx, 7).await?;
                    std::future::pending::<()>().await;
                    Ok(Note {
                        id: 7,
                        label: "n7".into(),
                    })
                },
                &q,
            ),
        )
        .await;
        assert!(cancelled.is_err(), "create_fn should still be pending");
        put(tx, 6).await?;
        Ok(())
    }))
    .await
    .expect("the block commits");
    assert_eq!(ids(pool).await, vec![6]);
}

/// The winner of a race on a manual transaction, committed on another pool.
async fn winner(other: &Pool) {
    Note {
        id: 5,
        label: "theirs".into(),
    }
    .insert(other)
    .await
    .unwrap();
}

/// `get_or_create_tx` that loses the insert race returns the winner's row
/// and leaves the transaction usable. SQLite allows one writer, so there is
/// no race to lose there.
async fn get_or_create_tx_lost_race_returns_the_winner(pool: &Pool) {
    if sqlite(pool) {
        eprintln!("skipped on SQLite: one writer at a time, so the insert race cannot be lost");
        return;
    }
    let other = other_pool(pool).await;
    let mut tx = rustango::sql::transaction_pool(pool).await.unwrap();
    let (row, created) = rustango::sql::get_or_create_tx(
        Note::objects().filter("id", 5_i64),
        |tx| {
            Box::pin(async move {
                winner(&other).await;
                let row = Note {
                    id: 5,
                    label: "mine".into(),
                };
                row.insert_tx(tx).await?;
                Ok(row)
            })
        },
        &mut tx,
    )
    .await
    .expect("the winner's row");
    Note {
        id: 6,
        label: "n6".into(),
    }
    .insert_tx(&mut tx)
    .await
    .expect("the transaction stays usable");
    tx.commit().await.unwrap();
    assert!(!created);
    assert_eq!(row.label, "theirs");
    assert_eq!(ids(pool).await, vec![5, 6]);
}

/// `update_or_create_tx` that loses the race updates the winner's row.
async fn update_or_create_tx_lost_race_updates_the_winner(pool: &Pool) {
    if sqlite(pool) {
        eprintln!("skipped on SQLite: one writer at a time, so the insert race cannot be lost");
        return;
    }
    let other = other_pool(pool).await;
    let mut tx = rustango::sql::transaction_pool(pool).await.unwrap();
    let (row, created) = rustango::sql::update_or_create_tx(
        Note::objects().filter("id", 5_i64),
        |tx, mut row| {
            Box::pin(async move {
                row.label = "updated".into();
                row.save_tx(tx).await?;
                Ok(row)
            })
        },
        |tx| {
            Box::pin(async move {
                winner(&other).await;
                let row = Note {
                    id: 5,
                    label: "mine".into(),
                };
                row.insert_tx(tx).await?;
                Ok(row)
            })
        },
        &mut tx,
    )
    .await
    .expect("the winner's row, updated");
    tx.commit().await.unwrap();
    assert!(!created);
    assert_eq!(row.label, "updated");
    let rows: Vec<Note> = Note::objects().fetch(pool).await.unwrap();
    assert_eq!(rows[0].label, "updated");
}

/// A `get_or_create_tx` inside another's `create_fn` gets its own
/// savepoint: on MySQL a shared name would be replaced and released early.
async fn nested_get_or_create_tx_keeps_both_savepoints(pool: &Pool) {
    let mut tx = rustango::sql::transaction_pool(pool).await.unwrap();
    let (_, created) = rustango::sql::get_or_create_tx(
        Note::objects().filter("id", 5_i64),
        |tx| {
            Box::pin(async move {
                let (_, inner) = rustango::sql::get_or_create_tx(
                    Note::objects().filter("id", 7_i64),
                    |tx| {
                        Box::pin(async move {
                            let row = Note {
                                id: 7,
                                label: "parent".into(),
                            };
                            row.insert_tx(tx).await?;
                            Ok(row)
                        })
                    },
                    tx,
                )
                .await?;
                assert!(inner);
                let row = Note {
                    id: 5,
                    label: "child".into(),
                };
                row.insert_tx(tx).await?;
                Ok(row)
            })
        },
        &mut tx,
    )
    .await
    .expect("both savepoints release");
    tx.commit().await.unwrap();
    assert!(created);
    assert_eq!(ids(pool).await, vec![5, 7]);
}

/// When the whole transaction is gone by the time `create_fn` fails (a
/// deadlock, a lost connection), its own error comes back, not the
/// failed rollback's.
async fn create_error_wins_over_a_failed_rollback(pool: &Pool) {
    let mut tx = rustango::sql::transaction_pool(pool).await.unwrap();
    let res = rustango::sql::get_or_create_tx(
        Note::objects().filter("id", 5_i64),
        |tx| {
            Box::pin(async move {
                // Stands in for the server rolling the transaction back.
                rustango::sql::raw_execute_tx(tx, "ROLLBACK", vec![]).await?;
                Err::<Note, _>(ExecError::Sql(rustango::sql::SqlError::EmptyInList))
            })
        },
        &mut tx,
    )
    .await;
    assert!(
        matches!(
            res,
            Err(ExecError::Sql(rustango::sql::SqlError::EmptyInList))
        ),
        "got {res:?}"
    );
}

/// PG repeatable read: no read sees the winner's row, so the unique
/// violation is returned, as documented.
async fn pg_repeatable_read_returns_the_unique_violation(pool: &Pool) {
    if pool.dialect().name() != "postgres" {
        eprintln!("skipped: the repeatable-read rule is PG's");
        return;
    }
    let other = other_pool(pool).await;
    let mut tx = rustango::sql::transaction_pool(pool).await.unwrap();
    rustango::sql::raw_execute_tx(
        &mut tx,
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
        vec![],
    )
    .await
    .unwrap();
    let res = rustango::sql::get_or_create_tx(
        Note::objects().filter("id", 5_i64),
        |tx| {
            Box::pin(async move {
                winner(&other).await;
                let row = Note {
                    id: 5,
                    label: "mine".into(),
                };
                row.insert_tx(tx).await?;
                Ok(row)
            })
        },
        &mut tx,
    )
    .await;
    assert!(
        res.as_ref().is_err_and(ExecError::is_unique_violation),
        "got {:?}",
        res.map(|(r, c)| (r.label, c))
    );
}

tri_dialect_test! {
    model: Note,
    sqlite: file,
    scenarios: [
        lost_race_through_the_pool,
        lost_race_through_the_guard,
        update_or_create_lost_race_through_the_guard,
        created_inside_the_block_commits,
        refused_with_the_guard_held,
        cancelled_create_rolls_back,
        get_or_create_tx_lost_race_returns_the_winner,
        update_or_create_tx_lost_race_updates_the_winner,
        nested_get_or_create_tx_keeps_both_savepoints,
        create_error_wins_over_a_failed_rollback,
        pg_repeatable_read_returns_the_unique_violation,
    ],
}
