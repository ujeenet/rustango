//! Audited `insert_pool`, `soft_delete`, `restore` and `delete_pool` on
//! `&Pool` each write one audit row, keyed by the row's real PK (#1675).

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use chrono::{DateTime, Utc};
use rustango::audit::{self, AuditEntry, AuditLog};
use rustango::sql::{Auto, CounterPool as _, FetcherPool as _, Pool};
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

/// Tracks a column the UPDATE never writes, so dropping it fails only
/// the pre-read (#1907).
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "audit1907_stamp",
    app = "audit1675",
    audit(track = "title, created_at")
)]
#[allow(dead_code)]
pub struct Stamp {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

const TABLE: &str = "audit1675_note";

/// Fresh table, and no audit rows left for it by an earlier run.
async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Note>(pool).await;
    rustango::testkit::matrix::fresh_table::<Stamp>(pool).await;
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

async fn noop_save_writes_no_audit_row(pool: &Pool) {
    let mut note = insert_note(pool).await;
    note.save_pool(pool).await.expect("no-op save");
    assert_eq!(
        audit_rows(pool).await,
        1,
        "a no-op save wrote an update row"
    );
    note.title = "changed".into();
    note.save_pool(pool).await.expect("save");
    assert_eq!(audit_rows(pool).await, 2);
}

async fn failed_pre_read_fails_the_save(pool: &Pool) {
    // SQLite reads a missing double-quoted column as a string literal,
    // so its pre-read cannot be made to fail this way.
    if pool.dialect().name() == "sqlite" {
        return;
    }
    let mut stamp = Stamp {
        id: Auto::default(),
        title: "old".into(),
        created_at: Auto::default(),
    };
    stamp.insert_pool(pool).await.expect("insert");
    let d = pool.dialect();
    let sql = format!(
        "ALTER TABLE {} DROP COLUMN {}",
        d.quote_ident("audit1907_stamp"),
        d.quote_ident("created_at")
    );
    rustango::sql::raw_execute_pool(pool, &sql, Vec::new())
        .await
        .expect("drop column");
    stamp.title = "new".into();
    assert!(stamp.save_pool(pool).await.is_err(), "save ran unaudited");
    let updated = Stamp::objects()
        .filter("title", "new")
        .count(pool)
        .await
        .expect("count");
    assert_eq!(updated, 0, "the UPDATE committed without its audit row");
}

async fn second_soft_delete_keeps_the_first_stamp(pool: &Pool) {
    let note = insert_note(pool).await;
    assert_eq!(note.soft_delete(pool).await.unwrap(), 1);
    let first = Note::objects().fetch(pool).await.unwrap()[0].deleted_at;
    assert_eq!(note.soft_delete(pool).await.unwrap(), 0, "re-stamped");
    assert_eq!(
        Note::objects().fetch(pool).await.unwrap()[0].deleted_at,
        first
    );
    assert_eq!(
        audit_rows(pool).await,
        2,
        "a second soft delete wrote an audit row"
    );
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        insert_records_the_assigned_pk,
        soft_delete_and_restore_each_write_one_row,
        delete_writes_one_row,
        no_row_changed_writes_no_audit_row,
        noop_save_writes_no_audit_row,
        failed_pre_read_fails_the_save,
        second_soft_delete_keeps_the_first_stamp,
    ],
}
