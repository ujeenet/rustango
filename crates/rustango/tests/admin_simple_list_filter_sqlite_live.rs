//! Django-parity #351 — custom `SimpleListFilter`.

#![cfg(all(feature = "sqlite", feature = "admin", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::core::{Filter, Op, SqlValue};
use rustango::sql::Pool;
use rustango::Model;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(table = "slf_post")]
#[allow(dead_code)]
pub struct SlfPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
    #[rustango(max_length = 32)]
    status: String,
}

fn status_to_filters(value: &str) -> Vec<Filter> {
    match value {
        "draft" => vec![Filter {
            column: "status",
            op: Op::Eq,
            value: SqlValue::String("draft".into()),
        }],
        "published" => vec![Filter {
            column: "status",
            op: Op::Eq,
            value: SqlValue::String("published".into()),
        }],
        _ => Vec::new(),
    }
}

rustango::register_admin_list_filter!(
    "slf_post",
    "by_status",
    "Status",
    &[("draft", "Drafts"), ("published", "Published")],
    status_to_filters,
);

async fn build_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE IF NOT EXISTS "slf_post" (
            "id"     INTEGER PRIMARY KEY AUTOINCREMENT,
            "title"  TEXT NOT NULL,
            "status" TEXT NOT NULL
        )"#,
        Vec::new(),
    )
    .await
    .expect("create");
    for (title, status) in [
        ("Hidden", "draft"),
        ("Visible", "published"),
        ("Pending", "draft"),
    ] {
        rustango::sql::raw_execute_pool(
            &pool,
            r#"INSERT INTO "slf_post" ("title", "status") VALUES (?, ?)"#,
            vec![
                SqlValue::String(title.into()),
                SqlValue::String(status.into()),
            ],
        )
        .await
        .expect("seed");
    }
    pool
}

fn build_app(pool: Pool) -> axum::Router {
    rustango::admin::Builder::new(pool).admin_prefix("").build()
}

async fn body_of(pool: Pool, uri: &str) -> String {
    let app = build_app(pool);
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "GET {uri} returned non-200");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[test]
fn registry_finds_registered_filter() {
    let names: Vec<&'static str> = rustango::admin::list_filters::for_table("slf_post")
        .map(|f| f.parameter_name)
        .collect();
    assert!(names.contains(&"by_status"));
}

#[tokio::test]
async fn list_view_renders_custom_filter_card() {
    let pool = build_pool().await;
    let body = body_of(pool, "/slf_post").await;
    assert!(
        body.contains(r#"class="facet custom-filter""#),
        "custom-filter card missing: {body}"
    );
    assert!(body.contains("Status"), "title missing: {body}");
    assert!(body.contains("Drafts"), "draft option missing: {body}");
    assert!(
        body.contains("Published"),
        "published option missing: {body}"
    );
}

#[tokio::test]
async fn list_view_filters_rows_via_custom_filter() {
    let pool = build_pool().await;
    let body = body_of(pool, "/slf_post?by_status=draft").await;
    // Both draft rows should appear; published one should not.
    assert!(body.contains("Hidden"), "Hidden row missing: {body}");
    assert!(body.contains("Pending"), "Pending row missing: {body}");
    assert!(
        !body.contains("Visible"),
        "published row should be filtered out: {body}"
    );
}

#[tokio::test]
async fn unknown_filter_value_returns_all_rows() {
    // `by_status=bogus` → `to_filters` returns empty Vec → no narrowing.
    let pool = build_pool().await;
    let body = body_of(pool, "/slf_post?by_status=bogus").await;
    assert!(body.contains("Hidden"));
    assert!(body.contains("Pending"));
    assert!(body.contains("Visible"));
}
