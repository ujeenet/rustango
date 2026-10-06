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
    assert_eq!(body["result"]["capabilities"]["tools"]["listChanged"], true);
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
async fn resources_templates_list_is_routed_not_method_not_found() {
    // #1099: the spec method exists. Over the unauthed transport it's
    // auth-required (INVALID_REQUEST) — crucially NOT method-not-found, which
    // proves the dispatch arm is wired (returns an empty list once authed).
    let (status, body) =
        post(json!({"jsonrpc": "2.0", "id": 9, "method": "resources/templates/list"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["error"]["code"], rustango::mcp::codes::INVALID_REQUEST);
    assert_ne!(
        body["error"]["code"],
        rustango::mcp::codes::METHOD_NOT_FOUND
    );
}

#[tokio::test]
async fn notification_is_accepted_with_no_body() {
    // No `id` → notification: 202, empty body, no JSON-RPC response.
    let (status, body) =
        post(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body, Value::Null);
}

/// A null id or a wrong `jsonrpc` is an invalid request, not a notification (#1963).
#[tokio::test]
async fn null_id_and_wrong_version_are_invalid_requests() {
    for msg in [
        json!({"jsonrpc": "2.0", "id": null, "method": "ping"}),
        json!({"jsonrpc": "1.0", "id": 3, "method": "ping"}),
        json!({"id": 4, "method": "ping"}),
    ] {
        let (status, body) = post(msg.clone()).await;
        assert_eq!(status, StatusCode::OK, "{msg}");
        assert_eq!(
            body["error"]["code"],
            rustango::mcp::codes::INVALID_REQUEST,
            "{msg}"
        );
    }
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

/// The unauthenticated routers have no SSE stream: it needs an agent token (#1802).
#[tokio::test]
async fn unauthenticated_routers_do_not_mount_the_sse_stream() {
    let mut apps = vec![rustango::mcp::tenant_router()];
    #[cfg(feature = "sqlite")]
    apps.push(rustango::mcp::router(rustango::sql::Pool::Sqlite(
        rustango::sql::sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap(),
    )));
    for app in apps {
        let req = Request::get("/").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
}
