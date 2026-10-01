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
    let res = send(pool, "", perms, Request::builder().uri(uri), "").await;
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn send(
    pool: &Pool,
    prefix: &str,
    perms: &[&str],
    req: axum::http::request::Builder,
    body: &'static str,
) -> axum::response::Response {
    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix(prefix)
        .with_user_perms(perms.iter().map(|p| (*p).to_owned()))
        .build();
    app.oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap()
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

/// Facet links and the cleanup redirect carry the admin prefix (#1916).
async fn feed_urls_carry_the_admin_prefix(pool: &Pool) {
    let perms = ["audit.view", "audit.delete", "audscope_seen.view"];
    let res = send(pool, "/adm", &perms, Request::builder().uri("/__audit"), "").await;
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8_lossy(&bytes).into_owned();
    assert!(
        body.contains(r#"href="/adm/__audit?entity_table=audscope_seen""#),
        "{body}"
    );
    let res = send(
        pool,
        "/adm",
        &perms,
        Request::builder()
            .method("POST")
            .uri("/__audit/cleanup")
            .header("content-type", "application/x-www-form-urlencoded"),
        // A century: trims nothing a shared test database still needs.
        "days=36500",
    )
    .await;
    let location = res
        .headers()
        .get("location")
        .map(|v| v.to_str().unwrap().to_owned());
    assert_eq!(
        location.as_deref(),
        Some("/adm/__audit"),
        "{:?}",
        res.status()
    );
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        feed_shows_only_viewable_tables,
        feed_with_no_viewable_table_is_empty,
        feed_urls_carry_the_admin_prefix,
    ],
}
