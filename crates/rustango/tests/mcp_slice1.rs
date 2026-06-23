//! MCP Slice 1 (#1014) acceptance — an MCP client completes `initialize` +
//! `ping` over the Streamable-HTTP transport, and the router mounts.
//!
//! Run: `cargo test -p rustango --features mcp --test mcp_slice1`.
#![cfg(feature = "mcp")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt; // for `oneshot`

/// POST one JSON-RPC message at the (tenant) MCP router and return
/// `(status, parsed-body-or-Null)`.
async fn post(message: Value) -> (StatusCode, Value) {
    let app = rustango::mcp::tenant_router();
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&message).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}

#[tokio::test]
async fn initialize_handshake_returns_protocol_and_server_info() {
    let (status, body) = post(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}
    }))
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 1);
    assert_eq!(
        body["result"]["protocolVersion"],
        rustango::mcp::PROTOCOL_VERSION
    );
    assert_eq!(body["result"]["serverInfo"]["name"], "rustango");
    // Slices 3 + 5 advertise tools / prompts / resources (asserted
    // individually so new capabilities don't break this).
    assert_eq!(
        body["result"]["capabilities"]["tools"]["listChanged"],
        false
    );
    assert!(body["result"]["capabilities"]["prompts"].is_object());
    assert!(body["result"]["capabilities"]["resources"].is_object());
    assert!(body.get("error").is_none());
}

#[tokio::test]
async fn ping_returns_empty_result() {
    let (status, body) = post(json!({"jsonrpc": "2.0", "id": "p1", "method": "ping"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], "p1");
    assert_eq!(body["result"], json!({}));
}

#[tokio::test]
async fn unknown_method_is_method_not_found() {
    let (status, body) =
        post(json!({"jsonrpc": "2.0", "id": 7, "method": "totally/unknown"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["error"]["code"],
        rustango::mcp::codes::METHOD_NOT_FOUND
    );
    assert!(body.get("result").is_none());
}

#[tokio::test]
async fn notification_is_accepted_with_no_body() {
    // No `id` → notification: 202, empty body, no JSON-RPC response.
    let (status, body) =
        post(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body, Value::Null);
}

#[tokio::test]
async fn malformed_json_is_parse_error() {
    let app = rustango::mcp::tenant_router();
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("content-type", "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["code"], rustango::mcp::codes::PARSE_ERROR);
    assert_eq!(body["id"], Value::Null);
}
