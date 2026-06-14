//! v0.39 — dialect-agnostic transaction helpers exercised on SQLite.
//!
//! Proves the new `_tx` family works end-to-end:
//!   * `transaction_pool(&pool)` opens a `PoolTx` on a SQLite pool
//!   * `insert_tx`, `save_tx`, `delete_tx`, `fetch_tx` round-trip
//!   * `tx.commit()` durably persists writes
//!   * `tx.rollback()` (or implicit drop) discards them

#![cfg(all(feature = "sqlite", feature = "postgres"))]

use rustango::core::Column as _;
use rustango::sql::{sqlx, transaction_pool, Auto, FetcherPool, FetcherTx, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "tx_widget")]
pub struct Widget {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub label: String,
    pub count: i32,
}

async fn fresh_pool() -> Pool {
    let pool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE tx_widget (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            label TEXT NOT NULL, \
            count INTEGER NOT NULL DEFAULT 0)",
    )
    .execute(&pool)
    .await
    .expect("widget table");
    Pool::Sqlite(pool)
}

#[tokio::test]
async fn tx_insert_save_delete_commit_round_trip() {
    let pool = fresh_pool().await;
    let mut tx = transaction_pool(&pool).await.expect("begin tx");

    let mut w = Widget {
        id: Auto::default(),
        label: "alpha".to_owned(),
        count: 1,
    };
    w.insert_tx(&mut tx).await.expect("insert_tx");
    let id = w.id.get().copied().expect("auto-id assigned");
    assert!(id > 0, "sqlite assigned an auto-PK");

    w.count = 42;
    w.save_tx(&mut tx).await.expect("save_tx update");

    let rows: Vec<Widget> = Widget::objects()
        .where_(Widget::id.eq(id))
        .fetch_tx(&mut tx)
        .await
        .expect("fetch_tx");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].count, 42);
    assert_eq!(rows[0].label, "alpha");

    tx.commit().await.expect("commit");

    let after: Vec<Widget> = Widget::objects()
        .where_(Widget::id.eq(id))
        .fetch(&pool)
        .await
        .expect("post-commit fetch");
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].count, 42);

    let mut tx2 = transaction_pool(&pool).await.expect("begin tx2");
    let deleted = after[0].delete_tx(&mut tx2).await.expect("delete_tx");
    assert_eq!(deleted, 1);
    tx2.commit().await.expect("commit tx2");

    let gone: Vec<Widget> = Widget::objects()
        .where_(Widget::id.eq(id))
        .fetch(&pool)
        .await
        .expect("post-delete fetch");
    assert!(gone.is_empty(), "row truly deleted");
}

#[tokio::test]
async fn tx_rollback_discards_writes() {
    let pool = fresh_pool().await;
    let mut tx = transaction_pool(&pool).await.expect("begin tx");

    let mut w = Widget {
        id: Auto::default(),
        label: "rollback-me".to_owned(),
        count: 7,
    };
    w.insert_tx(&mut tx).await.expect("insert_tx");
    let id = w.id.get().copied().expect("auto-id");

    tx.rollback().await.expect("rollback");

    let rows: Vec<Widget> = Widget::objects()
        .where_(Widget::id.eq(id))
        .fetch(&pool)
        .await
        .expect("post-rollback fetch");
    assert!(rows.is_empty(), "rollback discarded the insert");
}

#[tokio::test]
async fn pool_tx_dialect_returns_sqlite() {
    let pool = fresh_pool().await;
    let tx = transaction_pool(&pool).await.expect("begin tx");
    assert_eq!(tx.dialect().name(), "sqlite");
    assert_eq!(tx.dialect().placeholder(1), "?");
    tx.rollback().await.expect("rollback");
}
