//! Live test for admin inlines — Django `TabularInline` /
//! `StackedInline` read-only display on the parent detail page.
//! Issue #50 slice 1.
//!
//! Spins up two models (`il_blog` + `il_blog_post`), registers an
//! inline pointing the child at the parent, inserts one parent row +
//! two child rows, GETs `/__admin/il_blog/<pk>`, and asserts the
//! tabular panel renders both children.

#![cfg(feature = "postgres")]

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use rustango::admin::inlines::InlineKind;
use rustango::register_admin_inline;
use rustango::sql::sqlx;
use rustango::sql::Auto;
use rustango::Model;
use tower::ServiceExt;

use tokio::sync::Mutex;

/// Suite-wide lock — every test in this file mutates the shared
/// schema and runs alongside other admin-live tests under cargo's
/// parallel harness.
fn live_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

#[derive(Model, Debug)]
#[rustango(table = "il_blog")]
#[allow(dead_code)]
pub struct Blog {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 100)]
    pub name: String,
}

#[derive(Model, Debug)]
#[rustango(table = "il_blog_post")]
#[allow(dead_code)]
pub struct BlogPost {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "il_blog", on = "id")]
    pub blog_id: i64,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 500)]
    pub body: String,
}

register_admin_inline!(
    parent = "il_blog",
    child = "il_blog_post",
    fk = "blog_id",
    kind = InlineKind::Tabular,
    label = "Posts",
    fields = &["title", "body"],
);

async fn fresh(pool: &sqlx::PgPool) {
    // Build only the tables this test cares about. The shared
    // migrate::drop_all + apply_all path is too slow to run in every
    // admin-live test.
    for t in ["il_blog_post", "il_blog"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}" CASCADE"#))
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query(
        r#"CREATE TABLE "il_blog" (
               id BIGSERIAL PRIMARY KEY,
               name VARCHAR(100) NOT NULL
           )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"CREATE TABLE "il_blog_post" (
               id BIGSERIAL PRIMARY KEY,
               blog_id BIGINT NOT NULL REFERENCES "il_blog"(id),
               title VARCHAR(200) NOT NULL,
               body VARCHAR(500) NOT NULL
           )"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn parent_detail_renders_inline_panel_with_child_rows() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    let mut blog = Blog {
        id: Auto::default(),
        name: "Test Blog".into(),
    };
    blog.insert(&pool).await.unwrap();
    let blog_id = *blog.id.get().expect("PK assigned");

    let mut post_a = BlogPost {
        id: Auto::default(),
        blog_id,
        title: "First post".into(),
        body: "hello world".into(),
    };
    post_a.insert(&pool).await.unwrap();

    let mut post_b = BlogPost {
        id: Auto::default(),
        blog_id,
        title: "Second post".into(),
        body: "more content".into(),
    };
    post_b.insert(&pool).await.unwrap();

    let app = rustango::admin::router(pool.clone());
    let req = Request::builder()
        .uri(format!("/il_blog/{blog_id}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let body = to_bytes(res.into_body(), 1_000_000).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();

    // Panel header rendered with the inline's `label`.
    assert!(
        html.contains("Posts"),
        "inline panel header missing: {html}"
    );
    // Tabular kind — `<table class=\"inline-table\">` is the marker.
    assert!(
        html.contains("class=\"inline-table\""),
        "tabular variant didn't render its <table>: {html}"
    );
    // Both child rows visible.
    assert!(html.contains("First post"), "first child missing: {html}");
    assert!(html.contains("Second post"), "second child missing: {html}");
    // Edit-link target is the child admin route.
    assert!(
        html.contains("/il_blog_post/"),
        "row link should point at child admin route: {html}"
    );
    // No "No related rows." sentinel.
    assert!(
        !html.contains("No related rows"),
        "panel shouldn't show empty-state when children exist: {html}"
    );
}

#[tokio::test]
async fn parent_detail_renders_empty_state_when_no_children() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    let mut blog = Blog {
        id: Auto::default(),
        name: "Lonely Blog".into(),
    };
    blog.insert(&pool).await.unwrap();
    let blog_id = *blog.id.get().expect("PK assigned");

    let app = rustango::admin::router(pool.clone());
    let req = Request::builder()
        .uri(format!("/il_blog/{blog_id}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let body = to_bytes(res.into_body(), 1_000_000).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();

    // Panel still renders — but with the empty-state message.
    assert!(
        html.contains("Posts"),
        "inline panel header should still render: {html}"
    );
    assert!(
        html.contains("No related rows"),
        "panel should show empty-state when no children: {html}"
    );
}
