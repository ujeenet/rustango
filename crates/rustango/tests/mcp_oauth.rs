//! MCP follow-up #1088 — OAuth 2.1 discovery interop.
//!
//! The `.well-known/*` metadata endpoints require no `TenantContext`, so we
//! drive them over HTTP via `oneshot`. (The `client_credentials` token
//! endpoint shares the Slice-2 mint path, covered by `mcp_slice2`.)
#![cfg(all(feature = "sqlite", feature = "mcp"))]
#![allow(irrefutable_let_patterns)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::tenancy::jwt_lifecycle::JwtLifecycle;
use serde_json::Value;
use tower::ServiceExt;

async fn get(path: &str) -> (StatusCode, Value) {
    let app = rustango::mcp::tenant_router_authed(Arc::new(JwtLifecycle::new(
        b"oauth-test-secret-thirty-two-bytes-min!!".to_vec(),
    )));
    let req = Request::builder()
        .method("GET")
        .uri(path)
        .header("host", "app.example")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn protected_resource_metadata_is_served() {
    let (status, body) = get("/.well-known/oauth-protected-resource").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["resource"], "https://app.example");
    assert!(body["authorization_servers"][0]
        .as_str()
        .unwrap()
        .ends_with("/.well-known/oauth-authorization-server"));
    assert_eq!(body["bearer_methods_supported"][0], "header");
}

#[tokio::test]
async fn authorization_server_metadata_advertises_client_credentials() {
    let (status, body) = get("/.well-known/oauth-authorization-server").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["issuer"], "https://app.example");
    assert_eq!(body["token_endpoint"], "https://app.example/oauth/token");
    assert_eq!(body["grant_types_supported"][0], "client_credentials");
}

// localhost origin falls back to http (dev).
#[tokio::test]
async fn localhost_origin_uses_http() {
    let app = rustango::mcp::tenant_router_authed(Arc::new(JwtLifecycle::new(
        b"oauth-test-secret-thirty-two-bytes-min!!".to_vec(),
    )));
    let req = Request::builder()
        .method("GET")
        .uri("/.well-known/oauth-authorization-server")
        .header("host", "localhost:8080")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["issuer"], "http://localhost:8080");
}

// #1094: when the router is nested under a prefix, the advertised metadata
// URLs must track that prefix (RFC-9728 clients fetch the wrong path
// otherwise). Drives the real axum nest via `OriginalUri`.
#[tokio::test]
async fn metadata_urls_track_the_nest_prefix() {
    let mcp = rustango::mcp::tenant_router_authed(Arc::new(JwtLifecycle::new(
        b"oauth-test-secret-thirty-two-bytes-min!!".to_vec(),
    )));
    let app = axum::Router::new().nest("/api/mcp", mcp);

    let req = Request::builder()
        .method("GET")
        .uri("/api/mcp/.well-known/oauth-protected-resource")
        .header("host", "app.example")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let prm: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(prm["resource"], "https://app.example/api/mcp");
    assert_eq!(
        prm["authorization_servers"][0],
        "https://app.example/api/mcp/.well-known/oauth-authorization-server"
    );

    let req = Request::builder()
        .method("GET")
        .uri("/api/mcp/.well-known/oauth-authorization-server")
        .header("host", "app.example")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let asm: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(asm["issuer"], "https://app.example/api/mcp");
    assert_eq!(
        asm["token_endpoint"],
        "https://app.example/api/mcp/oauth/token"
    );
}
