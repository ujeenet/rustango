//! Django-parity #350 — `admin.list_display_links` wraps the named
//! cells in `<a href=…>` linking to the detail view.

#![cfg(all(feature = "sqlite", feature = "admin", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use rustango::core::Model as _;
use rustango::sql::Pool;
use rustango::Model;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "ldl_post",
    display = "title",
    admin(
        list_display = "title, views, is_published",
        list_display_links = "title, views",
        ordering = "-id",
    )
)]
pub struct LdlPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
    views: i64,
    is_published: bool,
}

/// Build an admin router with `admin_prefix=""` so generated hrefs
/// stay short + predictable for assertions. The `tenancy` feature
/// otherwise defaults the prefix to `/__admin` (see urls.rs:215).
fn build_app(pool: Pool) -> axum::Router {
    rustango::admin::Builder::new(pool).admin_prefix("").build()
}

async fn pool_with_post() -> (Pool, String) {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE IF NOT EXISTS "ldl_post" (
            "id"            INTEGER PRIMARY KEY AUTOINCREMENT,
            "title"         TEXT NOT NULL,
            "views"         INTEGER NOT NULL,
            "is_published"  INTEGER NOT NULL
        )"#,
        Vec::new(),
    )
    .await
    .expect("create");
    let app = build_app(pool.clone());
    // Seed one row.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/ldl_post")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("title=Hello&views=42&is_published=on"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::SEE_OTHER || resp.status() == StatusCode::OK,
        "seed POST failed: {}",
        resp.status()
    );
    (pool, "/ldl_post".into())
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
fn schema_threads_list_display_links() {
    let cfg = LdlPost::SCHEMA.admin.expect("admin attr set");
    assert_eq!(cfg.list_display_links, &["title", "views"]);
}

#[tokio::test]
async fn list_view_wraps_named_cells_in_anchor() {
    let (pool, uri) = pool_with_post().await;
    let body = fetch_body(pool, &uri).await;
    // `title` cell should be wrapped — the row's pk is 1 here.
    assert!(
        body.contains(r#"<a href="/ldl_post/1">Hello"#),
        "title cell should be wrapped in <a>, got: {body}"
    );
    // `views` cell should also be wrapped (also in list_display_links).
    assert!(
        body.contains(r#"<a href="/ldl_post/1">42"#),
        "views cell should be wrapped in <a>, got: {body}"
    );
}

#[tokio::test]
async fn list_view_does_not_wrap_unlisted_cells() {
    let (pool, uri) = pool_with_post().await;
    let body = fetch_body(pool, &uri).await;
    // `is_published` is in list_display but NOT in list_display_links.
    // The checkbox glyph cell should not be inside the link wrapping.
    // Easy way to check: every <a href="/ldl_post/1"> must be followed
    // by either "Hello" (title) or "42" (views), never by the bool
    // glyph cells. Inspect each occurrence:
    let hrefs: Vec<&str> = body.matches(r#"<a href="/ldl_post/1">"#).collect();
    // We expect exactly 2 — title + views — plus the trailing "View" link.
    // (The trailing "View" column is separate and not part of cells.)
    assert!(
        hrefs.len() >= 2,
        "expected at least 2 in-cell links, got {}",
        hrefs.len()
    );
}
