//! The admin audit feed's per-user table scope renders valid SQL on
//! every backend (#1858).

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "admin"
))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::audit::{self, AuditLog, AuditOp, AuditSource, PendingEntry};
use rustango::sql::Pool;
use rustango::tri_dialect_test;
use tower::ServiceExt as _;

const SEEN: &str = "audscope_seen";
const HIDDEN: &str = "audscope_hidden";

async fn setup(pool: &Pool) {
    audit::ensure_table_pool(pool).await.expect("audit table");
    for t in [SEEN, HIDDEN] {
        AuditLog::delete_where("entity_table", t, pool)
            .await
            .expect("clear audit rows");
        audit::emit_one_pool(
            pool,
            &PendingEntry {
                entity_table: t,
                entity_pk: "1".into(),
                operation: AuditOp::Update,
                source: AuditSource::System,
                changes: serde_json::json!({ "marker": format!("mark-{t}") }),
            },
        )
        .await
        .expect("emit");
    }
}

async fn get(pool: &Pool, perms: &[&str], uri: &str) -> (StatusCode, String) {
    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .with_user_perms(perms.iter().map(|p| (*p).to_owned()))
        .build();
    let res = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// Rows, count and facets all honour the scope, with and without a filter.
async fn feed_shows_only_viewable_tables(pool: &Pool) {
    let perms = ["audit.view", "audscope_seen.view"];
    let (status, body) = get(pool, &perms, "/__audit").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("mark-audscope_seen"), "{body}");
    assert!(!body.contains("audscope_hidden"), "{body}");

    let (status, body) = get(pool, &perms, "/__audit?entity_table=audscope_hidden").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("mark-audscope_hidden"), "{body}");
}

/// A user holding `audit.view` but no table perms sees an empty feed.
async fn feed_with_no_viewable_table_is_empty(pool: &Pool) {
    let (status, body) = get(pool, &["audit.view"], "/__audit").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("mark-audscope"), "{body}");
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        feed_shows_only_viewable_tables,
        feed_with_no_viewable_table_is_empty,
    ],
}
