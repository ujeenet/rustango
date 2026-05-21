//! Django-parity #354 — `admin.actions_on_top` / `actions_on_bottom`.
//! Position knobs for the action-bar on the list view.

#![cfg(all(feature = "sqlite", feature = "admin", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use rustango::core::Model as _;
use rustango::sql::Pool;
use rustango::Model;
use tower::ServiceExt;

// Default model: actions_on_top=true (Django default), actions_on_bottom=false.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "ap_top_post",
    admin(list_display = "title", actions = "delete_selected")
)]
pub struct ApTopPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
}

// Model with both bars enabled.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "ap_both_post",
    admin(
        list_display = "title",
        actions = "delete_selected",
        actions_on_top = true,
        actions_on_bottom = true,
    )
)]
pub struct ApBothPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
}

// Model with only the bottom bar (top suppressed).
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "ap_bottom_post",
    admin(
        list_display = "title",
        actions = "delete_selected",
        actions_on_top = false,
        actions_on_bottom = true,
    )
)]
pub struct ApBottomPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
}

fn build_app(pool: Pool) -> axum::Router {
    rustango::admin::Builder::new(pool).admin_prefix("").build()
}

async fn seeded_pool(table: &str) -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    let ddl = format!(
        r#"CREATE TABLE IF NOT EXISTS "{table}" (
            "id"    INTEGER PRIMARY KEY AUTOINCREMENT,
            "title" TEXT NOT NULL
        )"#,
    );
    rustango::sql::raw_execute_pool(&pool, &ddl, Vec::new())
        .await
        .expect("create");
    // Seed one row so the action-bar branch renders (the template
    // skips action-bar when `rows | length == 0`).
    let app = build_app(pool.clone());
    let body = "title=Hello";
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/{table}"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::SEE_OTHER || resp.status() == StatusCode::OK,
        "seed POST failed: {}",
        resp.status()
    );
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
fn defaults_match_django_top_true_bottom_false() {
    let cfg = ApTopPost::SCHEMA.admin.expect("admin attr set");
    assert!(cfg.actions_on_top);
    assert!(!cfg.actions_on_bottom);
}

#[tokio::test]
async fn default_renders_top_bar_only() {
    let pool = seeded_pool("ap_top_post").await;
    let body = fetch_body(pool, "/ap_top_post").await;
    assert!(
        body.contains("action-bar-top"),
        "expected top bar, got: {body}"
    );
    assert!(
        !body.contains("action-bar-bottom"),
        "default model should not render bottom bar"
    );
}

#[tokio::test]
async fn both_flags_render_both_bars() {
    let pool = seeded_pool("ap_both_post").await;
    let body = fetch_body(pool, "/ap_both_post").await;
    assert!(body.contains("action-bar-top"));
    assert!(body.contains("action-bar-bottom"));
}

#[tokio::test]
async fn only_bottom_renders_only_bottom_bar() {
    let pool = seeded_pool("ap_bottom_post").await;
    let body = fetch_body(pool, "/ap_bottom_post").await;
    assert!(
        !body.contains("action-bar-top"),
        "top bar should be suppressed when actions_on_top = false"
    );
    assert!(
        body.contains("action-bar-bottom"),
        "expected bottom bar, got: {body}"
    );
}
