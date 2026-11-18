//! `atomic_with`: isolation levels and PG transaction settings, applied
//! when the outermost block begins; `in_atomic`.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::sql::{
    atomic_with, in_atomic, raw_query_pool, AtomicOptions, ExecError, Isolation, Pool, SqlError,
    TxSetting,
};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "aopt_note")]
#[allow(dead_code)]
pub struct Note {
    #[rustango(primary_key)]
    pub id: i64,
}

fn dialect(pool: &Pool) -> &'static str {
    pool.dialect().name()
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

/// The current transaction's isolation level, as the server names it.
async fn level(pool: &Pool) -> Option<String> {
    let sql = match dialect(pool) {
        "postgres" => "SHOW transaction_isolation",
        "mysql" => {
            "SELECT CAST(ISOLATION_LEVEL AS CHAR) FROM performance_schema.events_transactions_current \
             WHERE THREAD_ID = PS_CURRENT_THREAD_ID()"
        }
        _ => return None,
    };
    let rows: Vec<(String,)> = raw_query_pool(sql, vec![], pool).await.unwrap();
    rows.into_iter().next().map(|(l,)| l.to_lowercase())
}

/// The level is the block's own; MySQL's does not reach the next transaction.
async fn isolation_level_is_read_back_in_the_block(pool: &Pool) {
    let p = one_conn(pool).await;
    let q = p.clone();
    let opts = AtomicOptions::new().isolation(Isolation::Serializable);
    let inside = atomic_with(&p, opts, |_tx| Box::pin(async move { Ok(level(&q).await) }))
        .await
        .unwrap();
    let q = p.clone();
    let next = rustango::atomic!(&p, |_tx| { Ok::<_, ExecError>(level(&q).await) })
        .await
        .unwrap();
    match dialect(pool) {
        "sqlite" => assert_eq!((inside, next), (None, None)),
        _ => {
            assert_eq!(inside.as_deref(), Some("serializable"));
            assert_ne!(next.as_deref(), Some("serializable"), "the level leaked");
        }
    }
}

/// A PG setting reaches joined reads and is gone after the block; the other
/// backends refuse settings before they begin.
async fn settings_are_transaction_local(pool: &Pool) {
    let p = one_conn(pool).await;
    let q = p.clone();
    let opts = AtomicOptions::new().setting(TxSetting::new("app.account_id", "42").unwrap());
    let read = |pool: Pool| async move {
        let rows: Vec<(Option<String>,)> = raw_query_pool(
            "SELECT current_setting('app.account_id', true)",
            vec![],
            &pool,
        )
        .await
        .unwrap();
        rows.into_iter().next().and_then(|(v,)| v)
    };
    let res = atomic_with(&p, opts, |_tx| Box::pin(async move { Ok(read(q).await) })).await;
    if dialect(pool) != "postgres" {
        assert!(
            matches!(
                res,
                Err(ExecError::Sql(
                    SqlError::OperatorNotSupportedInDialect { .. }
                ))
            ),
            "got {res:?}"
        );
        return;
    }
    assert_eq!(res.unwrap().as_deref(), Some("42"));
    let after = read(p.clone()).await;
    assert_ne!(
        after.as_deref(),
        Some("42"),
        "the setting outlived the block"
    );
}

/// Only a custom `prefix.name` is a setting.
async fn setting_names_are_checked(pool: &Pool) {
    let _ = pool;
    for bad in [
        "search_path",
        "role",
        "app",
        "app.",
        ".x",
        "app.x; DROP",
        "app.x.y",
        "transaction_isolation",
    ] {
        assert!(
            matches!(
                TxSetting::new(bad, "v"),
                Err(ExecError::InvalidTxSetting { .. })
            ),
            "`{bad}` was accepted"
        );
    }
    assert!(TxSetting::new("app.account_id", "v").is_ok());
}

/// A nested block cannot change the level or settings; `immediate` is a
/// plain savepoint.
async fn nested_options_are_refused(pool: &Pool) {
    let q = pool.clone();
    rustango::atomic!(pool, |_tx| {
        let r = q.clone();
        let opts = AtomicOptions::new().isolation(Isolation::Serializable);
        let nested = atomic_with(&r, opts, |_sp| Box::pin(async move { Ok(()) })).await;
        assert!(
            matches!(nested, Err(ExecError::InAtomic { .. })),
            "got {nested:?}"
        );
        let opts = AtomicOptions::new().immediate();
        atomic_with(&r, opts, |_sp| Box::pin(async move { Ok(()) })).await?;
        Ok(())
    })
    .await
    .unwrap();
}

async fn in_atomic_tells_the_block_apart(pool: &Pool) {
    assert!(!in_atomic(pool));
    let q = pool.clone();
    let inside = rustango::atomic!(pool, |_tx| { Ok::<_, ExecError>(in_atomic(&q)) })
        .await
        .unwrap();
    assert!(inside);
    let other = one_conn(pool).await;
    let q = pool.clone();
    let elsewhere = rustango::atomic!(&other, |_tx| { Ok::<_, ExecError>(in_atomic(&q)) })
        .await
        .unwrap();
    assert!(!elsewhere, "another pool's block");
}

tri_dialect_test! {
    model: Note,
    scenarios: [
        isolation_level_is_read_back_in_the_block,
        settings_are_transaction_local,
        setting_names_are_checked,
        nested_options_are_refused,
        in_atomic_tells_the_block_apart,
    ],
}
