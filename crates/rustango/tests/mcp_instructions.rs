//! `initialize` carries `instructions` only once some are registered.
//! One test: the registry is process-global.
#![cfg(feature = "mcp")]

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn initialize() -> Value {
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}).to_string(),
        ))
        .unwrap();
    let resp = rustango::mcp::tenant_router().oneshot(req).await.unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    body["result"].clone()
}

#[tokio::test]
async fn instructions_are_sent_only_when_registered() {
    let result = initialize().await;
    assert!(result.get("instructions").is_none(), "{result}");
    assert_eq!(result["serverInfo"]["name"], "rustango");

    rustango::mcp::register_instructions("Call describe_site first.");
    rustango::mcp::register_instructions("  ");
    rustango::mcp::register_instructions("Style pages with the shared stylesheet.");
    rustango::mcp::register_instructions("Call describe_site first.");
    let result = initialize().await;
    assert_eq!(
        result["instructions"],
        "Call describe_site first.\n\nStyle pages with the shared stylesheet."
    );
    assert_eq!(result["protocolVersion"], rustango::mcp::PROTOCOL_VERSION);
}
