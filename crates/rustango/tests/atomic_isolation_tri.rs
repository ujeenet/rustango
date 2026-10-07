//! `atomic_with` runs its transaction at the level asked for, and only
//! that transaction (#1460).

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::sql::{atomic, atomic_with, raw_query_tx, AtomicTx, ExecError, Isolation, Pool};
use rustango::tri_dialect_test;

async fn setup(_pool: &Pool) {}

/// The level the server reports for the running transaction, upper-cased
/// with spaces; SQLite has only one.
async fn level(tx: &AtomicTx) -> Result<String, ExecError> {
    let mut guard = tx.lock().await?;
    let sql = match guard.dialect().name() {
        "postgres" => "SHOW transaction_isolation",
        // `@@transaction_isolation` shows the session value, not this
        // transaction's; performance_schema has the real one.
        "mysql" => {
            "SELECT ISOLATION_LEVEL FROM performance_schema.events_transactions_current \
             WHERE THREAD_ID = PS_CURRENT_THREAD_ID()"
        }
        _ => return Ok("SERIALIZABLE".to_owned()),
    };
    let rows: Vec<(String,)> = raw_query_tx(&mut guard, sql, Vec::new()).await?;
    Ok(rows[0].0.to_uppercase().replace('-', " "))
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

async fn serializable_is_in_effect_then_gone(pool: &Pool) {
    let p = one_conn(pool).await;
    let inside = atomic_with(&p, Isolation::Serializable, |tx| Box::pin(level(tx)))
        .await
        .expect("atomic_with");
    assert_eq!(inside, "SERIALIZABLE");
    // Same connection: the level must not carry over.
    let next = atomic(&p, |tx| Box::pin(level(tx))).await.expect("atomic");
    assert_eq!(next, default_level(pool));
}

async fn read_committed_where_supported(pool: &Pool) {
    let res = atomic_with(pool, Isolation::ReadCommitted, |tx| Box::pin(level(tx))).await;
    if pool.dialect().name() == "sqlite" {
        assert!(
            matches!(res, Err(ExecError::IsolationUnsupported { .. })),
            "{res:?}"
        );
    } else {
        assert_eq!(res.expect("atomic_with"), "READ COMMITTED");
    }
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
        read_committed_where_supported,
        nested_level_is_refused,
    ],
}
