//! Audited `insert_pool`, `soft_delete`, `restore` and `delete_pool` on
//! `&Pool` each write one audit row, keyed by the row's real PK (#1675).

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use chrono::{DateTime, Utc};
use rustango::audit::{self, AuditEntry, AuditLog};
use rustango::sql::{Auto, CounterPool as _, Pool};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "audit1675_note",
    app = "audit1675",
    audit(track = "title, deleted_at")
)]
#[allow(dead_code)]
pub struct Note {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
    #[rustango(soft_delete)]
    pub deleted_at: Option<DateTime<Utc>>,
}

const TABLE: &str = "audit1675_note";

/// Fresh table, and no audit rows left for it by an earlier run.
async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Note>(pool).await;
    audit::ensure_table_pool(pool).await.expect("audit table");
    AuditLog::delete_where("entity_table", TABLE, pool)
        .await
        .expect("clear audit rows");
}

async fn audit_rows(pool: &Pool) -> i64 {
    AuditLog::objects()
        .filter("entity_table", TABLE)
        .count(pool)
        .await
        .expect("count audit rows")
}

async fn entries(pool: &Pool, pk: &str) -> Vec<AuditEntry> {
    audit::fetch_for_entity_pool(pool, TABLE, pk)
        .await
        .expect("fetch audit rows")
}

async fn insert_note(pool: &Pool) -> Note {
    let mut note = Note {
        id: Auto::default(),
        title: "hello".into(),
        deleted_at: None,
    };
    note.insert_pool(pool).await.expect("insert");
    note
}

async fn insert_records_the_assigned_pk(pool: &Pool) {
    let note = insert_note(pool).await;
    let pk = note.id.get().expect("pk assigned").to_string();
    assert_eq!(audit_rows(pool).await, 1);
    let rows = entries(pool, &pk).await;
    assert_eq!(rows.len(), 1, "the audit row must carry PK {pk}");
    assert_eq!(rows[0].operation, "create");
    assert_eq!(rows[0].changes["title"], "hello");
}

async fn soft_delete_and_restore_each_write_one_row(pool: &Pool) {
    let mut note = insert_note(pool).await;
    let pk = note.id.get().expect("pk assigned").to_string();

    assert_eq!(note.soft_delete(pool).await.unwrap(), 1);
    assert_eq!(audit_rows(pool).await, 2);
    let rows = entries(pool, &pk).await;
    assert_eq!(rows[0].operation, "soft_delete");
    assert!(
        !rows[0].changes["deleted_at"].is_null(),
        "records the written time"
    );

    // Stale in-memory value: the audit row must still record NULL.
    note.deleted_at = Some(Utc::now());
    assert_eq!(note.restore(pool).await.unwrap(), 1);
    assert_eq!(audit_rows(pool).await, 3);
    let rows = entries(pool, &pk).await;
    assert_eq!(rows[0].operation, "restore");
    assert!(rows[0].changes["deleted_at"].is_null());
}

async fn delete_writes_one_row(pool: &Pool) {
    let note = insert_note(pool).await;
    let pk = note.id.get().expect("pk assigned").to_string();
    assert_eq!(note.delete_pool(pool).await.unwrap(), 1);
    assert_eq!(audit_rows(pool).await, 2);
    let rows = entries(pool, &pk).await;
    assert_eq!(rows[0].operation, "delete");
    assert_eq!(rows[0].changes["title"], "hello");
}

async fn no_row_changed_writes_no_audit_row(pool: &Pool) {
    let note = insert_note(pool).await;
    assert_eq!(note.delete_pool(pool).await.unwrap(), 1);
    assert_eq!(audit_rows(pool).await, 2);
    assert_eq!(note.delete_pool(pool).await.unwrap(), 0);
    assert_eq!(note.soft_delete(pool).await.unwrap(), 0);
    assert_eq!(note.restore(pool).await.unwrap(), 0);
    assert_eq!(audit_rows(pool).await, 2);
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        insert_records_the_assigned_pk,
        soft_delete_and_restore_each_write_one_row,
        delete_writes_one_row,
        no_row_changed_writes_no_audit_row,
    ],
}
