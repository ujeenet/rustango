//! End-to-end live test for `ViewSet::filter_backend(...)` on SQLite
//! (issue #1010). A registered backend
//! contributes extra `WHERE` predicates on the list action, ANDed with
//! the built-in `filter_fields`.

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "serializer"))]

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use rustango::core::{Filter, Model as _, Op, SqlValue, WhereExpr};
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;
use serde_json::Value;
use std::collections::HashMap;
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone, serde::Serialize, serde::Deserialize)]
#[rustango(table = "vs_fb_post")]
#[rustango(app = "vs_fb_app")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 20)]
    pub status: String,
}

/// Backend: hide non-`published` rows unless `?include_drafts=1`.
fn published_only(
    params: &HashMap<String, String>,
    schema: &'static rustango::core::ModelSchema,
) -> Vec<WhereExpr> {
    if params.get("include_drafts").map(String::as_str) == Some("1") {
        return Vec::new();
    }
    schema.field("status").map_or_else(Vec::new, |f| {
        vec![WhereExpr::Predicate(Filter::new(
            f.column,
            Op::Eq,
            SqlValue::from("published"),
        ))]
    })
}

async fn router() -> (axum::Router, sqlx::SqlitePool) {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE vs_fb_post (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            title TEXT NOT NULL, \
            status TEXT NOT NULL)",
    )
    .execute(&sq)
    .await
    .expect("create");
    let pool = Pool::Sqlite(sq.clone());
    let app = rustango::viewset::ViewSet::for_model(Post::SCHEMA, &["id", "title", "status"])
        .allow_anonymous()
        .page_size(100)
        .filter_fields(&["title"])
        .filter_backend(published_only)
        .router("/posts", pool);
    (app, sq)
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

/// Seeded beside the API: a create the backend would hide is refused.
async fn seed(pool: &sqlx::SqlitePool, title: &str, status: &str) {
    sqlx::query("INSERT INTO vs_fb_post (title, status) VALUES (?, ?)")
        .bind(title)
        .bind(status)
        .execute(pool)
        .await
        .expect("seed");
}

fn count(body: &Value) -> usize {
    body["results"].as_array().expect("results").len()
}

#[tokio::test]
async fn filter_backend_constrains_list_by_default() {
    let (app, pool) = router().await;
    seed(&pool, "a", "published").await;
    seed(&pool, "b", "published").await;
    seed(&pool, "c", "published").await;
    seed(&pool, "d", "draft").await;
    seed(&pool, "e", "draft").await;

    // Default: backend hides the 2 drafts → 3 published.
    let body = body_json(app.clone().oneshot(get("/posts")).await.unwrap()).await;
    assert_eq!(body["count"], 3, "backend should hide drafts: {body}");
    assert_eq!(count(&body), 3);
    assert!(body["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["status"] == "published"));

    // Escape hatch: ?include_drafts=1 disables the backend → all 5.
    let body = body_json(
        app.clone()
            .oneshot(get("/posts?include_drafts=1"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(body["count"], 5);
}

#[tokio::test]
async fn filter_backend_ands_with_builtin_filter_fields() {
    let (app, pool) = router().await;
    seed(&pool, "shared", "published").await;
    seed(&pool, "shared", "draft").await;

    // ?title=shared matches both rows, but the backend AND-s status=published.
    let resp = app
        .clone()
        .oneshot(get("/posts?title=shared"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["count"], 1,
        "title filter AND status=published: {body}"
    );
    assert_eq!(body["results"][0]["status"], "published");
}
