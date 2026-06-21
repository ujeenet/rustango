//! Backing test for `docs/testing.md` — the in-process `TestClient`: drive a
//! router over real HTTP semantics without binding a socket or booting a server.
//! (This test *is* the example.)
//!
//! Run: `cargo test -p rustango --test testing_doc`

use axum::routing::{get, post};
use axum::{Json, Router};
use rustango::test_client::TestClient;
use serde_json::json;

/// The app under test — a tiny router. In a real project this is your
/// `urls::api()` (optionally with a test pool merged in).
fn app() -> Router {
    Router::new()
        .route("/ping", get(|| async { "pong" }))
        .route(
            "/echo",
            post(|Json(v): Json<serde_json::Value>| async move { Json(v) }),
        )
}

#[tokio::test]
async fn get_returns_status_and_body() {
    let client = TestClient::new(app());

    let res = client.get("/ping").send().await;
    assert_eq!(res.status, 200);
    assert_eq!(res.text(), "pong");
    // The content type is readable off the response.
    assert!(res.header("content-type").unwrap().contains("text/plain"));
}

#[tokio::test]
async fn post_json_round_trips() {
    let client = TestClient::new(app());

    let res = client
        .post("/echo")
        .json(&json!({ "name": "Ada", "admin": true }))
        .send()
        .await;

    assert_eq!(res.status, 200);
    // Inspect the JSON body untyped...
    assert_eq!(res.json_value()["name"], "Ada");

    // ...or deserialize it into a type.
    #[derive(serde::Deserialize)]
    struct Out {
        name: String,
        admin: bool,
    }
    let out: Out = res.json();
    assert_eq!(out.name, "Ada");
    assert!(out.admin);
}

#[tokio::test]
async fn unknown_route_is_404() {
    let client = TestClient::new(app());
    let res = client.get("/does-not-exist").send().await;
    assert_eq!(res.status, 404);
}

#[tokio::test]
async fn custom_headers_are_sent() {
    // Headers (auth tokens, content negotiation) attach via .header(..).
    let client = TestClient::new(app());
    let res = client
        .post("/echo")
        .header("x-request-id", "abc123")
        .json(&json!({ "ok": 1 }))
        .send()
        .await;
    assert_eq!(res.status, 200);
    assert_eq!(res.json_value()["ok"], 1);
}
