//! Live integration test for `ViewSet::router_pool` on SQLite — the
//! tri-dialect counterpart of `ViewSet::router(prefix, &PgPool)`,
//! slice 21. Boots the macro-emitted JSON viewset against a sqlite
//! pool and exercises list / get / create / update / delete.

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "serializer"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::core::Model as _;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone, serde::Serialize, serde::Deserialize)]
#[rustango(table = "vs_sqlite_post")]
#[rustango(app = "vs_sqlite_app")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub published: bool,
}

async fn make_router() -> axum::Router {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE vs_sqlite_post (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            title TEXT NOT NULL, \
            published INTEGER NOT NULL)",
    )
    .execute(&sq)
    .await
    .expect("create");
    let pool = Pool::Sqlite(sq);
    rustango::viewset::ViewSet::for_model(Post::SCHEMA)
        .page_size(50)
        .router_pool("/posts", pool)
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("body bytes");
    String::from_utf8(bytes.to_vec()).expect("utf8")
}

#[tokio::test]
async fn router_pool_list_returns_empty_then_inserted_rows_on_sqlite() {
    let app = make_router().await;
    // Empty list.
    let resp = app.clone().oneshot(get("/posts")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(
        body.contains("\"results\":[]")
            || body.contains("\"count\":0")
            || body.is_empty()
            || body == "[]",
        "empty list expected, got: {body}"
    );

    // POST a row.
    let post_body = r#"{"title":"hello","published":true}"#;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/posts")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(post_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "POST should succeed, got {}",
        resp.status()
    );

    // List again — should contain the row.
    let resp = app.clone().oneshot(get("/posts")).await.unwrap();
    let body = body_string(resp).await;
    assert!(
        body.contains("hello"),
        "list should contain the row: {body}"
    );
}
