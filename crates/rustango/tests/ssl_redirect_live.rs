//! Integration test for the HTTP → HTTPS redirect middleware —
//! Django `SECURE_SSL_REDIRECT` parity. Mounts the layer on an axum
//! Router, sends requests via tower's oneshot, and asserts:
//!
//! * Plain HTTP request → 301 with `Location: https://...` set.
//! * `X-Forwarded-Proto: https` from proxy → pass-through (200).
//! * Exempt-prefix path → pass-through (200).
//! * Default layer (no proxy header configured) redirects every
//!   non-HTTPS request.

#![cfg(feature = "admin")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use rustango::ssl_redirect::{SslRedirectLayer, SslRedirectRouterExt};
use tower::ServiceExt;

fn app(layer: SslRedirectLayer) -> Router {
    Router::new()
        .route("/", get(|| async { "secure" }))
        .route("/health", get(|| async { "ok" }))
        .ssl_redirect(layer)
}

fn req(path: &str, host: &str, forwarded_proto: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .uri(path)
        .method("GET")
        .header("Host", host);
    if let Some(p) = forwarded_proto {
        b = b.header("X-Forwarded-Proto", p);
    }
    b.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn plain_http_redirects_to_https() {
    let app = app(SslRedirectLayer::new());
    let resp = app
        .oneshot(req("/api/users?page=2", "example.com", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::MOVED_PERMANENTLY);
    let loc = resp
        .headers()
        .get("location")
        .expect("Location set")
        .to_str()
        .unwrap();
    assert_eq!(loc, "https://example.com/api/users?page=2");
}

#[tokio::test]
async fn forwarded_proto_https_passes_through() {
    let app = app(SslRedirectLayer::new().proxy_ssl_header("X-Forwarded-Proto", "https"));
    let resp = app
        .oneshot(req("/", "example.com", Some("https")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn forwarded_proto_http_still_redirects() {
    let app = app(SslRedirectLayer::new().proxy_ssl_header("X-Forwarded-Proto", "https"));
    let resp = app
        .oneshot(req("/", "example.com", Some("http")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::MOVED_PERMANENTLY);
}

#[tokio::test]
async fn exempt_path_bypasses_redirect() {
    let app = app(SslRedirectLayer::new().exempt(["/health"]));
    let resp = app
        .oneshot(req("/health", "example.com", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn exempt_prefix_matches_subpath() {
    let app = app(SslRedirectLayer::new().exempt(["/health"]));
    let resp = app
        .oneshot(req("/health/db", "example.com", None))
        .await
        .unwrap();
    // /health/db doesn't exist as a route → 404 — but the
    // redirect was correctly skipped (otherwise we'd see 301).
    assert_ne!(resp.status(), StatusCode::MOVED_PERMANENTLY);
}

#[tokio::test]
async fn non_exempt_path_redirects_with_query_intact() {
    let app = app(SslRedirectLayer::new().exempt(["/health"]));
    let resp = app
        .oneshot(req("/api/foo?bar=1&baz=2", "example.com", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::MOVED_PERMANENTLY);
    let loc = resp.headers().get("location").unwrap().to_str().unwrap();
    assert_eq!(loc, "https://example.com/api/foo?bar=1&baz=2");
}
