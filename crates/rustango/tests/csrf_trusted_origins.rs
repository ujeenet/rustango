//! Django parity — `CSRF_TRUSTED_ORIGINS` setting. Adds Origin-header
//! defense-in-depth to the CSRF middleware on top of the existing
//! double-submit-cookie token check.
//!
//! Behavior:
//! * Default `trusted_origins: []` → Origin-header check disabled
//!   (back-compat). Only the token check runs.
//! * Non-empty list → on unsafe methods, request's Origin must be
//!   either same-host or match one of the trusted entries.
//! * `https://*.example.com` wildcard matches any subdomain.

#![cfg(feature = "csrf")]

use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode};
use axum::routing::post;
use axum::Router;
use rustango::forms::csrf::{with_config, CsrfConfig};
use tower::ServiceExt;

const TOKEN: &str = "tokvalue";

fn app(cfg: CsrfConfig) -> Router {
    let cookie_name = cfg.cookie_name.clone();
    Router::new()
        .route("/post", post(|| async { "ok" }))
        .layer(with_config(cfg))
        .layer(axum::middleware::from_fn(
            move |mut req: Request<Body>, next: axum::middleware::Next| {
                // Seed the CSRF cookie so token-match passes when
                // the test sends a matching header. Pre-layer
                // middleware so the cookie is visible to the CSRF
                // service.
                let cookie_name = cookie_name.clone();
                async move {
                    let cookie = format!("{cookie_name}={TOKEN}");
                    if !req.headers().contains_key("cookie") {
                        req.headers_mut()
                            .insert("cookie", HeaderValue::from_str(&cookie).unwrap());
                    }
                    next.run(req).await
                }
            },
        ))
}

async fn post_with_headers(app: Router, host: &str, origin: Option<&str>) -> StatusCode {
    let mut req = Request::builder()
        .uri("/post")
        .method("POST")
        .header("Host", host)
        .header("X-CSRF-Token", TOKEN)
        .header("Cookie", format!("rustango_csrf={TOKEN}"));
    if let Some(o) = origin {
        req = req.header("Origin", o);
    }
    let req = req.body(Body::empty()).unwrap();
    app.oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn empty_trusted_origins_disables_origin_check() {
    let app = app(CsrfConfig::default().allow_insecure_for_dev());
    // Cross-origin POST passes when trusted_origins is empty
    // (back-compat with pre-v0.43 token-only checking).
    assert_eq!(
        post_with_headers(app, "example.com", Some("https://attacker.com")).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn same_host_origin_passes_with_trusted_origins_set() {
    let app = app(CsrfConfig::default()
        .allow_insecure_for_dev()
        .trust_origin("https://other.example.com"));
    assert_eq!(
        post_with_headers(app, "example.com", Some("https://example.com")).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn cross_origin_rejected_when_not_in_trusted_list() {
    let app = app(CsrfConfig::default()
        .allow_insecure_for_dev()
        .trust_origin("https://other.example.com"));
    assert_eq!(
        post_with_headers(app, "example.com", Some("https://attacker.com")).await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn cross_origin_in_trusted_list_passes() {
    let app = app(CsrfConfig::default()
        .allow_insecure_for_dev()
        .trust_origin("https://app.example.com"));
    assert_eq!(
        post_with_headers(app, "example.com", Some("https://app.example.com")).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn wildcard_subdomain_pattern_matches() {
    let app = app(CsrfConfig::default()
        .allow_insecure_for_dev()
        .trust_origin("https://*.example.com"));
    // Subdomain matches.
    assert_eq!(
        post_with_headers(
            app.clone(),
            "api.example.com",
            Some("https://web.example.com")
        )
        .await,
        StatusCode::OK
    );
    // Unrelated host doesn't match.
    assert_eq!(
        post_with_headers(app, "api.example.com", Some("https://evilexample.com")).await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn missing_origin_falls_back_to_token_check_only() {
    // No Origin header (curl / server-to-server) → trusted_origins
    // setting is bypassed; only the double-submit token gates.
    let app = app(CsrfConfig::default()
        .allow_insecure_for_dev()
        .trust_origin("https://app.example.com"));
    assert_eq!(
        post_with_headers(app, "example.com", None).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn with_trusted_origins_replaces_list() {
    let cfg = CsrfConfig::default()
        .allow_insecure_for_dev()
        .trust_origin("https://first.example.com")
        .with_trusted_origins(["https://second.example.com"]);
    // `with_trusted_origins` replaces — `first.example.com` is no
    // longer trusted.
    let app = app(cfg);
    assert_eq!(
        post_with_headers(
            app.clone(),
            "example.com",
            Some("https://first.example.com")
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post_with_headers(app, "example.com", Some("https://second.example.com")).await,
        StatusCode::OK
    );
}
