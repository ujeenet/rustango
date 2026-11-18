//! The `_on` forms of bulk update, upsert, insert-or-ignore, date buckets
//! and increment: each runs on the `PoolTx` it is given, so a rollback
//! undoes it.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::{BulkUpdateQuery, Model as _, SqlValue};
use rustango::query::DateKind;
use rustango::sql::{
    bulk_update_on, fetch_dates_on, fetch_datetimes_on, model_shortcuts::increment_one_on,
    transaction_pool, FetcherPool as _, Pool,
};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "eof_item")]
#[allow(dead_code)]
pub struct Item {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 20)]
    pub label: String,
    pub n: i64,
    pub at: chrono::DateTime<chrono::Utc>,
}

fn item(id: i64, label: &str) -> Item {
    Item {
        id,
        label: label.into(),
        n: 0,
        at: chrono::DateTime::from_timestamp(1_700_000_000 + id * 86_400, 0).unwrap(),
    }
}

async fn rows(pool: &Pool) -> Vec<(i64, String, i64)> {
    let rows: Vec<Item> = Item::objects()
        .order_by(&[("id", false)])
        .fetch(pool)
        .await
        .unwrap();
    rows.into_iter().map(|r| (r.id, r.label, r.n)).collect()
}

/// Every write `_on` form runs on the transaction: seen inside, gone after
/// its rollback.
async fn writes_run_on_the_transaction(pool: &Pool) {
    item(1, "a").insert(pool).await.unwrap();
    let before = rows(pool).await;
    let mut tx = transaction_pool(pool).await.unwrap();
    let update = BulkUpdateQuery::new(
        Item::SCHEMA,
        vec!["label"],
        vec![vec![SqlValue::I64(1), SqlValue::String("b".into())]],
    );
    assert_eq!(bulk_update_on(&mut tx, &update).await.unwrap(), 1);
    Item::bulk_upsert_on(&[item(1, "c"), item(2, "d")], &["id"], &["label"], &mut tx)
        .await
        .unwrap();
    Item::bulk_insert_or_ignore_on(&[item(2, "skipped"), item(3, "e")], &mut tx)
        .await
        .unwrap();
    increment_one_on::<Item, _>("id", SqlValue::I64(3), "n", 5, &mut tx)
        .await
        .unwrap();
    let inside: Vec<Item> = Item::objects()
        .order_by(&[("id", false)])
        .fetch_on(&mut tx)
        .await
        .unwrap();
    let inside: Vec<_> = inside.into_iter().map(|r| (r.id, r.label, r.n)).collect();
    assert_eq!(
        inside,
        vec![(1, "c".into(), 0), (2, "d".into(), 0), (3, "e".into(), 5)]
    );
    tx.rollback().await.unwrap();
    assert_eq!(rows(pool).await, before, "a write escaped the transaction");
}

/// The date bucket `_on` forms read through the transaction too.
async fn date_buckets_read_on_the_transaction(pool: &Pool) {
    let mut tx = transaction_pool(pool).await.unwrap();
    for id in [1, 2] {
        item(id, "x").insert_tx(&mut tx).await.unwrap();
    }
    let days = fetch_dates_on(&mut tx, Item::objects().dates("at", DateKind::Day))
        .await
        .unwrap();
    assert_eq!(days.len(), 2);
    let stamps = fetch_datetimes_on(
        &mut tx,
        Item::objects().datetimes("at", rustango::query::DateTimeKind::Hour),
    )
    .await
    .unwrap();
    assert_eq!(stamps.len(), 2);
    tx.rollback().await.unwrap();
}

tri_dialect_test! {
    model: Item,
    scenarios: [
        writes_run_on_the_transaction,
        date_buckets_read_on_the_transaction,
    ],
}
