//! End-to-end live test for the ViewSet RFC 10008 QUERY action (#1112,
//! epic #1107) on SQLite.
//!
//! `QUERY /posts` returns the same filtered / ordered / paginated list as
//! `GET /posts?…`, but with the criteria in the request body — urlencoded
//! (identical to the querystring path) or JSON (arrays for `__in`). This
//! is the DRF-beyond capability: complex search criteria that outgrow a
//! querystring travel in a safe, idempotent request body.

#![cfg(all(
    feature = "admin",
    feature = "sqlite",
    feature = "tenancy",
    feature = "serializer"
))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::core::Model as _;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;
use serde_json::Value;
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone, serde::Serialize, serde::Deserialize)]
#[rustango(table = "vs_query_post")]
#[rustango(app = "vs_query_app")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 50)]
    pub status: String,
    pub rating: i32,
}

async fn fresh_pool() -> Pool {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE vs_query_post (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            title TEXT NOT NULL, \
            status TEXT NOT NULL, \
            rating INTEGER NOT NULL)",
    )
    .execute(&sq)
    .await
    .expect("create");
    Pool::Sqlite(sq)
}

fn app(pool: Pool) -> axum::Router {
    rustango::viewset::ViewSet::for_model(Post::SCHEMA)
        .page_size(50)
        .filter_fields(&["status", "rating"])
        .ordering_fields(&["rating", "title"])
        .router_pool("/posts", pool)
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("body bytes");
    serde_json::from_slice(&bytes).expect("json")
}

fn ids(body: &Value) -> Vec<i64> {
    body["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|r| r["id"].as_i64().expect("id i64"))
        .collect()
}

async fn seed(app: &axum::Router) {
    for (title, status, rating) in [
        ("a", "draft", 3),
        ("b", "published", 1),
        ("c", "draft", 2),
        ("d", "published", 5),
    ] {
        let payload =
            serde_json::json!({ "title": title, "status": status, "rating": rating }).to_string();
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
        assert!(resp.status().is_success(), "seed {title} failed");
    }
}

fn query_req(content_type: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(Method::from_bytes(b"QUERY").unwrap())
        .uri("/posts")
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body.to_owned()))
        .unwrap()
}

#[tokio::test]
async fn query_urlencoded_body_matches_get_querystring() {
    let app = app(fresh_pool().await);
    seed(&app).await;

    let criteria = "status=draft&ordering=-rating";
    let get = body_json(
        app.clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(&format!("/posts?{criteria}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    let query = body_json(
        app.clone()
            .oneshot(query_req("application/x-www-form-urlencoded", criteria))
            .await
            .unwrap(),
    )
    .await;

    // draft rows are id 1 (rating 3) and id 3 (rating 2); -rating → [1, 3].
    assert_eq!(ids(&get), vec![1, 3]);
    assert_eq!(
        ids(&get),
        ids(&query),
        "QUERY body and GET querystring must return the same rows"
    );
    assert_eq!(get["count"], query["count"]);
}

#[tokio::test]
async fn query_json_body_with_in_array() {
    let app = app(fresh_pool().await);
    seed(&app).await;

    // JSON body with an array for `status__in` — not expressible in a flat
    // querystring value the same way; the array is comma-joined internally.
    let resp = app
        .clone()
        .oneshot(query_req(
            "application/json",
            r#"{"status__in":["draft","published"],"rating__gte":2,"ordering":"rating"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    // rating >= 2 → ids 1(3), 3(2), 4(5); status in {draft,published} keeps
    // all three; ordering=rating ASC → [3, 1, 4].
    assert_eq!(ids(&body), vec![3, 1, 4]);
}

#[tokio::test]
async fn query_unsupported_content_type_is_415() {
    let app = app(fresh_pool().await);
    seed(&app).await;
    let resp = app
        .oneshot(query_req("text/plain", "status=draft"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn query_empty_body_lists_everything() {
    let app = app(fresh_pool().await);
    seed(&app).await;
    // No criteria → full list, same as GET /posts.
    let resp = app
        .oneshot(query_req("application/x-www-form-urlencoded", ""))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["count"], 4);
}
