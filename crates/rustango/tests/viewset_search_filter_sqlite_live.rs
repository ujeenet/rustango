//! End-to-end live test for `ViewSet::search_fields(...)` on SQLite
//! (Django-parity #438 — DRF `SearchFilter`).
//!
//! The DSL (`ViewSet::search_fields`) + the IR (`SearchClause`) +
//! per-dialect writer (`Dialect::write_search` overrides for PG /
//! MySQL / SQLite) + OpenAPI doc emission have shipped since v0.30+.
//! The only material delta the audit flagged: no live test walked the
//! HTTP end-to-end on SQLite. This PR adds that.

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "serializer"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::core::Model as _;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;
use serde_json::Value;
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone, serde::Serialize, serde::Deserialize)]
#[rustango(table = "vs_search_post")]
#[rustango(app = "vs_search_app")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 500)]
    pub body: String,
}

async fn make_router_searching_title() -> axum::Router {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE vs_search_post (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            title TEXT NOT NULL, \
            body TEXT NOT NULL)",
    )
    .execute(&sq)
    .await
    .expect("create");
    let pool = Pool::Sqlite(sq);
    rustango::viewset::ViewSet::for_model(Post::SCHEMA)
        .page_size(50)
        .search_fields(&["title"])
        .router_pool("/posts", pool)
}

async fn make_router_searching_title_and_body() -> axum::Router {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE vs_search_post (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            title TEXT NOT NULL, \
            body TEXT NOT NULL)",
    )
    .execute(&sq)
    .await
    .expect("create");
    let pool = Pool::Sqlite(sq);
    rustango::viewset::ViewSet::for_model(Post::SCHEMA)
        .page_size(50)
        .search_fields(&["title", "body"])
        .router_pool("/posts", pool)
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("body bytes");
    serde_json::from_slice(&bytes).expect("json")
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

async fn post_row(app: &axum::Router, title: &str, body: &str) {
    let payload = serde_json::json!({ "title": title, "body": body }).to_string();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/posts")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(payload))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(resp.status().is_success(), "POST {title:?} failed");
}

#[tokio::test]
async fn search_filter_matches_substring_on_one_column() {
    let app = make_router_searching_title().await;
    post_row(&app, "Rust is fast", "irrelevant").await;
    post_row(&app, "Python is fun", "rust mention in body only").await;
    post_row(&app, "Go is bold", "no overlap").await;

    // ?search=rust should only hit the first row — body matches are
    // ignored because `search_fields = ["title"]`.
    let resp = app
        .clone()
        .oneshot(get("/posts?search=rust"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let results = body["results"].as_array().expect("results");
    assert_eq!(results.len(), 1, "expected 1 hit, got: {body}");
    assert!(results[0]["title"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("rust"));
}

#[tokio::test]
async fn search_filter_ors_across_multiple_columns() {
    let app = make_router_searching_title_and_body().await;
    post_row(&app, "Rust intro", "general programming").await;
    post_row(&app, "Python tutorial", "mentions rust in passing").await;
    post_row(&app, "Go basics", "totally unrelated").await;

    // search=rust should hit BOTH rows — title match + body match.
    let resp = app
        .clone()
        .oneshot(get("/posts?search=rust"))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let results = body["results"].as_array().expect("results");
    assert_eq!(
        results.len(),
        2,
        "expected 2 hits (title OR body), got: {body}"
    );
}

#[tokio::test]
async fn search_filter_is_case_insensitive() {
    let app = make_router_searching_title().await;
    post_row(&app, "Rust Programming", "x").await;
    post_row(&app, "PYTHON BASICS", "y").await;

    // Lowercase query, mixed-case data → both should match
    // case-insensitively (ILIKE on PG, LIKE on SQLite normalized).
    let resp = app
        .clone()
        .oneshot(get("/posts?search=python"))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let results = body["results"].as_array().expect("results");
    assert_eq!(
        results.len(),
        1,
        "case-insensitive PYTHON match failed: {body}"
    );

    let resp = app
        .clone()
        .oneshot(get("/posts?search=RUST"))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let results = body["results"].as_array().expect("results");
    assert_eq!(
        results.len(),
        1,
        "case-insensitive RUST match failed: {body}"
    );
}

#[tokio::test]
async fn empty_or_absent_search_returns_all_rows() {
    let app = make_router_searching_title().await;
    post_row(&app, "alpha", "x").await;
    post_row(&app, "beta", "y").await;
    post_row(&app, "gamma", "z").await;

    // No `?search=` at all
    let resp = app.clone().oneshot(get("/posts")).await.unwrap();
    let body = body_json(resp).await;
    let results = body["results"].as_array().expect("results");
    assert_eq!(results.len(), 3);

    // Empty `?search=` — same as no search at all (filtered out by the
    // `filter(|s| !s.is_empty())` chain in the ViewSet's param decode).
    let resp = app.clone().oneshot(get("/posts?search=")).await.unwrap();
    let body = body_json(resp).await;
    let results = body["results"].as_array().expect("results");
    assert_eq!(results.len(), 3);
}
