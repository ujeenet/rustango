//! The `CSRF_TRUSTED_ORIGINS` setting. Adds Origin-header
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

/// An empty `trusted_origins` checks the Origin against the request's
/// own Host instead of skipping the check (#1529).
///
/// This test used to assert the opposite — that a cross-origin POST
/// with a matching token pair returned 200 — which is the attack
/// itself: the pair is forgeable by anyone who can write a cookie on
/// the parent domain, and Origin is what catches it.
#[tokio::test]
async fn empty_trusted_origins_still_checks_against_host() {
    let app = app(CsrfConfig::default().allow_insecure_for_dev());
    assert_eq!(
        post_with_headers(app.clone(), "example.com", Some("https://attacker.com")).await,
        StatusCode::FORBIDDEN,
        "a foreign Origin must be refused with the default config"
    );
    // Same-origin still works with no configuration.
    assert_eq!(
        post_with_headers(app, "example.com", Some("http://example.com")).await,
        StatusCode::OK,
        "same-origin traffic must need no trusted_origins entry"
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

// ---------------------------------------------------------------- #1529

/// Like `post_with_headers`, plus an arbitrary extra header — used to
/// present the request as having arrived over TLS.
async fn post_with_proto(
    app: Router,
    host: &str,
    origin: &str,
    proto_header: Option<(&str, &str)>,
) -> StatusCode {
    let mut req = Request::builder()
        .uri("/post")
        .method("POST")
        .header("Host", host)
        .header("Origin", origin)
        .header("X-CSRF-Token", TOKEN)
        .header("Cookie", format!("rustango_csrf={TOKEN}"));
    if let Some((k, v)) = proto_header {
        req = req.header(k, v);
    }
    let req = req.body(Body::empty()).unwrap();
    app.oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn an_origin_without_a_scheme_is_not_same_origin() {
    // The same-origin test used to fall back to comparing the raw
    // header against Host, so anything spelling the host matched.
    let app = app(CsrfConfig::default()
        .allow_insecure_for_dev()
        .trust_origin("https://app.example.com"));
    for bogus in ["example.com", "null", "://example.com", "https://"] {
        assert_eq!(
            post_with_headers(app.clone(), "example.com", Some(bogus)).await,
            StatusCode::FORBIDDEN,
            "`Origin: {bogus}` is not `scheme://host` and must not pass as same-origin",
        );
    }
}

#[tokio::test]
async fn a_real_same_origin_post_still_passes() {
    // The control. Tightening the parse must not break the ordinary
    // browser case these checks exist to allow.
    let app = app(CsrfConfig::default()
        .allow_insecure_for_dev()
        .trust_origin("https://app.example.com"));
    assert_eq!(
        post_with_headers(app.clone(), "example.com", Some("https://example.com")).await,
        StatusCode::OK
    );
    // …including with a port, and over plain http when nothing says
    // the request was TLS.
    assert_eq!(
        post_with_headers(app, "example.com:8443", Some("https://example.com:8443")).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn an_http_origin_is_rejected_when_the_request_arrived_over_tls() {
    let cfg = || {
        CsrfConfig::default()
            .allow_insecure_for_dev()
            .trust_origin("https://app.example.com")
    };
    for header in [
        ("X-Forwarded-Proto", "https"),
        ("X-Forwarded-Proto", "https, http"),
        ("Forwarded", "proto=https;for=192.0.2.1"),
    ] {
        assert_eq!(
            post_with_proto(
                app(cfg()),
                "example.com",
                "http://example.com",
                Some(header)
            )
            .await,
            StatusCode::FORBIDDEN,
            "an http Origin is a different origin from the https site ({header:?})",
        );
        // Control: the https Origin passes through the same path, so
        // the rejection is about the scheme and not the header.
        assert_eq!(
            post_with_proto(
                app(cfg()),
                "example.com",
                "https://example.com",
                Some(header)
            )
            .await,
            StatusCode::OK,
            "the matching https Origin must still pass ({header:?})",
        );
    }
    // Without any TLS signal the scheme is unknown, so behaviour is
    // unchanged — a proxy forwarding neither header locks nobody out.
    assert_eq!(
        post_with_proto(app(cfg()), "example.com", "http://example.com", None).await,
        StatusCode::OK
    );
}
