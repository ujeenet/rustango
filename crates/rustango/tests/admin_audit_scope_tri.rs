//! The admin audit feed's per-user table scope renders valid SQL on
//! every backend (#1858), and a model table named `audit` can't open it (#1979).
//! Huge `?page=` values are empty pages (#1865).

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
/// Tables whose rows a hook hides (#2342).
const QS_HOOKED: &str = "audscope_qshooked";
const VIEW_HOOKED: &str = "audscope_viewhooked";

fn hide_all_rows(_: &axum::http::request::Parts) -> Vec<rustango::core::Filter> {
    vec![rustango::core::Filter::new(
        "id",
        rustango::core::Op::Eq,
        rustango::core::SqlValue::I64(-1),
    )]
}
rustango::register_admin_queryset!(QS_HOOKED, hide_all_rows);

fn deny_view(_: &axum::http::request::Parts, _: Option<&serde_json::Value>) -> bool {
    false
}
rustango::register_admin_object_permission!(VIEW_HOOKED, "view", deny_view);

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Audit>(pool).await;
    audit::ensure_table_pool(pool).await.expect("audit table");
    for t in [SEEN, HIDDEN, QS_HOOKED, VIEW_HOOKED] {
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
    let perms = [audit::VIEW_CODENAME, "audscope_seen.view"];
    let (status, body) = get(pool, &perms, "/__audit").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("mark-audscope_seen"), "{body}");
    assert!(!body.contains("audscope_hidden"), "{body}");

    let (status, body) = get(pool, &perms, "/__audit?entity_table=audscope_hidden").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("mark-audscope_hidden"), "{body}");
}

/// A table whose rows a hook hides keeps its snapshots out of a
/// non-superuser's feed and home page; a superuser still sees them (#2342).
async fn feed_hides_hook_scoped_tables(pool: &Pool) {
    let perms = [
        audit::VIEW_CODENAME,
        "audscope_seen.view",
        "audscope_qshooked.view",
        "audscope_viewhooked.view",
    ];
    for uri in ["/__audit", "/__audit?entity_table=audscope_qshooked", "/"] {
        let (status, body) = get(pool, &perms, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert!(!body.contains("mark-audscope_qshooked"), "{uri}: {body}");
        assert!(!body.contains("mark-audscope_viewhooked"), "{uri}: {body}");
        assert!(!body.contains("audscope_qshooked/1"), "{uri}: {body}");
        assert!(!body.contains("audscope_viewhooked/1"), "{uri}: {body}");
    }
    let (_, body) = get(pool, &perms, "/").await;
    assert!(
        body.contains("audscope_seen/1"),
        "home feed control: {body}"
    );
    let (_, body) = get(pool, &perms, "/__audit").await;
    assert!(body.contains("mark-audscope_seen"), "{body}");

    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .build();
    let res = app
        .oneshot(
            Request::builder()
                .uri("/__audit")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8_lossy(&bytes);
    assert!(body.contains("mark-audscope_qshooked"), "{body}");
    assert!(body.contains("mark-audscope_viewhooked"), "{body}");
}

/// A user holding the feed codename but no table perms sees an empty feed.
async fn feed_with_no_viewable_table_is_empty(pool: &Pool) {
    let (status, body) = get(pool, &[audit::VIEW_CODENAME], "/__audit").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("mark-audscope"), "{body}");
}

/// Facet links and the cleanup redirect carry the admin prefix (#1916).
async fn feed_urls_carry_the_admin_prefix(pool: &Pool) {
    let perms = [
        audit::VIEW_CODENAME,
        audit::DELETE_CODENAME,
        "audscope_seen.view",
    ];
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

/// `?page=i64::MAX` is an empty page in the feed and a model list, not an overflow (#1865).
async fn huge_page_is_an_empty_page(pool: &Pool) {
    let perms = [audit::VIEW_CODENAME, "audscope_seen.view", "audit.view"];
    let page = format!("page={}", i64::MAX);
    let (status, body) = get(pool, &perms, &format!("/__audit?{page}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("mark-audscope_seen"), "{body}");
    let (status, body) = get(pool, &perms, &format!("/audit?{page}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        feed_shows_only_viewable_tables,
        feed_hides_hook_scoped_tables,
        feed_with_no_viewable_table_is_empty,
        feed_urls_carry_the_admin_prefix,
        audit_model_perms_do_not_open_the_feed,
        feed_perms_do_not_open_the_audit_model,
        huge_page_is_an_empty_page,
    ],
}
