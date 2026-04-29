//! Integration tests for the CSRF middleware (slice 8.4C).
//!
//! Boots a tiny axum router with the `csrf::layer()` applied, drives
//! it through `tower::ServiceExt::oneshot` (no socket), and asserts
//! the four key behaviours:
//!
//! 1. GET sets a fresh CSRF cookie when none was sent.
//! 2. POST without a token → 403.
//! 3. POST with matching cookie + X-CSRF-Token header → passes.
//! 4. POST with mismatched cookie / header → 403.

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use http_body_util::BodyExt as _;
use rustango::forms::csrf;
use tower::ServiceExt;

fn app() -> Router {
    Router::new()
        .route("/safe", get(|| async { "hello" }))
        .route("/unsafe", post(|| async { "ok" }))
        .layer(csrf::layer())
}

fn read_set_cookie(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

fn extract_csrf_value(set_cookie: &str) -> Option<&str> {
    set_cookie
        .split(';')
        .next()
        .and_then(|kv| kv.split_once('='))
        .map(|(_k, v)| v)
}

#[tokio::test]
async fn safe_method_seeds_csrf_cookie_when_none_sent() {
    let response = app()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/safe")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let set_cookie =
        read_set_cookie(response.headers()).expect("Set-Cookie should be present on first GET");
    assert!(set_cookie.contains("rustango_csrf="));
    assert!(set_cookie.contains("SameSite=Lax"));
}

#[tokio::test]
async fn unsafe_method_without_token_is_403() {
    let response = app()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/unsafe")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(std::str::from_utf8(&body).unwrap().contains("CSRF"));
}

#[tokio::test]
async fn unsafe_method_with_matching_token_passes() {
    // Step 1: GET to mint a token.
    let response = app()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/safe")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let set_cookie = read_set_cookie(response.headers()).unwrap();
    let token = extract_csrf_value(&set_cookie).unwrap().to_owned();

    // Step 2: POST with the same token in cookie + header.
    let response = app()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/unsafe")
                .header(header::COOKIE, format!("rustango_csrf={token}"))
                .header("X-CSRF-Token", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn unsafe_method_with_mismatched_token_is_403() {
    let response = app()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/unsafe")
                .header(header::COOKIE, "rustango_csrf=cookie-value")
                .header("X-CSRF-Token", "different-value")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn unsafe_method_with_cookie_only_no_header_is_403() {
    let response = app()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/unsafe")
                .header(header::COOKIE, "rustango_csrf=some-value")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
