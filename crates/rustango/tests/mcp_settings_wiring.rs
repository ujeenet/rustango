//! MCP follow-up #1098 — the `[mcp]` settings knobs are actually wired into
//! `secure_tenant_router_from_settings` (CORS / enable_sse / rate limit /
//! body cap), not just parsed and ignored.
#![cfg(all(feature = "sqlite", feature = "mcp", feature = "config"))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rustango::config::McpSettings;
use tower::ServiceExt;

#[tokio::test]
async fn allowed_origins_applies_a_cors_layer() {
    let settings = McpSettings {
        allowed_origins: vec!["https://app.example".into()],
        ..Default::default()
    };
    let app = rustango::mcp::secure_tenant_router_from_settings(&settings);
    // A well-known GET needs no TenantContext; assert the CORS layer echoes
    // the allowed origin back.
    let req = Request::builder()
        .method("GET")
        .uri("/.well-known/oauth-authorization-server")
        .header("host", "app.example")
        .header("origin", "https://app.example")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://app.example")
    );
}

#[tokio::test]
async fn cors_layer_absent_when_no_origins_configured() {
    let app = rustango::mcp::secure_tenant_router_from_settings(&McpSettings::default());
    let req = Request::builder()
        .method("GET")
        .uri("/.well-known/oauth-authorization-server")
        .header("host", "app.example")
        .header("origin", "https://app.example")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert!(resp.headers().get("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn enable_sse_false_drops_the_get_stream() {
    let settings = McpSettings {
        enable_sse: Some(false),
        ..Default::default()
    };
    let app = rustango::mcp::secure_tenant_router_from_settings(&settings);
    // GET `/` with SSE disabled → only POST is registered → 405 (routing-layer,
    // before any extractor, so no TenantContext needed).
    let req = Request::builder()
        .method("GET")
        .uri("/")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn enable_sse_default_keeps_the_get_route() {
    // Default settings (enable_sse defaults true) → GET `/` routes to the SSE
    // handler. Without auth state it 500s ("not configured") but crucially is
    // NOT 405 — i.e. the route exists.
    let app = rustango::mcp::secure_tenant_router_from_settings(&McpSettings::default());
    let req = Request::builder()
        .method("GET")
        .uri("/")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_ne!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}
