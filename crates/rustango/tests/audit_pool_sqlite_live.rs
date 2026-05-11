//! v0.37 slice 1 — tri-dialect audit helpers exercised against a
//! real SQLite pool.
//!
//! Covers:
//! - `audit::ensure_table_pool` bootstraps the SQLite shape
//! - `audit::emit_one_pool` writes through the Pool enum
//! - `audit::emit_many_pool` writes a batch (per-row inside a tx on
//!   sqlite — proves the fallback works)
//! - `audit::fetch_for_entity_pool` decodes JSON `changes` from the
//!   TEXT column via the dialect-agnostic JSON bridge
//! - `audit::cleanup_older_than_pool` deletes by chrono-side cutoff
//!   (no `NOW() - INTERVAL`)
//! - `audit::cleanup_keep_last_n_pool` window-function retention on
//!   SQLite 3.25+
//!
//! The audit module historically required Postgres; v0.37's first
//! deliverable lifts every helper to the tri-dialect `Pool` enum
//! using the dialect's `placeholder` + `quote_ident` emitters (no
//! hand-rolled SQL).

#![cfg(feature = "sqlite")]

use rustango::audit::{
    self, cleanup_keep_last_n_pool, cleanup_older_than_pool, emit_many_pool, emit_one_pool,
    ensure_table_pool, fetch_for_entity_pool, AuditOp, AuditSource, PendingEntry,
};
use rustango::sql::Pool;
use serde_json::json;

async fn pool() -> Pool {
    Pool::connect("sqlite::memory:")
        .await
        .expect("sqlite in-memory pool")
}

fn entry(table: &'static str, pk: &str, op: AuditOp, changes: serde_json::Value) -> PendingEntry {
    PendingEntry {
        entity_table: table,
        entity_pk: pk.to_owned(),
        operation: op,
        source: AuditSource::Custom("test".into()),
        changes,
    }
}

#[tokio::test]
async fn ensure_table_then_emit_one_then_fetch_round_trips() {
    let pool = pool().await;
    ensure_table_pool(&pool).await.expect("create table");

    let e = entry(
        "post",
        "1",
        AuditOp::Create,
        json!({"title": {"after": "Hello"}}),
    );
    emit_one_pool(&pool, &e).await.expect("emit_one");

    let rows = fetch_for_entity_pool(&pool, "post", "1")
        .await
        .expect("fetch");
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r.entity_table, "post");
    assert_eq!(r.entity_pk, "1");
    assert_eq!(r.operation, "create");
    // JSON round-trip preserves nested objects (sqlite stores
    // changes as TEXT; the fetch helper parses it back).
    assert_eq!(r.changes["title"]["after"], "Hello");
}

#[tokio::test]
async fn emit_many_writes_all_entries_in_one_tx() {
    let pool = pool().await;
    ensure_table_pool(&pool).await.unwrap();

    let entries = vec![
        entry("post", "10", AuditOp::Create, json!({"v": 1})),
        entry("post", "10", AuditOp::Update, json!({"v": 2})),
        entry("post", "10", AuditOp::SoftDelete, json!({"v": 3})),
    ];
    emit_many_pool(&pool, &entries).await.expect("emit_many");

    let rows = fetch_for_entity_pool(&pool, "post", "10").await.unwrap();
    assert_eq!(rows.len(), 3);
    // Newest first: SoftDelete → Update → Create.
    assert_eq!(rows[0].operation, "soft_delete");
    assert_eq!(rows[1].operation, "update");
    assert_eq!(rows[2].operation, "create");
}

#[tokio::test]
async fn fetch_for_entity_returns_empty_for_unknown_pk() {
    let pool = pool().await;
    ensure_table_pool(&pool).await.unwrap();
    let rows = fetch_for_entity_pool(&pool, "post", "no-such-pk")
        .await
        .unwrap();
    assert!(rows.is_empty());
}

#[tokio::test]
async fn cleanup_older_than_clears_when_cutoff_zero() {
    let pool = pool().await;
    ensure_table_pool(&pool).await.unwrap();
    for i in 0..5 {
        emit_one_pool(
            &pool,
            &entry("post", &format!("{i}"), AuditOp::Create, json!({"i": i})),
        )
        .await
        .unwrap();
    }
    // `cutoff_days = 0` → cutoff timestamp is "right now" → everything
    // is older, so the DELETE removes the lot.
    let removed = cleanup_older_than_pool(&pool, 0).await.unwrap();
    assert_eq!(removed, 5);
}

#[tokio::test]
async fn cleanup_older_than_keeps_recent_rows() {
    let pool = pool().await;
    ensure_table_pool(&pool).await.unwrap();
    emit_one_pool(
        &pool,
        &entry("post", "1", AuditOp::Create, json!({"hello": "world"})),
    )
    .await
    .unwrap();
    // 7-day cutoff → just-written row is well within the keep
    // window → DELETE removes nothing.
    let removed = cleanup_older_than_pool(&pool, 7).await.unwrap();
    assert_eq!(removed, 0);
    let still = fetch_for_entity_pool(&pool, "post", "1").await.unwrap();
    assert_eq!(still.len(), 1);
}

#[tokio::test]
async fn cleanup_keep_last_n_prunes_per_entity_history() {
    let pool = pool().await;
    ensure_table_pool(&pool).await.unwrap();
    // 4 revisions of post:1, 2 of post:2.
    for v in 1..=4 {
        emit_one_pool(&pool, &entry("post", "1", AuditOp::Update, json!({"v": v})))
            .await
            .unwrap();
    }
    for v in 1..=2 {
        emit_one_pool(&pool, &entry("post", "2", AuditOp::Update, json!({"v": v})))
            .await
            .unwrap();
    }
    // Keep the latest 2 per (table, pk). post:1 loses 2 rows;
    // post:2 keeps both — total removed = 2.
    let removed = cleanup_keep_last_n_pool(&pool, 2).await.unwrap();
    assert_eq!(removed, 2);

    let p1 = fetch_for_entity_pool(&pool, "post", "1").await.unwrap();
    assert_eq!(p1.len(), 2);
    let p2 = fetch_for_entity_pool(&pool, "post", "2").await.unwrap();
    assert_eq!(p2.len(), 2);
}

#[tokio::test]
async fn cleanup_keep_last_n_zero_clears_table() {
    let pool = pool().await;
    ensure_table_pool(&pool).await.unwrap();
    emit_one_pool(&pool, &entry("post", "1", AuditOp::Create, json!({"v": 1})))
        .await
        .unwrap();
    let removed = cleanup_keep_last_n_pool(&pool, 0).await.unwrap();
    assert_eq!(removed, 1);
}

#[tokio::test]
async fn audit_log_table_quotes_identifiers_per_dialect() {
    // Sanity check that the helper-generated SQL actually runs
    // against the SQLite quoting shape (double-quoted identifiers).
    let pool = pool().await;
    ensure_table_pool(&pool).await.unwrap();
    // Empty-input emit_many is a no-op and shouldn't even talk to
    // the database.
    let _ = audit::emit_many_pool(&pool, &[]).await.unwrap();
}

// v0.37 slice 2 — the activity-feed list/count/facet helpers used
// by `admin/audit.rs::audit_log_view` against a real SQLite pool.
// Proves the dialect-emitter SQL renders correctly for placeholders
// (`?`) and identifier quoting (double-quote).

#[tokio::test]
async fn list_pool_filters_by_entity_table() {
    let pool = pool().await;
    ensure_table_pool(&pool).await.unwrap();
    for (table, pk) in [("post", "1"), ("post", "2"), ("author", "5")] {
        emit_one_pool(
            &pool,
            &entry(table, pk, AuditOp::Create, serde_json::json!({"x": 1})),
        )
        .await
        .unwrap();
    }
    let filter = audit::AuditFilter {
        entity_table: Some("post".into()),
        ..Default::default()
    };
    let rows = audit::list_pool(&pool, &filter, 50, 0).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.entity_table == "post"));
    let total = audit::count_pool(&pool, &filter).await.unwrap();
    assert_eq!(total, 2);
}

#[tokio::test]
async fn list_pool_combines_multiple_filters() {
    let pool = pool().await;
    ensure_table_pool(&pool).await.unwrap();
    emit_one_pool(
        &pool,
        &entry("post", "1", AuditOp::Create, serde_json::json!({"v": 1})),
    )
    .await
    .unwrap();
    emit_one_pool(
        &pool,
        &entry("post", "1", AuditOp::Update, serde_json::json!({"v": 2})),
    )
    .await
    .unwrap();
    emit_one_pool(
        &pool,
        &entry("post", "2", AuditOp::Update, serde_json::json!({"v": 3})),
    )
    .await
    .unwrap();
    // post + update + pk=1 → exactly one row.
    let filter = audit::AuditFilter {
        entity_table: Some("post".into()),
        entity_pk: Some("1".into()),
        operation: Some("update".into()),
        source: None,
    };
    let rows = audit::list_pool(&pool, &filter, 50, 0).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].entity_pk, "1");
    assert_eq!(rows[0].operation, "update");
}

#[tokio::test]
async fn list_pool_paginates() {
    let pool = pool().await;
    ensure_table_pool(&pool).await.unwrap();
    for i in 0..5 {
        emit_one_pool(
            &pool,
            &entry("post", "1", AuditOp::Update, serde_json::json!({"i": i})),
        )
        .await
        .unwrap();
    }
    let filter = audit::AuditFilter::default();
    let page1 = audit::list_pool(&pool, &filter, 2, 0).await.unwrap();
    let page2 = audit::list_pool(&pool, &filter, 2, 2).await.unwrap();
    let page3 = audit::list_pool(&pool, &filter, 2, 4).await.unwrap();
    assert_eq!(page1.len(), 2);
    assert_eq!(page2.len(), 2);
    assert_eq!(page3.len(), 1);
    // Newest-first ordering — id descends across pages.
    assert!(page1[0].id > page2[0].id);
    assert!(page2[0].id > page3[0].id);
}

#[tokio::test]
async fn facet_counts_returns_groupby() {
    let pool = pool().await;
    ensure_table_pool(&pool).await.unwrap();
    for (table, op) in [
        ("post", AuditOp::Create),
        ("post", AuditOp::Update),
        ("post", AuditOp::Update),
        ("author", AuditOp::Create),
    ] {
        emit_one_pool(&pool, &entry(table, "1", op, serde_json::json!({})))
            .await
            .unwrap();
    }
    // entity_table facet: post=3, author=1, sorted count-desc.
    let facets = audit::facet_counts_pool(&pool, "entity_table")
        .await
        .unwrap();
    assert_eq!(facets[0], ("post".to_string(), 3));
    assert_eq!(facets[1], ("author".to_string(), 1));

    let ops = audit::facet_counts_pool(&pool, "operation").await.unwrap();
    assert!(ops.iter().find(|(v, c)| v == "update" && *c == 2).is_some());
    assert!(ops.iter().find(|(v, c)| v == "create" && *c == 2).is_some());
}

#[tokio::test]
async fn facet_counts_rejects_non_allowlisted_column() {
    let pool = pool().await;
    ensure_table_pool(&pool).await.unwrap();
    let r = audit::facet_counts_pool(&pool, "no_such_column").await;
    assert!(r.is_err(), "non-allowlisted column should be rejected");
}
