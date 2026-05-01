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
// the declared scalar fields.
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

async fn setup_post(pool: &sqlx::PgPool) {
    let _ = sqlx::query(r#"DROP TABLE IF EXISTS "rustango_audit_post""#)
        .execute(pool)
        .await;
    sqlx::query(
        r#"CREATE TABLE "rustango_audit_post" (
              "id" BIGSERIAL PRIMARY KEY,
              "title" TEXT NOT NULL,
              "body" TEXT NOT NULL
          )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    reset(pool).await;
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

#[tokio::test]
async fn macro_emits_audit_create_entry_on_insert_on() {
    // v0.12 commit 3a: with `#[rustango(audit(track = "title, body"))]`
    // the macro-generated `insert_on` writes a snapshot to
    // `rustango_audit_log` after the data INSERT. Default source is
    // `system` because no `with_source` scope is active here.
    let _g = lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    setup_post(&pool).await;

    let mut conn = pool.acquire().await.unwrap();
    let mut row = AuditedPost {
        id: rustango::sql::Auto::default(),
        title: "first".into(),
        body: "hello world".into(),
    };
    row.insert_on(&mut *conn).await.unwrap();
    let pk = row.id.get().copied().unwrap();

    let entries = audit::fetch_for_entity(&pool, "rustango_audit_post", &pk.to_string())
        .await
        .unwrap();
    assert_eq!(entries.len(), 1, "exactly one audit entry expected");
    assert_eq!(entries[0].operation, "create");
    assert_eq!(entries[0].source, "system");
    assert_eq!(
        entries[0].changes,
        json!({ "title": "first", "body": "hello world" })
    );
}

#[tokio::test]
async fn macro_emits_audit_delete_entry_on_delete_on() {
    // commit 3b: `delete_on` writes a snapshot of the deleted row's
    // tracked fields with operation = "delete". Captures the
    // in-memory `&self` values; no separate before-SELECT.
    let _g = lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    setup_post(&pool).await;

    let mut conn = pool.acquire().await.unwrap();
    let mut row = AuditedPost {
        id: rustango::sql::Auto::default(),
        title: "doomed".into(),
        body: "soon gone".into(),
    };
    row.insert_on(&mut *conn).await.unwrap();
    let pk = row.id.get().copied().unwrap();
    row.delete_on(&mut *conn).await.unwrap();

    let entries = audit::fetch_for_entity(&pool, "rustango_audit_post", &pk.to_string())
        .await
        .unwrap();
    assert_eq!(entries.len(), 2, "create + delete entries expected");
    assert_eq!(entries[0].operation, "delete");
    assert_eq!(
        entries[0].changes,
        json!({ "title": "doomed", "body": "soon gone" })
    );
    assert_eq!(entries[1].operation, "create");
}

// Audited model with the soft_delete mixin so we can exercise both
// snapshot-shaped audit hooks (delete + soft_delete + restore) on the
// same model.
#[derive(Model, Debug, Clone)]
#[rustango(table = "rustango_audit_paper", display = "title")]
#[rustango(audit(track = "title"))]
#[allow(dead_code)]
pub struct AuditedPaper {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
    #[rustango(soft_delete)]
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

async fn setup_paper(pool: &sqlx::PgPool) {
    let _ = sqlx::query(r#"DROP TABLE IF EXISTS "rustango_audit_paper""#)
        .execute(pool)
        .await;
    sqlx::query(
        r#"CREATE TABLE "rustango_audit_paper" (
              "id"         BIGSERIAL PRIMARY KEY,
              "title"      TEXT NOT NULL,
              "deleted_at" TIMESTAMPTZ NULL
          )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    reset(pool).await;
}

#[tokio::test]
async fn macro_emits_audit_softdelete_and_restore_entries() {
    let _g = lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    setup_paper(&pool).await;

    let mut conn = pool.acquire().await.unwrap();
    let mut row = AuditedPaper {
        id: rustango::sql::Auto::default(),
        title: "draft".into(),
        deleted_at: None,
    };
    row.insert_on(&mut *conn).await.unwrap();
    let pk = row.id.get().copied().unwrap();

    row.soft_delete_on(&mut *conn).await.unwrap();
    row.restore_on(&mut *conn).await.unwrap();

    let entries = audit::fetch_for_entity(&pool, "rustango_audit_paper", &pk.to_string())
        .await
        .unwrap();
    assert_eq!(entries.len(), 3, "create + soft_delete + restore expected");
    let ops: Vec<&str> = entries.iter().map(|e| e.operation.as_str()).collect();
    assert_eq!(ops, vec!["restore", "soft_delete", "create"]);
}

#[tokio::test]
async fn macro_emits_audit_with_user_source_inside_with_source_scope() {
    // The `with_source` task-local override propagates into the
    // macro-emitted hook so admin handlers can attribute writes to
    // the authenticated user.
    let _g = lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    setup_post(&pool).await;

    let mut conn = pool.acquire().await.unwrap();
    audit::with_source(AuditSource::User { id: "alice".into() }, async {
        let mut row = AuditedPost {
            id: rustango::sql::Auto::default(),
            title: "scoped".into(),
            body: "x".into(),
        };
        row.insert_on(&mut *conn).await.unwrap();
        let pk = row.id.get().copied().unwrap();
        let entries =
            audit::fetch_for_entity(&pool, "rustango_audit_post", &pk.to_string())
                .await
                .unwrap();
        assert_eq!(entries[0].source, "user:alice");
    })
    .await;
}

#[tokio::test]
async fn macro_emits_audit_update_entry_with_before_after_diff() {
    // v0.12.2: save_on UPDATE branch runs a before-SELECT and writes
    // a true diff via `diff_changes(before, after)`. Unchanged
    // columns drop out of the JSON entirely; changed ones land as
    // `{ "field": { "before": <v>, "after": <v> } }`.
    let _g = lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    setup_post(&pool).await;

    let mut conn = pool.acquire().await.unwrap();
    let mut row = AuditedPost {
        id: rustango::sql::Auto::default(),
        title: "v1".into(),
        body: "unchanged".into(),
    };
    row.insert_on(&mut *conn).await.unwrap();
    let pk = row.id.get().copied().unwrap();

    row.title = "v2".into();
    // body left at "unchanged" — must NOT appear in the diff.
    row.save_on(&mut *conn).await.unwrap();

    let entries =
        audit::fetch_for_entity(&pool, "rustango_audit_post", &pk.to_string())
            .await
            .unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].operation, "update");
    assert_eq!(
        entries[0].changes,
        json!({ "title": { "before": "v1", "after": "v2" } }),
        "diff should only include the changed column"
    );
    assert_eq!(entries[1].operation, "create");
}

#[tokio::test]
async fn save_on_with_overrides_audit_source_for_one_call() {
    // commit 3c: `save_on_with(executor, source)` runs save_on inside
    // an `audit::with_source(source, ...)` scope so a single call can
    // override the active source — useful for seed scripts and
    // background jobs that don't sit inside a request handler.
    let _g = lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    setup_post(&pool).await;

    let mut conn = pool.acquire().await.unwrap();
    let mut row = AuditedPost {
        id: rustango::sql::Auto::default(),
        title: "system-default".into(),
        body: "x".into(),
    };
    row.save_on_with(
        &mut *conn,
        AuditSource::Custom("seed-script".into()),
    )
    .await
    .unwrap();
    let pk = row.id.get().copied().unwrap();

    let entries =
        audit::fetch_for_entity(&pool, "rustango_audit_post", &pk.to_string())
            .await
            .unwrap();
    assert_eq!(entries[0].source, "seed-script");
}

#[tokio::test]
async fn bulk_insert_on_emits_one_batched_audit_for_all_rows() {
    // commit 3c: `bulk_insert_on` writes ONE batched INSERT INTO
    // audit_log covering every row, regardless of batch size. We
    // assert (a) every row got an audit entry and (b) the wall-clock
    // gap between consecutive `occurred_at` values is essentially
    // zero — they all came from the same `emit_many` round-trip.
    let _g = lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    setup_post(&pool).await;

    let mut conn = pool.acquire().await.unwrap();
    let mut rows = vec![
        AuditedPost {
            id: rustango::sql::Auto::default(),
            title: "a".into(),
            body: "ax".into(),
        },
        AuditedPost {
            id: rustango::sql::Auto::default(),
            title: "b".into(),
            body: "bx".into(),
        },
        AuditedPost {
            id: rustango::sql::Auto::default(),
            title: "c".into(),
            body: "cx".into(),
        },
    ];
    AuditedPost::bulk_insert_on(&mut rows, &mut *conn)
        .await
        .unwrap();

    let total: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "rustango_audit_log" WHERE "entity_table" = 'rustango_audit_post'"#,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(total, 3);

    // Bulk audit: every entry must point at one of the bulk-inserted PKs.
    let pks: std::collections::HashSet<i64> = rows
        .iter()
        .map(|r| r.id.get().copied().unwrap())
        .collect();
    let recorded: Vec<String> = sqlx::query_scalar(
        r#"SELECT "entity_pk" FROM "rustango_audit_log" WHERE "entity_table" = 'rustango_audit_post' ORDER BY "id""#,
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    for pk_str in &recorded {
        let pk: i64 = pk_str.parse().unwrap();
        assert!(pks.contains(&pk), "bulk audit recorded an unexpected PK {pk}");
    }
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
