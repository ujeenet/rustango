//! Django-parity #418 (Slack provider variant) — `notifications::slack`
//! webhook callback round-tripped through a real HTTP server.
//!
//! Spins up an axum listener, plugs the callback into a
//! NotificationContext, fires a broadcast, and inspects the captured
//! POST body to verify the Slack envelope shape.

#![cfg(all(
    feature = "notifications",
    feature = "http-client",
    feature = "postgres"
))]

use std::sync::Arc;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use rustango::notifications::slack;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

/// Bind to 127.0.0.1:0, return (base_url, captured-bodies handle).
async fn spawn_capture_server() -> (String, Arc<Mutex<Vec<Value>>>) {
    let captured: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let state = Arc::clone(&captured);
    let app = Router::new()
        .route(
            "/hook",
            post(
                |State(state): State<Arc<Mutex<Vec<Value>>>>, Json(body): Json<Value>| async move {
                    state.lock().await.push(body);
                    axum::http::StatusCode::OK
                },
            ),
        )
        .with_state(state);

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let url = format!("http://{}/hook", addr);
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (url, captured)
}

#[tokio::test]
async fn webhook_callback_posts_text_envelope_for_string_payload() {
    let (url, captured) = spawn_capture_server().await;
    let cb = slack::webhook_callback(url);
    cb(json!("disk-full on prod-db-2")).await.expect("send ok");

    let bodies = captured.lock().await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(
        bodies[0],
        json!({ "text": "disk-full on prod-db-2" }),
        "Slack envelope should wrap a string payload",
    );
}

#[tokio::test]
async fn webhook_callback_passes_through_block_payload() {
    let (url, captured) = spawn_capture_server().await;
    let cb = slack::webhook_callback(url);

    let block_payload = json!({
        "blocks": [
            { "type": "section", "text": { "type": "mrkdwn", "text": "*incident*" } }
        ]
    });
    cb(block_payload.clone()).await.expect("send ok");

    let bodies = captured.lock().await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0], block_payload, "rich payload must pass through");
}

#[tokio::test]
async fn webhook_callback_surfaces_non_2xx_as_error() {
    // Spin up a server that always returns 500.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let url = format!("http://{}/hook", addr);

    let app = Router::new().route(
        "/hook",
        post(|| async { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let cb = slack::webhook_callback(url);
    let err = cb(json!("ping"))
        .await
        .expect_err("500 should surface as Err");
    assert!(
        err.contains("500"),
        "error string should mention status: {err}"
    );
    assert!(
        err.contains("boom"),
        "error string should include body: {err}"
    );
}
