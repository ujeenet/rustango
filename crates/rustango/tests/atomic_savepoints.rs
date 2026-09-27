//! Nested `atomic_tx` runs in a savepoint on the outer connection, and
//! `on_commit` waits for the outermost commit (#1666). Pools have one
//! connection, so a second transaction would deadlock.

#[cfg(any(feature = "postgres", feature = "sqlite", feature = "mysql"))]
mod scenarios {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use rustango::sql::{on_commit, ExecError, FetcherPool as _, Pool, SqlError};
    use rustango::Model;

    #[derive(Model, Debug, Clone)]
    #[rustango(table = "asp_note")]
    #[allow(dead_code)]
    pub struct Note {
        #[rustango(primary_key)]
        pub id: i64,
        #[rustango(max_length = 20)]
        pub label: String,
    }

    fn note(id: i64) -> Note {
        Note {
            id,
            label: format!("n{id}"),
        }
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

    /// Bound every scenario so a deadlock fails instead of hanging.
    async fn within<F: std::future::Future>(f: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(10), f)
            .await
            .expect("deadlock: nested block waited for a second connection")
    }

    pub async fn check_outer_rollback_drops_inner_write(pool: &Pool) {
        let res: Result<(), ExecError> = within(rustango::atomic!(pool, |tx| {
            note(1).insert_tx(tx).await?;
            rustango::atomic_tx!(tx, |sp| { note(2).insert_tx(sp).await })
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

    pub async fn check_inner_rollback_keeps_outer_write(pool: &Pool) {
        within(rustango::atomic!(pool, |tx| {
            note(1).insert_tx(tx).await?;
            let inner: Result<(), ExecError> = rustango::atomic_tx!(tx, |sp| {
                note(2).insert_tx(sp).await?;
                Err(bail())
            })
            .await;
            assert!(inner.is_err());
            note(3).insert_tx(tx).await?;
            Ok(())
        }))
        .await
        .expect("outer commits");
        assert_eq!(ids(pool).await, vec![1, 3]);
    }

    pub async fn check_on_commit_waits_for_outermost(pool: &Pool) {
        let fired = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let (f1, f2, d1, seen) = (
            Arc::clone(&fired),
            Arc::clone(&fired),
            Arc::clone(&dropped),
            Arc::clone(&fired),
        );
        within(rustango::atomic!(pool, |tx| {
            rustango::atomic_tx!(tx, |sp| {
                on_commit(move || {
                    f1.fetch_add(1, Ordering::SeqCst);
                });
                rustango::atomic_tx!(sp, |sp2| {
                    note(1).insert_tx(sp2).await?;
                    on_commit(move || {
                        f2.fetch_add(1, Ordering::SeqCst);
                    });
                    Ok(())
                })
                .await
            })
            .await?;
            let _ = rustango::atomic_tx!(tx, |_sp| {
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
            "released callbacks fire once, at the end"
        );
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            0,
            "rolled-back savepoint drops its callback"
        );
        assert_eq!(ids(pool).await, vec![1]);
    }

    pub async fn check_nested_atomic_same_pool_rejected(pool: &Pool) {
        let pool2 = pool.clone();
        within(rustango::atomic!(pool, |tx| {
            note(1).insert_tx(tx).await?;
            let inner: Result<(), ExecError> = rustango::atomic!(&pool2, |_t| { Ok(()) }).await;
            assert!(
                matches!(inner, Err(ExecError::NestedAtomic)),
                "got {inner:?}"
            );
            Ok(())
        }))
        .await
        .unwrap();
        assert_eq!(ids(pool).await, vec![1]);
    }
}

// ------------------------------------------------------------- Postgres

#[cfg(feature = "postgres")]
mod pg_live {
    use std::sync::OnceLock;

    use rustango::sql::{sqlx, Pool};
    use tokio::sync::Mutex;

    use super::scenarios;

    fn live_lock() -> &'static Mutex<()> {
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
    }

    async fn fresh_pool() -> Option<Pool> {
        let url = std::env::var("DATABASE_URL").ok()?;
        let pg = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap_or_else(|e| panic!("DATABASE_URL is set but unreachable ({url}): {e}"));
        for sql in [
            r#"DROP TABLE IF EXISTS "asp_note" CASCADE"#,
            r#"CREATE TABLE "asp_note" ("id" BIGINT PRIMARY KEY, "label" VARCHAR(20) NOT NULL)"#,
        ] {
            sqlx::query(sql).execute(&pg).await.unwrap();
        }
        Some(Pool::Postgres(pg))
    }

    macro_rules! pg_case {
        ($name:ident) => {
            #[tokio::test]
            async fn $name() {
                let _g = live_lock().lock().await;
                let Some(pool) = fresh_pool().await else {
                    eprintln!("DATABASE_URL not set — skipping the PG arm of this scenario");
                    return;
                };
                scenarios::$name(&pool).await;
            }
        };
    }

    pg_case!(check_outer_rollback_drops_inner_write);
    pg_case!(check_inner_rollback_keeps_outer_write);
    pg_case!(check_on_commit_waits_for_outermost);
    pg_case!(check_nested_atomic_same_pool_rejected);
}

// --------------------------------------------------------------- SQLite

#[cfg(feature = "sqlite")]
mod sqlite_live {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use rustango::sql::{on_commit, sqlx, Pool};

    use super::scenarios;

    async fn fresh_pool() -> Pool {
        let sq = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite mem pool");
        sqlx::query("CREATE TABLE asp_note (id INTEGER PRIMARY KEY, label TEXT NOT NULL)")
            .execute(&sq)
            .await
            .expect("ddl");
        Pool::Sqlite(sq)
    }

    macro_rules! sqlite_case {
        ($name:ident) => {
            #[tokio::test]
            async fn $name() {
                let pool = fresh_pool().await;
                scenarios::$name(&pool).await;
            }
        };
    }

    sqlite_case!(check_outer_rollback_drops_inner_write);
    sqlite_case!(check_inner_rollback_keeps_outer_write);
    sqlite_case!(check_on_commit_waits_for_outermost);
    sqlite_case!(check_nested_atomic_same_pool_rejected);

    /// A different pool is a different database: its block is its own
    /// transaction and fires its callbacks on its own commit.
    #[tokio::test]
    async fn nested_atomic_on_another_pool_is_independent() {
        let (a, b) = (fresh_pool().await, fresh_pool().await);
        let fired = Arc::new(AtomicUsize::new(0));
        let (f, seen) = (Arc::clone(&fired), Arc::clone(&fired));
        rustango::atomic!(&a, |_tx| {
            rustango::atomic!(&b, |_t| {
                on_commit(move || {
                    f.fetch_add(1, Ordering::SeqCst);
                });
                Ok(())
            })
            .await?;
            assert_eq!(seen.load(Ordering::SeqCst), 1);
            Ok(())
        })
        .await
        .unwrap();
    }
}

// ---------------------------------------------------------------- MySQL

#[cfg(feature = "mysql")]
mod mysql_live {
    use std::sync::OnceLock;

    use rustango::sql::{sqlx, Pool};
    use tokio::sync::Mutex;

    use super::scenarios;

    fn live_lock() -> &'static Mutex<()> {
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
    }

    async fn fresh_pool() -> Option<Pool> {
        let url = std::env::var("MYSQL_TEST_URL").ok()?;
        let my = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap_or_else(|e| panic!("MYSQL_TEST_URL is set but unreachable ({url}): {e}"));
        for sql in [
            "DROP TABLE IF EXISTS asp_note",
            "CREATE TABLE asp_note (id BIGINT PRIMARY KEY, label VARCHAR(20) NOT NULL) ENGINE=InnoDB",
        ] {
            sqlx::query(sql).execute(&my).await.unwrap();
        }
        Some(Pool::Mysql(my))
    }

    macro_rules! mysql_case {
        ($name:ident) => {
            #[tokio::test]
            async fn $name() {
                let _g = live_lock().lock().await;
                let Some(pool) = fresh_pool().await else {
                    eprintln!("MYSQL_TEST_URL unset — skipping the MySQL arm of this scenario");
                    return;
                };
                scenarios::$name(&pool).await;
            }
        };
    }

    mysql_case!(check_outer_rollback_drops_inner_write);
    mysql_case!(check_inner_rollback_keeps_outer_write);
    mysql_case!(check_on_commit_waits_for_outermost);
    mysql_case!(check_nested_atomic_same_pool_rejected);
}
