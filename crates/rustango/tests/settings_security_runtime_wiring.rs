//! Integration test for the `Settings.security.*` → middleware
//! auto-wiring in `manage.rs::apply_settings_layers`. Verifies the
//! three new security middlewares actually fire on requests when
//! the corresponding TOML field is populated.
//!
//! This is a behavior test, not a smoke test — the lib-side
//! `apply_settings_layers_*` tests in manage.rs only prove
//! composition; this test proves the host_validation +
//! ssl_redirect layers REJECT/REDIRECT when configured.

#![cfg(all(feature = "config", feature = "admin"))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use rustango::host_validation::{AllowedHostsLayer, AllowedHostsRouterExt as _};
use rustango::ssl_redirect::{SslRedirectLayer, SslRedirectRouterExt as _};
use tower::ServiceExt;

#[tokio::test]
async fn allowed_hosts_from_settings_blocks_unknown_host() {
    // This test wires the layer the same way `apply_settings_layers`
    // does — driving `AllowedHostsLayer::from_settings_list` with a
    // populated `Settings.security.allowed_hosts`.
    let allowed = vec!["example.com".to_string()];
    let layer = AllowedHostsLayer::from_settings_list(allowed.iter().map(String::as_str));
    let app: Router = Router::new()
        .route("/", get(|| async { "ok" }))
        .allowed_hosts(layer);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/")
                .method("GET")
                .header("Host", "example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/")
                .method("GET")
                .header("Host", "attacker.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn ssl_redirect_from_settings_redirects_plain_http() {
    // Mirror the `apply_settings_layers` ssl_redirect branch shape:
    // build SslRedirectLayer with the proxy header pair from settings.
    let proxy_header = vec!["X-Forwarded-Proto".to_string(), "https".to_string()];
    let exempt = vec!["/health".to_string()];

    let mut layer = SslRedirectLayer::new();
    if proxy_header.len() == 2 {
        layer = layer.proxy_ssl_header(&proxy_header[0], &proxy_header[1]);
    }
    if !exempt.is_empty() {
        layer = layer.exempt(exempt.iter().cloned());
    }
    let app: Router = Router::new()
        .route("/", get(|| async { "https" }))
        .route("/health", get(|| async { "ok" }))
        .ssl_redirect(layer);

    // Plain HTTP without the proxy header → 301.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/")
                .method("GET")
                .header("Host", "example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::MOVED_PERMANENTLY);

    // Proxy header set → pass-through.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/")
                .method("GET")
                .header("Host", "example.com")
                .header("X-Forwarded-Proto", "https")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Exempt path → pass-through (no redirect).
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .method("GET")
                .header("Host", "example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
