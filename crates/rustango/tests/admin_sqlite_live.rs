//! v0.37 slice 7 — end-to-end admin CRUD against a real SQLite pool.
//!
//! Litmus test for v0.37: every fetch site in `admin::*` runs on the
//! tri-dialect `Pool` enum + JSON bridge. This file boots the
//! `admin::router(Pool::Sqlite(...))` in-memory, performs each CRUD
//! verb, and asserts the rendered HTML reflects the database state.
//!
//! Covered routes:
//!   GET  /                         — index lists registered models
//!   GET  /admin_blog_post          — list view with rows
//!   POST /admin_blog_post          — create
//!   GET  /admin_blog_post/:pk      — detail view + computed FK / audit
//!   GET  /admin_blog_post/:pk/edit — edit form (pre-fill)
//!   POST /admin_blog_post/:pk      — update
//!   POST /admin_blog_post/:pk/delete — delete
//!
//! Requires the `sqlite` + `admin` features (no postgres / tenancy).
//! Run with: `cargo test --no-default-features --features sqlite,admin
//!           --test admin_sqlite_live`.

#![cfg(all(feature = "sqlite", feature = "admin"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use rustango::sql::Pool;
use rustango::Model;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "admin_blog_post",
    display = "title",
    admin(
        list_display = "title, views, is_published",
        search_fields = "title",
        ordering = "-id",
    )
)]
pub struct AdminBlogPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
    views: i64,
    is_published: bool,
}

async fn pool() -> Pool {
    Pool::connect("sqlite::memory:")
        .await
        .expect("sqlite in-memory pool")
}

/// Bootstrap the post table directly so we don't drag the full
/// migrate pipeline into this slim test. SQLite stores bools as
/// integers, hence the `INTEGER NOT NULL` shape (rustango's
/// `is_published: bool` decodes either INTEGER or BOOL columns).
async fn create_table(pool: &Pool) {
    rustango::sql::raw_execute_pool(
        pool,
        r#"CREATE TABLE IF NOT EXISTS "admin_blog_post" (
            "id"            INTEGER PRIMARY KEY AUTOINCREMENT,
            "title"         TEXT NOT NULL,
            "views"         INTEGER NOT NULL,
            "is_published"  INTEGER NOT NULL
        )"#,
        Vec::new(),
    )
    .await
    .expect("create table");
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn index_lists_registered_models_on_sqlite() {
    let pool = pool().await;
    create_table(&pool).await;
    let app = rustango::admin::router(pool);
    let resp = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(
        body.contains("AdminBlogPost"),
        "missing model name in: {body}"
    );
}

#[tokio::test]
async fn empty_list_view_renders_no_rows_marker() {
    let pool = pool().await;
    create_table(&pool).await;
    let app = rustango::admin::router(pool);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin_blog_post")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(body.contains("No rows"), "missing empty marker: {body}");
}

#[tokio::test]
async fn create_then_list_then_detail_round_trips() {
    let pool = pool().await;
    create_table(&pool).await;
    let app = rustango::admin::router(pool.clone());

    // POST /admin_blog_post — create.
    let body = "title=Hello+World&views=42&is_published=on".to_owned();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/admin_blog_post")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::SEE_OTHER || resp.status() == StatusCode::OK,
        "expected redirect after create, got: {}",
        resp.status()
    );

    // GET /admin_blog_post — list shows the row.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin_blog_post")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(
        body.contains("Hello World"),
        "missing title in list: {body}"
    );
    assert!(body.contains("1 row"), "expected exactly 1 row: {body}");

    // GET /admin_blog_post/1 — detail view.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin_blog_post/1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(body.contains("Hello World"), "detail missing title: {body}");
    assert!(body.contains("42"), "detail missing views=42: {body}");
}

#[tokio::test]
async fn edit_form_prefills_existing_row() {
    let pool = pool().await;
    create_table(&pool).await;
    AdminBlogPost {
        id: rustango::Auto::Set(1),
        title: "Original".into(),
        views: 5,
        is_published: false,
    }
    .insert_pool(&pool)
    .await
    .unwrap();

    let app = rustango::admin::router(pool.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin_blog_post/1/edit")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    // `title` renders as a `<textarea>` (maxlength=200 hits the
    // textarea threshold), so the pre-fill is in the body text;
    // `views` renders as `<input value=...>`.
    assert!(
        body.contains(">Original</textarea>"),
        "edit form should pre-fill title: {body}"
    );
    assert!(
        body.contains(r#"value="5""#),
        "edit form should pre-fill views: {body}"
    );
}

#[tokio::test]
async fn update_submit_persists_change() {
    let pool = pool().await;
    create_table(&pool).await;
    AdminBlogPost {
        id: rustango::Auto::Set(1),
        title: "Before".into(),
        views: 0,
        is_published: false,
    }
    .insert_pool(&pool)
    .await
    .unwrap();

    let app = rustango::admin::router(pool.clone());
    let body = "title=After&views=99&is_published=on".to_owned();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/admin_blog_post/1")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::SEE_OTHER || resp.status() == StatusCode::OK,
        "expected redirect after update, got: {}",
        resp.status()
    );

    // Detail should reflect the new state.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin_blog_post/1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_string(resp).await;
    assert!(body.contains("After"), "missing updated title: {body}");
    assert!(body.contains("99"), "missing updated views: {body}");
}

#[tokio::test]
async fn delete_submit_removes_row() {
    let pool = pool().await;
    create_table(&pool).await;
    AdminBlogPost {
        id: rustango::Auto::Set(1),
        title: "Doomed".into(),
        views: 0,
        is_published: false,
    }
    .insert_pool(&pool)
    .await
    .unwrap();

    let app = rustango::admin::router(pool.clone());
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/admin_blog_post/1/delete")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::SEE_OTHER || resp.status() == StatusCode::OK,
        "expected redirect after delete, got: {}",
        resp.status()
    );

    // List should show "no rows".
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin_blog_post")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_string(resp).await;
    assert!(body.contains("No rows"), "row not removed: {body}");
}

#[tokio::test]
async fn list_view_search_filters_results() {
    let pool = pool().await;
    create_table(&pool).await;
    for (id, title) in [(1_i64, "Hello world"), (2, "Goodbye world"), (3, "Other")] {
        AdminBlogPost {
            id: rustango::Auto::Set(id),
            title: title.into(),
            views: 0,
            is_published: false,
        }
        .insert_pool(&pool)
        .await
        .unwrap();
    }

    let app = rustango::admin::router(pool);
    // First verify the list works without search (regression guard
    // — slice 3 should already cover this but easier to debug if a
    // future change breaks it).
    let resp_all = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin_blog_post")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body_all = body_string(resp_all).await;
    assert!(body_all.contains("Hello world"));
    assert!(body_all.contains("Other"));

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin_blog_post?q=world")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = body_string(resp).await;
    assert_eq!(status, StatusCode::OK, "search failed: {body}");
    assert!(body.contains("Hello world"), "missing match: {body}");
    assert!(body.contains("Goodbye world"), "missing match: {body}");
    assert!(
        !body.contains(">Other<"),
        "non-match leaked through: {body}"
    );
}
