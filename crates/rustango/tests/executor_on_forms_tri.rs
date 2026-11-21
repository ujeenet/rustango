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
    let day = |d| chrono::NaiveDate::from_ymd_opt(2023, 11, d).unwrap();
    assert_eq!(days, vec![day(15), day(16)]);
    let stamps = fetch_datetimes_on(
        &mut tx,
        Item::objects().datetimes("at", rustango::query::DateTimeKind::Hour),
    )
    .await
    .unwrap();
    let hour = |d| day(d).and_hms_opt(22, 0, 0).unwrap().and_utc();
    assert_eq!(stamps, vec![hour(15), hour(16)]);
    tx.rollback().await.unwrap();
}

/// `UPDATE eof_item SET n = <n> WHERE id = <id>` in the pool's dialect.
fn set_n(pool: &Pool, id: i64, n: SqlValue) -> rustango::sql::CompiledStatement {
    let d = pool.dialect();
    let sql = format!(
        "UPDATE eof_item SET n = {} WHERE id = {}",
        d.placeholder(1),
        d.placeholder(2)
    );
    rustango::sql::CompiledStatement::new(d, sql, vec![n, SqlValue::I64(id)]).unwrap()
}

/// Several batches run all or none on a pool and a held connection, and in
/// the given transaction.
async fn batches_run_all_or_none(pool: &Pool) {
    use rustango::sql::SqlExecutor as _;
    for id in [1, 2] {
        item(id, "x").insert(pool).await.unwrap();
    }
    // The second batch fails (NOT NULL), so the first must not stay.
    let failing = || {
        vec![
            set_n(pool, 1, SqlValue::I64(7)),
            set_n(pool, 2, SqlValue::Null),
        ]
    };
    assert!(pool.run_batches(failing()).await.is_err());
    let mut conn = pool.acquire().await.unwrap();
    assert!((&mut conn).run_batches(failing()).await.is_err());
    drop(conn);
    assert_eq!(
        rows(pool).await[0].2,
        0,
        "a failed batch kept the first one"
    );
    let ok = || {
        vec![
            set_n(pool, 1, SqlValue::I64(7)),
            set_n(pool, 2, SqlValue::I64(8)),
        ]
    };
    assert_eq!(pool.run_batches(ok()).await.unwrap(), 2);
    let mut tx = transaction_pool(pool).await.unwrap();
    let more = vec![
        set_n(pool, 1, SqlValue::I64(9)),
        set_n(pool, 2, SqlValue::I64(9)),
    ];
    assert_eq!((&mut tx).run_batches(more).await.unwrap(), 2);
    tx.rollback().await.unwrap();
    let ns: Vec<i64> = rows(pool).await.into_iter().map(|r| r.2).collect();
    assert_eq!(
        ns,
        vec![7, 8],
        "the transaction's batches rolled back with it"
    );
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "eof_audited", audit(track = "label"))]
#[allow(dead_code)]
pub struct Audited {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 20)]
    pub label: String,
}

/// The `_on` upserts cannot audit, so an audited model is refused.
async fn audited_upserts_are_refused(pool: &Pool) {
    let row = Audited {
        id: 1,
        label: "a".into(),
    };
    let up = Audited::bulk_upsert_on(&[row.clone()], &["id"], &["label"], pool).await;
    assert!(
        matches!(up, Err(rustango::sql::ExecError::AuditUnsupported { .. })),
        "got {up:?}"
    );
    let ig = Audited::bulk_insert_or_ignore_on(&[row], pool).await;
    assert!(
        matches!(ig, Err(rustango::sql::ExecError::AuditUnsupported { .. })),
        "got {ig:?}"
    );
}

tri_dialect_test! {
    model: Item,
    scenarios: [
        writes_run_on_the_transaction,
        date_buckets_read_on_the_transaction,
        batches_run_all_or_none,
        audited_upserts_are_refused,
    ],
}
