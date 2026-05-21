//! End-to-end live test for `ViewSet::cursor_pagination("id")` on
//! SQLite (Django-parity #440). Walks three pages of a 12-row list,
//! following the `next` cursor each step, and asserts the
//! cursor-aware WHERE clause picks up where the previous page left
//! off and stops returning `next` on the last partial page.
//!
//! The primitive (`pagination::CursorPaginator`) has unit coverage,
//! but the ViewSet integration (`handle_list_cursor`) lacked an HTTP
//! end-to-end before this batch — that gap was the only material
//! delta the audit flagged, the API itself already shipped.

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "serializer"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::core::Model as _;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;
use serde_json::Value;
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone, serde::Serialize, serde::Deserialize)]
#[rustango(table = "vs_cursor_post")]
#[rustango(app = "vs_cursor_app")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

async fn make_router_and_pool(page_size: usize) -> (axum::Router, Pool) {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE vs_cursor_post (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            title TEXT NOT NULL)",
    )
    .execute(&sq)
    .await
    .expect("create");
    let pool = Pool::Sqlite(sq);
    let router = rustango::viewset::ViewSet::for_model(Post::SCHEMA)
        .page_size(page_size)
        .cursor_pagination("id")
        .router_pool("/posts", pool.clone());
    (router, pool)
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

async fn seed(app: &axum::Router, n: usize) {
    for i in 1..=n {
        let body = format!(r#"{{"title":"post-{i}"}}"#);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/posts")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(resp.status().is_success(), "seed {i} failed");
    }
}

#[tokio::test]
async fn cursor_pagination_walks_pages_ascending() {
    let (app, _pool) = make_router_and_pool(5).await;
    seed(&app, 12).await;

    // Page 1: should return ids 1..=5, cursor pointing at 5.
    let resp = app.clone().oneshot(get("/posts")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 5);
    assert_eq!(results[0]["id"], 1);
    assert_eq!(results[4]["id"], 5);
    let next1 = body["next"].as_str().expect("next cursor on page 1");
    assert!(!next1.is_empty());

    // Page 2: ids 6..=10, cursor pointing at 10.
    let resp = app
        .clone()
        .oneshot(get(&format!("/posts?cursor={next1}")))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let results = body["results"].as_array().expect("results");
    assert_eq!(results.len(), 5);
    assert_eq!(results[0]["id"], 6);
    assert_eq!(results[4]["id"], 10);
    let next2 = body["next"].as_str().expect("next cursor on page 2");

    // Page 3 (partial): ids 11..=12, no next cursor.
    let resp = app
        .clone()
        .oneshot(get(&format!("/posts?cursor={next2}")))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let results = body["results"].as_array().expect("results");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["id"], 11);
    assert_eq!(results[1]["id"], 12);
    assert!(
        body["next"].is_null(),
        "last page should have null next, got {body}"
    );
}

#[tokio::test]
async fn cursor_pagination_rejects_malformed_cursor() {
    let (app, _pool) = make_router_and_pool(5).await;
    seed(&app, 3).await;

    let resp = app
        .clone()
        .oneshot(get("/posts?cursor=not-a-real-cursor%21"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn cursor_pagination_descending_walks_back_from_highest_id() {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE vs_cursor_post (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            title TEXT NOT NULL)",
    )
    .execute(&sq)
    .await
    .expect("create");
    let pool = Pool::Sqlite(sq);
    let app = rustango::viewset::ViewSet::for_model(Post::SCHEMA)
        .page_size(3)
        .cursor_pagination_desc("id")
        .router_pool("/posts", pool);

    seed(&app, 7).await;

    // Page 1 desc: ids 7, 6, 5 — cursor at 5.
    let resp = app.clone().oneshot(get("/posts")).await.unwrap();
    let body = body_json(resp).await;
    let results = body["results"].as_array().expect("results");
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["id"], 7);
    assert_eq!(results[2]["id"], 5);
    let next1 = body["next"].as_str().expect("next");

    // Page 2 desc: ids 4, 3, 2 — cursor at 2.
    let resp = app
        .clone()
        .oneshot(get(&format!("/posts?cursor={next1}")))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let results = body["results"].as_array().expect("results");
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["id"], 4);
    assert_eq!(results[2]["id"], 2);
    let next2 = body["next"].as_str().expect("next");

    // Page 3 desc: just id 1 (partial), no next.
    let resp = app
        .clone()
        .oneshot(get(&format!("/posts?cursor={next2}")))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let results = body["results"].as_array().expect("results");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["id"], 1);
    assert!(body["next"].is_null());
}
