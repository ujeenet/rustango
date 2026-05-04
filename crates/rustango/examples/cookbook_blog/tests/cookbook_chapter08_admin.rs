//! Cookbook Chapter 8 — admin (auto-CRUD) smoke.
//!
//! In-process boot via `tower::ServiceExt::oneshot` against
//! `rustango::admin::Builder` — no socket needed. Verifies the router
//! constructs and a few baseline routes respond.
//!
//! Browser-side interactive testing (login, list/detail/create flow)
//! is queued for cookbook_chapter08_admin_browser.rs which spawns the
//! cookbook_blog binary and uses playwright MCP to navigate.
//!
//! Run: `DATABASE_URL=... cargo test --test cookbook_chapter08_admin -- --test-threads=1`

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use rustango::sql::sqlx;
use tower::ServiceExt;

use cookbook_blog::apps::blog::models::Author; // pulls Author into inventory

fn url() -> Option<String> { std::env::var("DATABASE_URL").ok() }

async fn pool() -> Option<sqlx::PgPool> {
    let url = url()?;
    Some(sqlx::PgPool::connect(&url).await.expect("connect"))
}

async fn fresh_author_table(pool: &sqlx::PgPool) {
    sqlx::query("DROP TABLE IF EXISTS cookbook_author CASCADE")
        .execute(pool).await.unwrap();
    sqlx::query(
        r#"CREATE TABLE cookbook_author (
            id BIGSERIAL PRIMARY KEY,
            name VARCHAR(80) NOT NULL,
            email VARCHAR(200) NOT NULL UNIQUE,
            bio VARCHAR(500) NULL,
            joined_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )"#,
    ).execute(pool).await.unwrap();
}

// §8.100 / §8.101 — Builder constructs an axum::Router that responds
// to GET /<table>/. Simulates a browse (no auth required when admin's
// basic-auth layer is absent — bare Builder here, see §8.109 for the
// full auth wrap).
#[tokio::test]
async fn admin_builder_serves_list_page_for_registered_model() {
    let Some(pool) = pool().await else { return };
    fresh_author_table(&pool).await;

    // Force the model into inventory by referencing its SCHEMA at runtime.
    // The admin walks `inventory::iter` to find every #[derive(Model)].
    use rustango::core::Model as _;
    let _ = Author::SCHEMA.table;

    let router = rustango::admin::Builder::new(pool.clone()).build();

    let req = Request::builder()
        .method(Method::GET)
        .uri("/cookbook_author")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.expect("router responds");
    let status = resp.status();
    let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8_lossy(&body_bytes);
    assert_eq!(
        status, StatusCode::OK,
        "admin list page should be 200; got {status}, body head: {}",
        &body[..body.len().min(400)]
    );
    // List page renders the model's table name.
    assert!(
        body.contains("cookbook_author") || body.to_lowercase().contains("author"),
        "list page should mention `cookbook_author`. body head: {}",
        &body[..body.len().min(400)]
    );
}

// §8.103 — Create form GET /<table>/new responds 200 with form fields.
#[tokio::test]
async fn admin_create_form_renders_input_for_each_writable_field() {
    let Some(pool) = pool().await else { return };
    fresh_author_table(&pool).await;
    use rustango::core::Model as _;
    let _ = Author::SCHEMA.table;

    let router = rustango::admin::Builder::new(pool.clone()).build();
    let req = Request::builder()
        .method(Method::GET)
        .uri("/cookbook_author/new")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.expect("router responds");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&body);

    // Form should expose `name`, `email`, `bio` as editable inputs.
    // `id` (Auto<i64>) and `joined_at` (auto_now_add) are skipped.
    for field in ["name", "email", "bio"] {
        assert!(
            html.contains(&format!("name=\"{field}\"")),
            "create form should have an input for `{field}`. html head:\n{}",
            &html[..html.len().min(800)]
        );
    }
    assert!(
        !html.contains("name=\"id\""),
        "Auto<i64> id should NOT appear in create form (server-assigned)"
    );
}
