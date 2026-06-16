//! Live integration test proving the ViewSet renders list / retrieve /
//! create responses **through a registered serializer**, tri-dialect
//! (here on SQLite). Before v0.45 `.serializer::<S>()` stored a dead
//! PG-only `row_render` closure that was never invoked — output was
//! always the default field-level projection. This test pins the new
//! behavior: `method` fields appear, `write_only` fields are hidden,
//! and the response shape is the serializer's, not the raw model's.

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "serializer"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::core::Model as _;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::{Model, Serializer};
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone, serde::Serialize, serde::Deserialize)]
#[rustango(table = "vs_ser_post")]
#[rustango(app = "vs_ser_app")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 500)]
    pub body: String,
}

/// Serializer that reshapes `Post`:
/// * `excerpt` is a computed `method` field (first 10 chars of body),
/// * `body` is `write_only` — accepted on write, hidden from output.
///
/// So the rendered shape is `{ "title", "excerpt" }` — provably
/// different from the raw model projection (which would include
/// `id` + `body`).
#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
#[allow(dead_code)] // `body` is write-only by design — never read directly.
struct PostSerializer {
    pub title: String,
    #[serializer(method = "excerpt")]
    pub excerpt: String,
    #[serializer(write_only)]
    pub body: String,
}

impl PostSerializer {
    fn excerpt(p: &Post) -> String {
        p.body.chars().take(10).collect::<String>()
    }
}

async fn sqlite_pool() -> Pool {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE vs_ser_post (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            title TEXT NOT NULL, \
            body TEXT NOT NULL)",
    )
    .execute(&sq)
    .await
    .expect("create");
    Pool::Sqlite(sq)
}

/// ViewSet with the serializer wired in.
async fn serializer_router() -> axum::Router {
    rustango::viewset::ViewSet::for_model(Post::SCHEMA)
        .page_size(50)
        .serializer::<PostSerializer>()
        .router_pool("/posts", sqlite_pool().await)
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

fn post_json(uri: &str, json: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json.to_owned()))
        .unwrap()
}

async fn json_body(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("body bytes");
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

#[tokio::test]
async fn create_response_renders_through_serializer() {
    let app = serializer_router().await;

    let resp = app
        .clone()
        .oneshot(post_json(
            "/posts",
            r#"{"title":"Hello","body":"the quick brown fox jumps"}"#,
        ))
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "POST should succeed, got {}",
        resp.status()
    );

    let v = json_body(resp).await;
    // Serializer shape: title + computed excerpt, NO body, NO id.
    assert_eq!(v["title"], "Hello", "title should be serialized: {v}");
    assert_eq!(
        v["excerpt"], "the quick ",
        "method field `excerpt` (first 10 chars) should be rendered: {v}"
    );
    assert!(
        v.get("body").is_none(),
        "write_only `body` must be hidden from output: {v}"
    );
    assert!(
        v.get("id").is_none(),
        "serializer didn't declare `id`, so it must be absent: {v}"
    );
}

#[tokio::test]
async fn list_and_retrieve_render_through_serializer() {
    let app = serializer_router().await;

    // Seed two rows.
    for (t, b) in [("First", "aaaaaaaaaaaa"), ("Second", "bbbbbbbbbbbb")] {
        let resp = app
            .clone()
            .oneshot(post_json(
                "/posts",
                &format!(r#"{{"title":"{t}","body":"{b}"}}"#),
            ))
            .await
            .unwrap();
        assert!(resp.status().is_success());
    }

    // LIST — each result is serializer-shaped.
    let resp = app.clone().oneshot(get("/posts")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    let results = v["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2, "two rows expected: {v}");
    for row in results {
        assert!(row.get("excerpt").is_some(), "excerpt rendered: {row}");
        assert!(row.get("body").is_none(), "body hidden: {row}");
    }
    assert_eq!(results[0]["title"], "First");
    assert_eq!(results[0]["excerpt"], "aaaaaaaaaa"); // first 10 of 12 a's

    // RETRIEVE by pk — serializer-shaped too.
    let resp = app.clone().oneshot(get("/posts/1")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["title"], "First", "retrieve renders serializer: {v}");
    assert_eq!(v["excerpt"], "aaaaaaaaaa");
    assert!(v.get("body").is_none(), "body hidden on retrieve: {v}");
}

#[tokio::test]
async fn without_serializer_default_projection_is_unchanged() {
    // Contrast: same model, no `.serializer()` — the default
    // field-level projection still includes every column (id + body),
    // confirming the serializer path is what reshapes the output.
    let app = rustango::viewset::ViewSet::for_model(Post::SCHEMA)
        .page_size(50)
        .router_pool("/posts", sqlite_pool().await);

    let resp = app
        .clone()
        .oneshot(post_json(
            "/posts",
            r#"{"title":"Raw","body":"visible body"}"#,
        ))
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let v = json_body(resp).await;
    assert_eq!(v["title"], "Raw");
    assert_eq!(
        v["body"], "visible body",
        "default projection keeps body: {v}"
    );
    assert!(v.get("id").is_some(), "default projection keeps id: {v}");
    assert!(
        v.get("excerpt").is_none(),
        "no serializer → no computed excerpt: {v}"
    );
}
