//! Integration test for the host-validation middleware — Django
//! `ALLOWED_HOSTS` parity. Mounts the layer on an axum Router, sends
//! requests with various Host headers via tower's oneshot, and
//! asserts the gate semantics:
//!
//! * Exact host → 200
//! * Subdomain (via `.example.com`) → 200
//! * Mismatched host → 400 with the Django-shape error body
//! * Missing Host header → 400 (allowlist is non-empty)
//! * Empty allowlist → every host passes (DEBUG-style opt-out)

#![cfg(feature = "admin")]

use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode};
use axum::routing::get;
use axum::Router;
use rustango::host_validation::{AllowedHostsLayer, AllowedHostsRouterExt};
use tower::ServiceExt;

fn build_app(layer: AllowedHostsLayer) -> Router {
    Router::new()
        .route("/", get(|| async { "ok" }))
        .allowed_hosts(layer)
}

async fn run(app: Router, host: Option<&str>) -> StatusCode {
    let mut req = Request::builder().uri("/").method("GET");
    if let Some(h) = host {
        req = req.header("Host", HeaderValue::from_str(h).unwrap());
    }
    let req = req.body(Body::empty()).unwrap();
    app.oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn exact_host_passes() {
    let app = build_app(AllowedHostsLayer::new(["example.com"]));
    assert_eq!(run(app, Some("example.com")).await, StatusCode::OK);
}

#[tokio::test]
async fn subdomain_via_dot_prefix_passes() {
    let app = build_app(AllowedHostsLayer::new([".example.com"]));
    assert_eq!(run(app, Some("api.example.com")).await, StatusCode::OK);
}

#[tokio::test]
async fn subdomain_pattern_also_matches_base() {
    let app = build_app(AllowedHostsLayer::new([".example.com"]));
    assert_eq!(run(app, Some("example.com")).await, StatusCode::OK);
}

#[tokio::test]
async fn mismatched_host_returns_400() {
    let app = build_app(AllowedHostsLayer::new(["example.com"]));
    assert_eq!(
        run(app, Some("attacker.com")).await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn host_header_with_port_strips_and_matches() {
    let app = build_app(AllowedHostsLayer::new(["example.com"]));
    assert_eq!(run(app, Some("example.com:8443")).await, StatusCode::OK);
}

#[tokio::test]
async fn missing_host_header_is_rejected_when_list_is_set() {
    let app = build_app(AllowedHostsLayer::new(["example.com"]));
    assert_eq!(run(app, None).await, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn empty_list_disables_validation() {
    let app = build_app(AllowedHostsLayer::new(Vec::<String>::new()));
    assert_eq!(
        run(app.clone(), Some("anywhere.example")).await,
        StatusCode::OK
    );
    assert_eq!(run(app, None).await, StatusCode::OK);
}

#[tokio::test]
async fn star_passes_every_host() {
    let app = build_app(AllowedHostsLayer::new(["*"]));
    assert_eq!(run(app, Some("attacker.com")).await, StatusCode::OK);
}

#[tokio::test]
async fn suffix_collision_does_not_match_subdomain_pattern() {
    // `.example.com` must not match `evilexample.com` — that's a
    // Django-bug-shaped pitfall. Verify the dot-boundary check.
    let app = build_app(AllowedHostsLayer::new([".example.com"]));
    assert_eq!(
        run(app, Some("evilexample.com")).await,
        StatusCode::BAD_REQUEST
    );
}
