//! v0.41 — MySQL parity for the tri-dialect audit log helpers.
//!
//! Mirrors `audit_pool_sqlite_live.rs`. Covers:
//! - `ensure_table_pool` creates the audit table with the MySQL shape
//!   (JSON column type, BIGINT AUTO_INCREMENT id)
//! - `emit_one_pool` / `emit_many_pool` insert rows through the
//!   Pool enum dispatch
//! - `fetch_for_entity_pool` round-trips JSON `changes`
//! - `cleanup_older_than_pool` deletes by chrono-side cutoff (not
//!   PG's `NOW() - INTERVAL`)
//! - `cleanup_keep_last_n_pool` per-entity retention via window
//!   functions (MySQL 8.0+ supports `ROW_NUMBER() OVER`)
//! - `list` / `count` / `facet_counts` activity-feed
//!   helpers
//!
//! Reads `MYSQL_TEST_URL`. Tests skip silently when unset so
//! `cargo test` stays green offline.

#![cfg(feature = "mysql")]

use std::sync::OnceLock;

use rustango::audit::{
    self, cleanup_keep_last_n_pool, cleanup_older_than_pool, emit_many_pool, emit_one_pool,
    ensure_table_pool, fetch_for_entity_pool, AuditOp, AuditSource, PendingEntry,
};
use rustango::sql::Pool;
use serde_json::json;

fn serial_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

async fn mysql_pool() -> Option<Pool> {
    let url = std::env::var("MYSQL_TEST_URL").ok()?;
    let mp = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("mysql connect");
    let _ = sqlx::query("DROP TABLE IF EXISTS rustango_audit_log")
        .execute(&mp)
        .await;
    let pool = Pool::Mysql(mp);
    ensure_table_pool(&pool).await.expect("ensure_table_pool");
    Some(pool)
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
async fn ensure_table_then_emit_one_then_fetch_round_trips_on_mysql() {
    let _serial = serial_lock().lock().await;
    let Some(pool) = mysql_pool().await else {
        return;
    };
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
    assert_eq!(r.changes["title"]["after"], "Hello");
}

#[tokio::test]
async fn emit_many_writes_all_entries_in_one_tx_on_mysql() {
    let _serial = serial_lock().lock().await;
    let Some(pool) = mysql_pool().await else {
        return;
    };
    let entries = vec![
        entry("post", "10", AuditOp::Create, json!({"v": 1})),
        entry("post", "10", AuditOp::Update, json!({"v": 2})),
        entry("post", "10", AuditOp::SoftDelete, json!({"v": 3})),
    ];
    emit_many_pool(&pool, &entries).await.expect("emit_many");

    let rows = fetch_for_entity_pool(&pool, "post", "10").await.unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].operation, "soft_delete");
    assert_eq!(rows[1].operation, "update");
    assert_eq!(rows[2].operation, "create");
}

#[tokio::test]
async fn cleanup_older_than_clears_when_cutoff_zero_on_mysql() {
    let _serial = serial_lock().lock().await;
    let Some(pool) = mysql_pool().await else {
        return;
    };
    for i in 0..5 {
        emit_one_pool(
            &pool,
            &entry("post", &format!("{i}"), AuditOp::Create, json!({"i": i})),
        )
        .await
        .unwrap();
    }
    let removed = cleanup_older_than_pool(&pool, 0).await.unwrap();
    assert_eq!(removed, 5);
}

#[tokio::test]
async fn cleanup_older_than_keeps_recent_rows_on_mysql() {
    let _serial = serial_lock().lock().await;
    let Some(pool) = mysql_pool().await else {
        return;
    };
    emit_one_pool(
        &pool,
        &entry("post", "1", AuditOp::Create, json!({"hello": "world"})),
    )
    .await
    .unwrap();
    let removed = cleanup_older_than_pool(&pool, 7).await.unwrap();
    assert_eq!(removed, 0);
    let still = fetch_for_entity_pool(&pool, "post", "1").await.unwrap();
    assert_eq!(still.len(), 1);
}

#[tokio::test]
async fn cleanup_keep_last_n_prunes_per_entity_history_on_mysql() {
    let _serial = serial_lock().lock().await;
    let Some(pool) = mysql_pool().await else {
        return;
    };
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
    let removed = cleanup_keep_last_n_pool(&pool, 2).await.unwrap();
    assert_eq!(removed, 2);

    let p1 = fetch_for_entity_pool(&pool, "post", "1").await.unwrap();
    assert_eq!(p1.len(), 2);
    let p2 = fetch_for_entity_pool(&pool, "post", "2").await.unwrap();
    assert_eq!(p2.len(), 2);
}

#[tokio::test]
async fn list_pool_filters_and_paginates_on_mysql() {
    let _serial = serial_lock().lock().await;
    let Some(pool) = mysql_pool().await else {
        return;
    };
    for (table, pk) in [("post", "1"), ("post", "2"), ("author", "5")] {
        emit_one_pool(&pool, &entry(table, pk, AuditOp::Create, json!({"x": 1})))
            .await
            .unwrap();
    }
    let filter = audit::AuditFilter {
        entity_table: Some("post".into()),
        ..Default::default()
    };
    let rows = audit::list(&pool, &filter, 50, 0).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.entity_table == "post"));
    let total = audit::count(&pool, &filter).await.unwrap();
    assert_eq!(total, 2);
}

#[tokio::test]
async fn facet_counts_returns_groupby_on_mysql() {
    let _serial = serial_lock().lock().await;
    let Some(pool) = mysql_pool().await else {
        return;
    };
    for (table, op) in [
        ("post", AuditOp::Create),
        ("post", AuditOp::Update),
        ("post", AuditOp::Update),
        ("author", AuditOp::Create),
    ] {
        emit_one_pool(&pool, &entry(table, "1", op, json!({})))
            .await
            .unwrap();
    }
    let facets = audit::facet_counts(&pool, "entity_table").await.unwrap();
    assert_eq!(facets[0], ("post".to_string(), 3));
    assert_eq!(facets[1], ("author".to_string(), 1));
}

#[tokio::test]
async fn facet_counts_rejects_non_allowlisted_column_on_mysql() {
    let _serial = serial_lock().lock().await;
    let Some(pool) = mysql_pool().await else {
        return;
    };
    let r = audit::facet_counts(&pool, "no_such_column").await;
    assert!(r.is_err(), "non-allowlisted column should be rejected");
}
