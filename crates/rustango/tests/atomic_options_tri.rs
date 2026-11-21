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

fn dialect(pool: &Pool) -> rustango::sql::DialectKind {
    pool.dialect().kind()
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

/// PG's current transaction isolation level.
async fn pg_level(pool: &Pool) -> String {
    let rows: Vec<(String,)> = raw_query_pool("SHOW transaction_isolation", vec![], pool)
        .await
        .unwrap();
    rows[0].0.clone()
}

/// Rows `pool` sees before and after `other` commits one more, inside one
/// block: a repeatable read sees no change.
async fn sees_a_concurrent_commit(
    p: &Pool,
    other: &Pool,
    id: i64,
    opts: AtomicOptions,
) -> (usize, usize) {
    let (q, other) = (p.clone(), other.clone());
    atomic_with(p, opts, |_tx| {
        Box::pin(async move {
            use rustango::sql::FetcherPool as _;
            let before = Note::objects().fetch(&q).await?.len();
            Note { id }.insert(&other).await?;
            let after = Note::objects().fetch(&q).await?.len();
            Ok((before, after))
        })
    })
    .await
    .unwrap()
}

/// The level is the block's own; MySQL's does not reach the next transaction.
async fn isolation_level_is_read_back_in_the_block(pool: &Pool) {
    let p = one_conn(pool).await;
    match dialect(pool) {
        rustango::sql::DialectKind::Postgres => {
            let q = p.clone();
            let opts = AtomicOptions::new().isolation(Isolation::Serializable);
            let inside = atomic_with(&p, opts, |_tx| {
                Box::pin(async move { Ok(pg_level(&q).await) })
            })
            .await
            .unwrap();
            assert_eq!(inside, "serializable");
            let q = p.clone();
            let next = rustango::atomic!(&p, |_tx| { Ok::<_, ExecError>(pg_level(&q).await) })
                .await
                .unwrap();
            assert_ne!(next, "serializable", "the level leaked");
        }
        rustango::sql::DialectKind::Mysql => {
            // Proven by behaviour: performance_schema may not be readable.
            let other = one_conn(pool).await;
            let rc = AtomicOptions::new().isolation(Isolation::ReadCommitted);
            assert_eq!(sees_a_concurrent_commit(&p, &other, 1, rc).await, (0, 1));
            // The next block on that connection is back to repeatable read.
            let next = sees_a_concurrent_commit(&p, &other, 2, AtomicOptions::new()).await;
            assert_eq!(next, (1, 1), "the level leaked");
        }
        // SQLite is always serializable: any level is accepted.
        rustango::sql::DialectKind::Sqlite => {
            let opts = AtomicOptions::new().isolation(Isolation::ReadCommitted);
            atomic_with(&p, opts, |_tx| Box::pin(async move { Ok(()) }))
                .await
                .unwrap();
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
    if dialect(pool) != rustango::sql::DialectKind::Postgres {
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
        "app..x",
        "app.x; DROP",
        "app.1x",
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
    assert!(
        TxSetting::new("app.tenant.id", "v").is_ok(),
        "more dots are fine"
    );
}

/// A nested block cannot change the level or settings; without them it is a
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
        let opts = AtomicOptions::new();
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
