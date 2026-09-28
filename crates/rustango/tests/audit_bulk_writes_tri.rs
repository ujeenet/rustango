//! Bulk writes on audited models write their audit rows in the write's
//! transaction: one per affected row, or one bulk entry for `truncate`
//! (#1747).

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::audit::{self, AuditLog};
use rustango::sql::{Auto, CounterPool as _, Pool};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "audit1747_item",
    app = "audit1747",
    audit(track = "name, score")
)]
#[allow(dead_code)]
pub struct Item {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub name: String,
    pub score: i64,
}

/// Natural-PK model: exercises the non-`Auto` bulk insert.
#[derive(Model, Debug, Clone)]
#[rustango(table = "audit1747_tag", app = "audit1747", audit(track = "label"))]
#[allow(dead_code)]
pub struct Tag {
    #[rustango(primary_key, max_length = 32)]
    pub slug: String,
    #[rustango(max_length = 64)]
    pub label: String,
}

const ITEM: &str = "audit1747_item";
const TAG: &str = "audit1747_tag";

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Item>(pool).await;
    rustango::testkit::matrix::fresh_table::<Tag>(pool).await;
    audit::ensure_table_pool(pool).await.expect("audit table");
    for table in [ITEM, TAG] {
        AuditLog::delete_where("entity_table", table, pool)
            .await
            .expect("clear audit rows");
    }
}

async fn ops(pool: &Pool, table: &str, operation: &str) -> i64 {
    AuditLog::objects()
        .filter("entity_table", table)
        .filter("operation", operation)
        .count(pool)
        .await
        .expect("count audit rows")
}

/// The newest entry for one row.
async fn latest(pool: &Pool, table: &str, pk: &str) -> audit::AuditEntry {
    audit::fetch_for_entity_pool(pool, table, pk)
        .await
        .expect("fetch audit rows")
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("no audit row for {table}/{pk}"))
}

/// Three items: `a` (1), `b` (2), `b` (3). Returns their PKs.
async fn seed(pool: &Pool) -> Vec<i64> {
    let mut pks = Vec::new();
    for (name, score) in [("a", 1), ("b", 2), ("b", 3)] {
        let mut item = Item {
            id: Auto::default(),
            name: name.into(),
            score,
        };
        item.insert_pool(pool).await.expect("insert");
        pks.push(*item.id.get().expect("pk assigned"));
    }
    assert_eq!(ops(pool, ITEM, "create").await, 3);
    pks
}

async fn destroy_audits_each_deleted_row(pool: &Pool) {
    let pks = seed(pool).await;
    assert_eq!(Item::destroy([pks[0], pks[2]], pool).await.unwrap(), 2);
    assert_eq!(ops(pool, ITEM, "delete").await, 2);
    let entry = latest(pool, ITEM, &pks[2].to_string()).await;
    assert_eq!(entry.operation, "delete");
    assert_eq!(entry.changes["score"], 3);
}

async fn delete_where_audits_each_deleted_row(pool: &Pool) {
    let pks = seed(pool).await;
    assert_eq!(Item::delete_where("name", "b", pool).await.unwrap(), 2);
    assert_eq!(ops(pool, ITEM, "delete").await, 2);
    assert_eq!(
        latest(pool, ITEM, &pks[1].to_string()).await.operation,
        "delete"
    );
    assert_eq!(
        latest(pool, ITEM, &pks[0].to_string()).await.operation,
        "create"
    );
}

async fn update_where_audits_the_written_values(pool: &Pool) {
    let pks = seed(pool).await;
    assert_eq!(
        Item::update_where("name", "b", "score", 7_i64, pool)
            .await
            .unwrap(),
        2
    );
    assert_eq!(ops(pool, ITEM, "update").await, 2);
    let entry = latest(pool, ITEM, &pks[1].to_string()).await;
    assert_eq!(entry.operation, "update");
    assert_eq!(entry.changes["score"], 7);
}

async fn update_all_audits_every_row(pool: &Pool) {
    seed(pool).await;
    assert_eq!(Item::update_all("name", "z", pool).await.unwrap(), 3);
    assert_eq!(ops(pool, ITEM, "update").await, 3);
}

async fn increment_each_audits_the_new_values(pool: &Pool) {
    let pks = seed(pool).await;
    assert_eq!(Item::increment_each("score", 5, pool).await.unwrap(), 3);
    assert_eq!(ops(pool, ITEM, "update").await, 3);
    assert_eq!(
        latest(pool, ITEM, &pks[0].to_string()).await.changes["score"],
        6
    );
}

async fn truncate_writes_one_bulk_entry(pool: &Pool) {
    seed(pool).await;
    Item::truncate(pool).await.unwrap();
    assert_eq!(ops(pool, ITEM, "delete").await, 1);
    let entry = latest(pool, ITEM, "").await;
    assert_eq!(entry.changes["bulk"], "truncate");
}

/// A failed bulk write commits neither the data nor any audit row.
async fn failed_bulk_write_writes_no_audit_row(pool: &Pool) {
    for slug in ["a", "b"] {
        Tag {
            slug: slug.into(),
            label: "x".into(),
        }
        .insert_pool(pool)
        .await
        .expect("insert");
    }
    // Both rows to one PK: a key violation on every backend.
    let err = Tag::update_all("slug", "dup", pool).await.unwrap_err();
    assert!(matches!(err, rustango::sql::ExecError::Driver(_)), "{err}");
    assert_eq!(ops(pool, TAG, "update").await, 0);
    assert_eq!(
        Tag::objects()
            .filter("slug", "a")
            .count(pool)
            .await
            .unwrap(),
        1
    );
}

/// `bulk_insert` and `upsert` are PostgreSQL-only methods.
async fn bulk_insert_and_upsert_audit_on_postgres(pool: &Pool) {
    let _ = pool;
    #[cfg(feature = "postgres")]
    #[allow(irrefutable_let_patterns)]
    if let Pool::Postgres(pg) = pool {
        let tags = [
            Tag {
                slug: "p".into(),
                label: "one".into(),
            },
            Tag {
                slug: "q".into(),
                label: "two".into(),
            },
        ];
        Tag::bulk_insert(&tags, pg).await.expect("bulk insert");
        assert_eq!(ops(pool, TAG, "create").await, 2);
        assert_eq!(latest(pool, TAG, "q").await.changes["label"], "two");

        let mut item = Item {
            id: Auto::default(),
            name: "u".into(),
            score: 1,
        };
        item.upsert(pg).await.expect("upsert insert");
        let pk = item.id.get().expect("pk assigned").to_string();
        assert_eq!(latest(pool, ITEM, &pk).await.operation, "create");
        item.score = 9;
        item.upsert(pg).await.expect("upsert update");
        let entry = latest(pool, ITEM, &pk).await;
        assert_eq!(entry.operation, "update");
        assert_eq!(entry.changes["score"], 9);
    }
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        destroy_audits_each_deleted_row,
        delete_where_audits_each_deleted_row,
        update_where_audits_the_written_values,
        update_all_audits_every_row,
        increment_each_audits_the_new_values,
        truncate_writes_one_bulk_entry,
        failed_bulk_write_writes_no_audit_row,
        bulk_insert_and_upsert_audit_on_postgres,
    ],
}
