//! Django-parity #353 — `admin.search_help_text` renders a caption
//! beside the list view's search box. Empty string suppresses it.

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
    table = "sht_post",
    admin(
        list_display = "title",
        search_fields = "title",
        search_help_text = "Search by title only — author lookups are case-sensitive.",
    )
)]
pub struct ShtPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
}

fn build_app(pool: Pool) -> axum::Router {
    rustango::admin::Builder::new(pool).admin_prefix("").build()
}

async fn pool_ready() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE IF NOT EXISTS "sht_post" (
            "id"    INTEGER PRIMARY KEY AUTOINCREMENT,
            "title" TEXT NOT NULL
        )"#,
        Vec::new(),
    )
    .await
    .expect("create");
    pool
}

async fn fetch_body(pool: Pool, uri: &str) -> String {
    let app = build_app(pool);
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[test]
fn schema_threads_search_help_text() {
    let cfg = ShtPost::SCHEMA.admin.expect("admin attr set");
    assert_eq!(
        cfg.search_help_text,
        "Search by title only — author lookups are case-sensitive.",
    );
}

#[tokio::test]
async fn list_view_renders_caption_when_set() {
    let pool = pool_ready().await;
    let body = fetch_body(pool, "/sht_post").await;
    assert!(
        body.contains("Search by title only"),
        "expected caption text in list view, got: {body}",
    );
    assert!(
        body.contains(r#"<small class="search-help">"#),
        "expected caption wrapper class, got: {body}",
    );
}
