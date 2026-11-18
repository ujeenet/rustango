//! Backing test for `docs/transactions.md` — runs the page's examples on a
//! file SQLite database: commit and rollback, savepoints, `on_commit`, the
//! nested-block workaround, `atomic_with` refusals, `in_atomic`, the `_tx`
//! race helpers, the `_on` forms, DDL rollback, keyset iteration,
//! `BEGIN IMMEDIATE` and cancellation. The per-backend differences are in the
//! `atomic_*_tri` suites.
//!
//! Run: `cargo test -p rustango --features sqlite --test transactions_doc`

#![cfg(feature = "sqlite")]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustango::core::{BulkUpdateQuery, Model as _, SqlValue};
use rustango::sql::model_shortcuts::increment_one_on;
use rustango::sql::{
    atomic, atomic_with, bulk_update_on, get_or_create_tx, in_atomic, on_commit,
    run_ddl_idempotent, transaction_pool, update_or_create_tx, AtomicOptions, Auto, ExecError,
    FetcherPool as _, Isolation, Pool, SqlError, TxSetting,
};
use rustango::Model;

type BoxError = Box<dyn std::error::Error>;

#[derive(Model, Debug, Clone)]
#[rustango(table = "accounts")]
pub struct Account {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(unique, max_length = 64)]
    pub email: String,
    pub balance: i64,
}

fn account(email: &str, balance: i64) -> Account {
    Account {
        id: Auto::Unset,
        email: email.into(),
        balance,
    }
}

/// A file database: a second connection must see the block's write lock.
async fn pool() -> (tempfile::TempDir, Pool) {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("tx.db").display());
    let pool = Pool::connect(&url).await.unwrap();
    rustango::testkit::create_tables_for::<Account>(&pool)
        .await
        .unwrap();
    (dir, pool)
}

async fn emails(pool: &Pool) -> Vec<(String, i64)> {
    let mut v: Vec<_> = Account::objects()
        .fetch(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|a| (a.email, a.balance))
        .collect();
    v.sort();
    v
}

// The page's "A first block".
async fn transfer(pool: &Pool, from: i64, to: i64, amount: i64) -> Result<(), ExecError> {
    let p = pool.clone(); // the closure must own what it uses
    atomic(pool, move |_tx| {
        Box::pin(async move {
            let mut a = Account::find_or_fail(from, &p).await?;
            let mut b = Account::find_or_fail(to, &p).await?;
            a.balance -= amount;
            b.balance += amount;
            a.save(&p).await?;
            b.save(&p).await?;
            Ok(()) // Ok commits; an Err (or a failed `?`) rolls both saves back
        })
    })
    .await
}

#[tokio::test]
async fn a_block_commits_on_ok_and_rolls_back_on_err() {
    let (_d, pool) = pool().await;
    let mut a = account("a@example.com", 100);
    a.insert(&pool).await.unwrap();
    let mut b = account("b@example.com", 0);
    b.insert(&pool).await.unwrap();
    let (a_id, b_id) = (*a.id.get().unwrap(), *b.id.get().unwrap());
    transfer(&pool, a_id, b_id, 30).await.unwrap();
    assert_eq!(
        emails(&pool).await,
        vec![("a@example.com".into(), 70), ("b@example.com".into(), 30)]
    );
    // The second account is missing: the first save rolls back.
    assert!(transfer(&pool, a_id, 999, 30).await.is_err());
    assert_eq!(emails(&pool).await[0], ("a@example.com".into(), 70));
}

#[tokio::test]
async fn a_nested_block_is_a_savepoint() -> Result<(), BoxError> {
    let (_d, pool) = pool().await;
    let pool = &pool;
    // The page's "Nested blocks".
    let p = pool.clone();
    atomic(pool, move |_tx| {
        Box::pin(async move {
            account("a@example.com", 0).insert(&p).await?;
            let q = p.clone();
            let inner = atomic(&p, move |_sp| {
                Box::pin(async move {
                    account("b@example.com", 0).insert(&q).await?;
                    // Stands in for any failure inside the nested block.
                    Err::<(), _>(ExecError::Driver(rustango::sql::sqlx::Error::RowNotFound))
                })
            })
            .await;
            assert!(inner.is_err()); // b@ is rolled back, a@ is kept
            Ok(())
        })
    })
    .await?;
    assert_eq!(emails(pool).await, vec![("a@example.com".into(), 0)]);
    Ok(())
}

#[tokio::test]
async fn on_commit_runs_after_the_outermost_commit_only() -> Result<(), BoxError> {
    let (_d, pool) = pool().await;
    let pool = &pool;
    let sent = Arc::new(Mutex::new(Vec::new()));
    // The page's "After the commit", with a log in place of the email.
    let (p, log) = (pool.clone(), sent.clone());
    atomic(pool, move |_tx| {
        Box::pin(async move {
            let mut acc = account("new@example.com", 0);
            acc.insert(&p).await?;
            on_commit(move || {
                log.lock().unwrap().push("welcome");
                tokio::spawn(async move { /* send the welcome email */ });
            });
            Ok(())
        })
    })
    .await?;
    assert_eq!(*sent.lock().unwrap(), vec!["welcome"]);

    let log = sent.clone();
    let _ = atomic(pool, move |_tx| {
        Box::pin(async move {
            on_commit(move || log.lock().unwrap().push("rolled back"));
            Err::<(), _>(ExecError::Driver(rustango::sql::sqlx::Error::RowNotFound))
        })
    })
    .await;
    assert_eq!(*sent.lock().unwrap(), vec!["welcome"]);
    Ok(())
}

#[tokio::test]
async fn a_failed_step_in_a_nested_block_keeps_the_outer_one() -> Result<(), BoxError> {
    let (_d, pool) = pool().await;
    let pool = &pool;
    account("a@example.com", 0).insert(pool).await?;
    // The page's workaround.
    let p = pool.clone();
    atomic(pool, move |_tx| {
        Box::pin(async move {
            let q = p.clone();
            // A failed read in a nested block undoes only that block, even on PG.
            let locked = atomic(&p, move |_sp| {
                Box::pin(async move {
                    Account::objects()
                        .filter("email", "a@example.com")
                        .nowait()
                        .fetch(&q)
                        .await
                })
            })
            .await;
            match locked {
                Ok(_rows) => { /* got the lock */ }
                Err(_) => { /* row busy: the outer block goes on */ }
            }
            account("b@example.com", 0).insert(&p).await?;
            Ok(())
        })
    })
    .await?;
    assert_eq!(emails(pool).await.len(), 2);
    // A missing table fails the nested block; the outer one still commits.
    let p = pool.clone();
    atomic(pool, move |_tx| {
        Box::pin(async move {
            let q = p.clone();
            let bad = atomic(&p, move |_sp| {
                Box::pin(async move {
                    rustango::sql::raw_query_pool::<(i64,)>("SELECT x FROM missing", vec![], &q)
                        .await
                })
            })
            .await;
            assert!(bad.is_err());
            account("c@example.com", 0).insert(&p).await
        })
    })
    .await?;
    assert_eq!(emails(pool).await.len(), 3);
    Ok(())
}

#[tokio::test]
async fn atomic_with_on_sqlite() -> Result<(), BoxError> {
    let (_d, pool) = pool().await;
    let pool = &pool;
    // Any level is accepted: SQLite is always serializable.
    let opts = AtomicOptions::new().isolation(Isolation::RepeatableRead);
    atomic_with(pool, opts, |_tx| Box::pin(async move { Ok(()) })).await?;

    // Settings are PostgreSQL-only: refused before the block begins.
    let opts = AtomicOptions::new().setting(TxSetting::new("app.account_id", "42")?);
    let err =
        atomic_with(
            pool,
            opts,
            |_tx| -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<(), ExecError>> + Send>,
            > { panic!("the block must not begin") },
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        ExecError::Sql(SqlError::OperatorNotSupportedInDialect { .. })
    ));
    assert!(err.to_string().contains("set_config"), "{err}");

    for bad in ["search_path", "role", "app.x;drop", "app..x", "app.x y"] {
        assert!(
            matches!(
                TxSetting::new(bad, "1"),
                Err(ExecError::InvalidTxSetting { .. })
            ),
            "{bad}"
        );
    }

    // Options on a nested block are refused.
    let p = pool.clone();
    let nested = atomic(pool, move |_tx| {
        Box::pin(async move {
            let opts = AtomicOptions::new().isolation(Isolation::Serializable);
            Ok(atomic_with(&p, opts, |_sp| Box::pin(async move { Ok(()) })).await)
        })
    })
    .await?;
    assert!(matches!(nested, Err(ExecError::InAtomic { .. })));
    Ok(())
}

#[tokio::test]
async fn in_atomic_names_the_pool() -> Result<(), BoxError> {
    let (dir, pool) = pool().await;
    let other = Pool::connect(&format!("sqlite://{}", dir.path().join("tx.db").display())).await?;
    assert!(!in_atomic(&pool));
    let p = pool.clone();
    let (mine, theirs) = atomic(&pool, move |_tx| {
        Box::pin(async move { Ok((in_atomic(&p), in_atomic(&other))) })
    })
    .await?;
    assert!(mine && !theirs);
    Ok(())
}

#[tokio::test]
async fn get_or_create_tx_and_update_or_create_tx() -> Result<(), BoxError> {
    let (_d, pool) = pool().await;
    let pool = &pool;
    for expect_created in [true, false] {
        // The page's get_or_create_tx example.
        let email = "a@example.com".to_owned();
        let (acc, created) = atomic(pool, move |tx| {
            Box::pin(async move {
                let mut g = tx.lock().await?;
                let filter = Account::objects().filter("email", email.clone());
                get_or_create_tx(
                    filter,
                    move |t| {
                        Box::pin(async move {
                            let mut acc = Account {
                                id: Auto::Unset,
                                email,
                                balance: 0,
                            };
                            acc.insert_tx(t).await?;
                            Ok(acc)
                        })
                    },
                    &mut *g,
                )
                .await
            })
        })
        .await?;
        assert_eq!(
            (acc.email.as_str(), created),
            ("a@example.com", expect_created)
        );
    }
    for (expect_created, balance) in [(true, 10), (false, 20)] {
        // The page's update_or_create_tx example.
        let (acc, created) = atomic(pool, move |tx| {
            Box::pin(async move {
                let mut g = tx.lock().await?;
                update_or_create_tx(
                    Account::objects().filter("email", "b@example.com"),
                    |t, mut acc: Account| {
                        Box::pin(async move {
                            acc.balance += 10;
                            acc.save_tx(t).await?;
                            Ok(acc)
                        })
                    },
                    |t| {
                        Box::pin(async move {
                            let mut acc = Account {
                                id: Auto::Unset,
                                email: "b@example.com".into(),
                                balance: 10,
                            };
                            acc.insert_tx(t).await?;
                            Ok(acc)
                        })
                    },
                    &mut *g,
                )
                .await
            })
        })
        .await?;
        assert_eq!((created, acc.balance), (expect_created, balance));
    }
    Ok(())
}

/// The page's `_on` example, on any open transaction.
async fn on_forms(tx: &mut rustango::sql::PoolTx<'_>) -> Result<(), BoxError> {
    let mut rows = vec![
        Account {
            id: Auto::Unset,
            email: "a@example.com".into(),
            balance: 0,
        },
        Account {
            id: Auto::Unset,
            email: "b@example.com".into(),
            balance: 0,
        },
    ];
    Account::bulk_insert_on(&mut rows, &mut *tx).await?;
    // MySQL leaves `Auto` ids unset after a bulk insert: read them back.
    let ids: Vec<i64> = Account::objects()
        .order_by(&[("id", false)])
        .fetch_on(&mut *tx)
        .await?
        .iter()
        .filter_map(|a| a.id.get().copied())
        .collect();
    let set_100 = BulkUpdateQuery::new(
        Account::SCHEMA,
        vec!["balance"],
        ids.iter()
            .map(|id| vec![SqlValue::from(*id), SqlValue::from(100_i64)])
            .collect(),
    );
    bulk_update_on(&mut *tx, &set_100).await?;
    increment_one_on::<Account, _>("id", SqlValue::from(ids[0]), "balance", 5, &mut *tx).await?;
    // Insert c@, skip a@ (already there).
    let more = [
        Account {
            id: Auto::Unset,
            email: "a@example.com".into(),
            balance: 0,
        },
        Account {
            id: Auto::Unset,
            email: "c@example.com".into(),
            balance: 7,
        },
    ];
    Account::bulk_insert_or_ignore_on(&more, &mut *tx).await?;
    Ok(())
}

#[tokio::test]
async fn on_forms_follow_the_transaction() -> Result<(), BoxError> {
    let (_d, pool) = pool().await;
    let pool = &pool;
    let mut tx = transaction_pool(pool).await?;
    on_forms(&mut tx).await?;
    tx.rollback().await?;
    assert!(emails(pool).await.is_empty());

    let mut tx = transaction_pool(pool).await?;
    on_forms(&mut tx).await?;
    tx.commit().await?;
    assert_eq!(
        emails(pool).await,
        vec![
            ("a@example.com".into(), 105),
            ("b@example.com".into(), 100),
            ("c@example.com".into(), 7)
        ]
    );
    Ok(())
}

#[tokio::test]
async fn ddl_in_a_sqlite_block_rolls_back_with_it() {
    let (_d, pool) = pool().await;
    let p = pool.clone();
    let _ = atomic(&pool, move |_tx| {
        Box::pin(async move {
            run_ddl_idempotent(&p, "CREATE TABLE notes (id INTEGER)").await?;
            Err::<(), _>(ExecError::Driver(rustango::sql::sqlx::Error::RowNotFound))
        })
    })
    .await;
    let probe =
        rustango::sql::raw_query_pool::<(i64,)>("SELECT COUNT(*) FROM notes", vec![], &pool).await;
    assert!(probe.is_err(), "the table outlived the rollback");
}

#[tokio::test]
async fn iterator_in_a_block_pages_by_primary_key() -> Result<(), BoxError> {
    let (_d, pool) = pool().await;
    let pool = &pool;
    for i in 0..7 {
        account(&format!("u{i}@example.com"), 0)
            .insert(pool)
            .await?;
    }
    // The page's iterator example, with a counter.
    let p = pool.clone();
    let seen = atomic(pool, move |_tx| {
        Box::pin(async move {
            let mut seen = 0;
            let mut it = Account::objects()
                .filter("balance", 0_i64)
                .order_by(&[("id", false)]) // primary key only, or no order
                .iterator(2)?;
            while let Some(chunk) = it.next_chunk(&p).await? {
                for mut acc in chunk {
                    seen += 1;
                    acc.balance = 1; // leaves the filter; paging by key still reaches every row
                    acc.save(&p).await?;
                }
            }
            Ok(seen)
        })
    })
    .await?;
    assert_eq!(seen, 7);

    let p = pool.clone();
    let refused = atomic(pool, move |_tx| {
        Box::pin(async move {
            let mut it = Account::objects()
                .order_by(&[("email", false)])
                .iterator(2)?;
            Ok(it.next_chunk(&p).await.map(|c| c.map(|v| v.len())))
        })
    })
    .await?;
    assert!(
        matches!(refused, Err(ExecError::InAtomic { .. })),
        "{refused:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_sqlite_block_takes_the_write_lock_at_begin() {
    let (_d, pool) = pool().await;
    let t0 = Instant::now();
    let p = pool.clone();
    let first = tokio::spawn(async move {
        let q = p.clone();
        atomic(&p, move |_tx| {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(400)).await;
                account("first@example.com", 0).insert(&q).await
            })
        })
        .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let p = pool.clone();
    let body_started = atomic(&pool, move |_tx| {
        Box::pin(async move {
            let started = t0.elapsed();
            account("second@example.com", 0).insert(&p).await?;
            Ok(started)
        })
    })
    .await
    .unwrap();
    first.await.unwrap().unwrap();
    assert!(
        body_started >= Duration::from_millis(350),
        "the second block began at {body_started:?}, before the first ended"
    );
}

/// Every language shows the same examples as the English page.
#[test]
fn every_language_prints_the_same_examples() {
    let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs");
    let fences = |page: &str| -> Vec<String> {
        let text = std::fs::read_to_string(docs.join(page)).unwrap();
        text.split("```rust\n")
            .skip(1)
            .map(|rest| rest.split("```").next().unwrap().to_owned())
            .collect()
    };
    let en = fences("transactions.md");
    assert!(!en.is_empty());
    for locale in ["de", "fr", "es"] {
        assert_eq!(fences(&format!("{locale}/transactions.md")), en, "{locale}");
    }
}

#[tokio::test]
async fn a_dropped_block_rolls_back_and_the_pool_goes_on() -> Result<(), BoxError> {
    let (_d, pool) = pool().await;
    let pool = &pool;
    // The page's cancellation example.
    let p = pool.clone();
    let slow = atomic(pool, move |_tx| {
        Box::pin(async move {
            Account {
                id: Auto::Unset,
                email: "late@example.com".into(),
                balance: 0,
            }
            .insert(&p)
            .await?;
            tokio::time::sleep(Duration::from_secs(5)).await; // stands in for slow work
            Ok(())
        })
    });
    // Dropping the block (here on timeout) rolls it back.
    assert!(tokio::time::timeout(Duration::from_millis(100), slow)
        .await
        .is_err());

    let p = pool.clone();
    atomic(pool, move |_tx| {
        Box::pin(async move { account("next@example.com", 0).insert(&p).await })
    })
    .await?;
    assert_eq!(emails(pool).await, vec![("next@example.com".into(), 0)]);
    Ok(())
}
