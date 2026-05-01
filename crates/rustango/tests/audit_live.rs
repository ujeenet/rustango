//! Live tests for v0.12 commit 2 — audit primitives.
//!
//! Verifies the runtime API (`ensure_table`, `emit_one`, `emit_many`,
//! `fetch_for_entity`) without relying on the macro-generated audit
//! hooks (those land in commit 3). Macro parsing of
//! `#[rustango(audit(track = "..."))]` is exercised by compiling
//! the fixture model — field-name validation runs at compile time.
//!
//! Skipped silently when `DATABASE_URL` is unset.

use std::sync::OnceLock;

use rustango::audit::{
    self, AuditOp, AuditSource, PendingEntry,
};
use rustango::sql::sqlx;
use rustango::Model;
use serde_json::json;
use tokio::sync::Mutex;

// Compiles only when `audit(track = ...)` parses + validates against
// the declared scalar fields. The runtime audit emission this model
// triggers ships in commit 3.
#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_audit_post", display = "title")]
#[rustango(audit(track = "title, body"))]
#[allow(dead_code)]
pub struct AuditedPost {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
    #[rustango(max_length = 200)]
    pub body: String,
}

fn lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

async fn reset(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "rustango_audit_log" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    audit::ensure_table(pool).await.unwrap();
}

#[tokio::test]
async fn ensure_table_is_idempotent() {
    let _g = lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    reset(&pool).await;
    audit::ensure_table(&pool).await.unwrap();
    audit::ensure_table(&pool).await.unwrap(); // second call must not error
}

#[tokio::test]
async fn emit_one_persists_a_pending_entry() {
    let _g = lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    reset(&pool).await;

    let entry = PendingEntry {
        entity_table: "post",
        entity_pk: "42".into(),
        operation: AuditOp::Create,
        source: AuditSource::User { id: "alice".into() },
        changes: json!({ "title": "first", "body": "hello" }),
    };
    audit::emit_one(&pool, &entry).await.unwrap();

    let rows = audit::fetch_for_entity(&pool, "post", "42").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].operation, "create");
    assert_eq!(rows[0].source, "user:alice");
    assert_eq!(rows[0].changes, json!({ "title": "first", "body": "hello" }));
}

#[tokio::test]
async fn emit_many_batches_multiple_entries_in_one_statement() {
    let _g = lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    reset(&pool).await;

    let entries: Vec<PendingEntry> = (1..=3)
        .map(|i| PendingEntry {
            entity_table: "post",
            entity_pk: i.to_string(),
            operation: AuditOp::Create,
            source: AuditSource::System,
            changes: json!({ "title": format!("p{i}") }),
        })
        .collect();
    audit::emit_many(&pool, &entries).await.unwrap();

    let total: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "rustango_audit_log" WHERE "entity_table" = 'post'"#,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(total, 3);
    let one = audit::fetch_for_entity(&pool, "post", "2").await.unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].changes, json!({ "title": "p2" }));
}

#[tokio::test]
async fn emit_many_with_empty_input_is_a_noop() {
    let _g = lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    reset(&pool).await;
    audit::emit_many(&pool, &[]).await.unwrap();
    let total: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "rustango_audit_log""#)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(total, 0);
}

#[tokio::test]
async fn fetch_for_entity_orders_newest_first() {
    let _g = lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    reset(&pool).await;

    for op in [AuditOp::Create, AuditOp::Update, AuditOp::SoftDelete] {
        let entry = PendingEntry {
            entity_table: "post",
            entity_pk: "9".into(),
            operation: op,
            source: AuditSource::System,
            changes: json!({}),
        };
        audit::emit_one(&pool, &entry).await.unwrap();
        // Tiny gap so occurred_at advances measurably across rows.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let rows = audit::fetch_for_entity(&pool, "post", "9").await.unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].operation, "soft_delete");
    assert_eq!(rows[1].operation, "update");
    assert_eq!(rows[2].operation, "create");
}

#[tokio::test]
async fn with_source_scope_overrides_default_system() {
    // No DB needed — exercises the task-local in isolation.
    assert!(matches!(audit::current_source(), AuditSource::System));
    let captured = audit::with_source(AuditSource::User { id: "bob".into() }, async {
        match audit::current_source() {
            AuditSource::User { id } => id,
            other => panic!("unexpected source {other:?}"),
        }
    })
    .await;
    assert_eq!(captured, "bob");
    // Outside the scope, defaults restored.
    assert!(matches!(audit::current_source(), AuditSource::System));
}

#[test]
fn diff_changes_skips_unchanged_fields() {
    let before = vec![
        ("title", json!("old")),
        ("body", json!("hello")),
    ];
    let after = vec![
        ("title", json!("new")),
        ("body", json!("hello")),
    ];
    let diff = audit::diff_changes(&before, &after);
    assert_eq!(
        diff,
        json!({ "title": { "before": "old", "after": "new" } })
    );
}

#[test]
fn snapshot_changes_captures_all_after_values() {
    let after = vec![
        ("title", json!("first")),
        ("body", json!("hello")),
    ];
    let snap = audit::snapshot_changes(&after);
    assert_eq!(snap, json!({ "title": "first", "body": "hello" }));
}

#[test]
fn audit_source_token_is_stable() {
    assert_eq!(AuditSource::System.as_token(), "system");
    assert_eq!(
        AuditSource::User { id: "42".into() }.as_token(),
        "user:42"
    );
    assert_eq!(
        AuditSource::Custom("webhook:stripe".into()).as_token(),
        "webhook:stripe"
    );
}
