//! The admin audit feed's per-user table scope renders valid SQL on
//! every backend (#1858), and a model table named `audit` can't open it (#1979).

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "admin"
))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::audit::{self, AuditLog, AuditOp, AuditSource, PendingEntry};
use rustango::sql::{FetcherPool as _, Pool};
use rustango::{tri_dialect_test, Model};
use tower::ServiceExt as _;

/// Its `{table}.view` is `audit.view`, the feed's pre-#1979 codename.
#[derive(Model, Debug)]
#[rustango(table = "audit")]
#[allow(dead_code)]
pub struct Audit {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
}

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
    let perms = [audit::VIEW_CODENAME, "audscope_seen.view"];
    let (status, body) = get(pool, &perms, "/__audit").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("mark-audscope_seen"), "{body}");
    assert!(!body.contains("audscope_hidden"), "{body}");

    let (status, body) = get(pool, &perms, "/__audit?entity_table=audscope_hidden").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("mark-audscope_hidden"), "{body}");
}

/// A user holding the feed codename but no table perms sees an empty feed.
async fn feed_with_no_viewable_table_is_empty(pool: &Pool) {
    let (status, body) = get(pool, &[audit::VIEW_CODENAME], "/__audit").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("mark-audscope"), "{body}");
}

/// View and delete on the `audit` model grant neither the feed nor cleanup.
async fn audit_model_perms_do_not_open_the_feed(pool: &Pool) {
    let perms = ["audit.view", "audit.delete", "audscope_seen.view"];
    let (status, body) = get(pool, &perms, "/__audit").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(!body.contains("mark-audscope_seen"), "{body}");

    let status = post(pool, &perms, "/__audit/cleanup", "mode=keep_last&keep=0").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "cleanup refused");
    assert_eq!(seen_rows(pool).await.len(), 1, "the trail must survive");
}

/// The feed codenames open the feed only, not the `AuditLog` model admin.
async fn feed_perms_do_not_open_the_audit_model(pool: &Pool) {
    let perms = [
        audit::VIEW_CODENAME,
        audit::DELETE_CODENAME,
        "audscope_seen.view",
    ];
    let (status, body) = get(pool, &perms, "/__audit").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = get(pool, &perms, "/rustango_audit_log").await;
    assert!(status.is_client_error(), "{status}: {body}");
    assert!(!body.contains("audscope_seen"), "{body}");

    let row = seen_rows(pool).await.remove(0);
    let uri = format!("/rustango_audit_log/{}/delete", row.id.get().unwrap());
    let status = post(pool, &perms, &uri, "").await;
    assert!(status.is_client_error(), "{status}");
    assert_eq!(seen_rows(pool).await.len(), 1, "the row must survive");
}

async fn post(pool: &Pool, perms: &[&str], uri: &str, body: &str) -> StatusCode {
    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .with_user_perms(perms.iter().map(|p| (*p).to_owned()))
        .build();
    app.oneshot(
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(body.to_owned()))
            .unwrap(),
    )
    .await
    .unwrap()
    .status()
}

async fn seen_rows(pool: &Pool) -> Vec<AuditLog> {
    AuditLog::objects()
        .filter("entity_table", SEEN)
        .fetch(pool)
        .await
        .expect("seen rows")
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        feed_shows_only_viewable_tables,
        feed_with_no_viewable_table_is_empty,
        audit_model_perms_do_not_open_the_feed,
        feed_perms_do_not_open_the_audit_model,
    ],
}
