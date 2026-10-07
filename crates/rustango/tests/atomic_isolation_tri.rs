//! `atomic_with` runs its transaction at the level asked for, and only
//! that transaction (#1460).

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::sql::{
    atomic, atomic_with, raw_execute_pool, raw_query_tx, AtomicTx, ExecError, Isolation, Pool,
};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "aiso_row", app = "aiso")]
#[allow(dead_code)]
pub struct Row {
    #[rustango(primary_key)]
    pub id: i64,
    pub v: i64,
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Row>(pool).await;
    Row { id: 1, v: 0 }.insert_pool(pool).await.expect("seed");
}

/// `v` of row 1, read in the block.
async fn read_v(tx: &AtomicTx) -> Result<i64, ExecError> {
    let mut guard = tx.lock().await?;
    let sql = format!(
        "SELECT {v} FROM {t} WHERE {id} = 1",
        v = guard.dialect().quote_ident("v"),
        t = guard.dialect().quote_ident("aiso_row"),
        id = guard.dialect().quote_ident("id"),
    );
    let rows: Vec<(i64,)> = raw_query_tx(&mut guard, &sql, Vec::new()).await?;
    Ok(rows[0].0)
}

/// The level the running transaction behaves at, upper-cased with spaces.
///
/// PG reports it. MySQL's `@@transaction_isolation` shows the session value
/// and `performance_schema` needs a grant, so probe it: a concurrent UPDATE
/// waits on SERIALIZABLE's shared read lock, and a re-read sees it under
/// READ COMMITTED only. SQLite has one level.
async fn level(tx: &AtomicTx, other: &Pool) -> Result<String, ExecError> {
    let name = tx.lock().await?.dialect().name();
    match name {
        "postgres" => {
            let mut guard = tx.lock().await?;
            let rows: Vec<(String,)> =
                raw_query_tx(&mut guard, "SHOW transaction_isolation", Vec::new()).await?;
            Ok(rows[0].0.to_uppercase())
        }
        "mysql" => {
            let before = read_v(tx).await?;
            let bump = raw_execute_pool(
                other,
                "UPDATE `aiso_row` SET `v` = `v` + 1 WHERE `id` = 1",
                Vec::new(),
            )
            .await;
            if bump.is_err() {
                return Ok("SERIALIZABLE".to_owned());
            }
            Ok(if read_v(tx).await? == before {
                "REPEATABLE READ".to_owned()
            } else {
                "READ COMMITTED".to_owned()
            })
        }
        _ => Ok("SERIALIZABLE".to_owned()),
    }
}

/// The backend's default level, as `level` spells it.
fn default_level(pool: &Pool) -> &'static str {
    match pool.dialect().name() {
        "postgres" => "READ COMMITTED",
        "mysql" => "REPEATABLE READ",
        _ => "SERIALIZABLE",
    }
}

/// A one-connection pool, so the next block reuses the same connection.
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

/// The second connection `level` writes on: gives up on a lock after 1s.
async fn writer(pool: &Pool) -> Pool {
    let w = one_conn(pool).await;
    if pool.dialect().name() == "mysql" {
        raw_execute_pool(&w, "SET SESSION innodb_lock_wait_timeout = 1", Vec::new())
            .await
            .expect("lock timeout");
    }
    w
}

async fn serializable_is_in_effect_then_gone(pool: &Pool) {
    let (p, w) = (one_conn(pool).await, writer(pool).await);
    let o = w.clone();
    let inside = atomic_with(&p, Isolation::Serializable, move |tx| {
        Box::pin(async move { level(tx, &o).await })
    })
    .await
    .expect("atomic_with");
    assert_eq!(inside, "SERIALIZABLE");
    // Same connection: the level must not carry over.
    let o = w.clone();
    let next = atomic(&p, move |tx| Box::pin(async move { level(tx, &o).await }))
        .await
        .expect("atomic");
    assert_eq!(next, default_level(pool));
}

/// SQLite is always serializable, which is stricter than asked.
async fn read_committed_everywhere(pool: &Pool) {
    let w = writer(pool).await;
    let got = atomic_with(pool, Isolation::ReadCommitted, move |tx| {
        Box::pin(async move { level(tx, &w).await })
    })
    .await
    .expect("atomic_with");
    let want = if pool.dialect().name() == "sqlite" {
        "SERIALIZABLE"
    } else {
        "READ COMMITTED"
    };
    assert_eq!(got, want);
}

async fn nested_level_is_refused(pool: &Pool) {
    let p = pool.clone();
    let inner = atomic(pool, move |_tx| {
        Box::pin(async move {
            Ok(atomic_with(&p, Isolation::Serializable, |_| Box::pin(async { Ok(()) })).await)
        })
    })
    .await
    .expect("outer commits");
    assert!(
        matches!(inner, Err(ExecError::NestedIsolation)),
        "{inner:?}"
    );
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        serializable_is_in_effect_then_gone,
        read_committed_everywhere,
        nested_level_is_refused,
    ],
}
