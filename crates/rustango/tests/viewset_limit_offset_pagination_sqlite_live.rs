//! End-to-end live test for `ViewSet::limit_offset_pagination()` on
//! SQLite (Django-parity #1010). DRF-shape `?limit=&offset=` windowing:
//! asserts the window walks by offset, echoes `count`/`limit`/`offset`,
//! defaults sensibly, and clamps hostile bounds.

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "serializer"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::core::Model as _;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;
use serde_json::Value;
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone, serde::Serialize, serde::Deserialize)]
#[rustango(table = "vs_lo_post")]
#[rustango(app = "vs_lo_app")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

async fn make_router_and_pool(page_size: usize) -> axum::Router {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE vs_lo_post (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            title TEXT NOT NULL)",
    )
    .execute(&sq)
    .await
    .expect("create");
    let pool = Pool::Sqlite(sq);
    rustango::viewset::ViewSet::for_model(Post::SCHEMA)
        .page_size(page_size)
        .limit_offset_pagination()
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

fn ids(body: &Value) -> Vec<i64> {
    body["results"]
        .as_array()
        .expect("results array")
        .iter()
        .map(|r| r["id"].as_i64().expect("id"))
        .collect()
}

#[tokio::test]
async fn limit_offset_windows_rows_by_offset() {
    let app = make_router_and_pool(5).await;
    seed(&app, 12).await;

    // limit=5, offset=0 → ids 1..=5, total count 12.
    let body = body_json(app.clone().oneshot(get("/posts?limit=5")).await.unwrap()).await;
    assert_eq!(body["count"], 12);
    assert_eq!(body["limit"], 5);
    assert_eq!(body["offset"], 0);
    assert_eq!(ids(&body), vec![1, 2, 3, 4, 5]);

    // limit=5, offset=5 → ids 6..=10.
    let body = body_json(
        app.clone()
            .oneshot(get("/posts?limit=5&offset=5"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(body["count"], 12);
    assert_eq!(body["offset"], 5);
    assert_eq!(ids(&body), vec![6, 7, 8, 9, 10]);

    // limit=5, offset=10 → partial tail ids 11..=12.
    let body = body_json(
        app.clone()
            .oneshot(get("/posts?limit=5&offset=10"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(body["count"], 12);
    assert_eq!(ids(&body), vec![11, 12]);
}

#[tokio::test]
async fn limit_offset_defaults_to_page_size_and_zero_offset() {
    let app = make_router_and_pool(3).await;
    seed(&app, 5).await;

    // No params → limit defaults to the configured page_size (3), offset 0.
    let resp = app.clone().oneshot(get("/posts")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["count"], 5);
    assert_eq!(body["limit"], 3);
    assert_eq!(body["offset"], 0);
    assert_eq!(ids(&body), vec![1, 2, 3]);
}

#[tokio::test]
async fn limit_offset_clamps_hostile_bounds() {
    let app = make_router_and_pool(10).await;
    seed(&app, 4).await;

    // A hostile `limit` clamps to the ViewSet's ceiling — `max_page_size`,
    // which defaults to 100 (#1196). It used to be a hard-coded 1000 that no
    // app could lower. `offset` below 0 clamps to 0.
    let body = body_json(
        app.clone()
            .oneshot(get("/posts?limit=999999&offset=-5"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(body["limit"], 100);
    assert_eq!(body["offset"], 0);
    assert_eq!(ids(&body), vec![1, 2, 3, 4]);
}
