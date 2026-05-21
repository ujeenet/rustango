//! Django-parity #355 — admin `date_hierarchy`.
//!
//! Verifies the macro-emitted `date_hierarchy` attr, URL drill-down,
//! and the GROUP BY bucket enumeration on SQLite.

#![cfg(all(feature = "sqlite", feature = "admin", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::core::Model as _;
use rustango::sql::Pool;
use rustango::Model;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "dh_post",
    admin(list_display = "title,published_at", date_hierarchy = "published_at")
)]
#[allow(dead_code)]
pub struct DhPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
    published_at: chrono::DateTime<chrono::Utc>,
}

async fn build_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    let ddl = r#"CREATE TABLE IF NOT EXISTS "dh_post" (
        "id"           INTEGER PRIMARY KEY AUTOINCREMENT,
        "title"        TEXT NOT NULL,
        "published_at" TEXT NOT NULL
    )"#;
    rustango::sql::raw_execute_pool(&pool, ddl, Vec::new())
        .await
        .expect("create");
    // Seed three rows across two distinct years + months.
    for (title, ts) in [
        ("Old", "2024-01-15T12:00:00Z"),
        ("Spring", "2025-03-10T09:00:00Z"),
        ("Fall", "2025-11-15T18:00:00Z"),
    ] {
        rustango::sql::raw_execute_pool(
            &pool,
            r#"INSERT INTO "dh_post" ("title", "published_at") VALUES (?, ?)"#,
            vec![
                rustango::core::SqlValue::String(title.into()),
                rustango::core::SqlValue::String(ts.into()),
            ],
        )
        .await
        .expect("seed insert");
    }
    pool
}

fn build_app(pool: Pool) -> axum::Router {
    rustango::admin::Builder::new(pool).admin_prefix("").build()
}

async fn fetch_body(pool: Pool, uri: &str) -> String {
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
fn schema_records_date_hierarchy() {
    let cfg = DhPost::SCHEMA.admin.expect("admin attr set");
    assert_eq!(cfg.date_hierarchy, "published_at");
}

#[tokio::test]
async fn root_strip_lists_year_buckets() {
    let pool = build_pool().await;
    let body = fetch_body(pool, "/dh_post").await;
    assert!(body.contains("date-hierarchy"), "strip missing: {body}");
    // Year buckets should both appear (2024 + 2025).
    assert!(
        body.contains(">2024 <small>(1)"),
        "2024 bucket missing: {body}"
    );
    assert!(
        body.contains(">2025 <small>(2)"),
        "2025 bucket missing: {body}"
    );
}

#[tokio::test]
async fn year_drill_filters_rows_and_lists_month_buckets() {
    let pool = build_pool().await;
    let body = fetch_body(pool, "/dh_post?year=2025").await;
    // 2024 row hidden, 2025 rows visible.
    assert!(!body.contains("Old"), "2024 row should be filtered out");
    assert!(body.contains("Spring") && body.contains("Fall"));
    // Month buckets within 2025.
    assert!(body.contains("March"), "March bucket missing: {body}");
    assert!(body.contains("November"), "November bucket missing: {body}");
}

#[tokio::test]
async fn month_drill_narrows_to_single_day() {
    let pool = build_pool().await;
    let body = fetch_body(pool, "/dh_post?year=2025&month=11").await;
    assert!(!body.contains("Spring"));
    assert!(body.contains("Fall"));
    // Day bucket "15" with count 1.
    assert!(
        body.contains(">15 <small>(1)"),
        "day bucket missing: {body}"
    );
}

#[tokio::test]
async fn day_drill_shows_only_matching_row() {
    let pool = build_pool().await;
    let body = fetch_body(pool, "/dh_post?year=2025&month=11&day=15").await;
    assert!(body.contains("Fall"));
    assert!(!body.contains("Spring"));
}
