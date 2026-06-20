//! Proves `oauth2::router::oauth2_router` is reachable **without** the `admin`
//! feature — it only needs axum, which arrives via `manage`. Before the cfg fix
//! in `oauth2/mod.rs`, `pub mod router` was gated on `admin`, so a non-admin app
//! enabling just `oauth2` + `manage` could not mount social login at all.
//!
//! The `not(feature = "admin")` gate is the real proof: this file only compiles
//! when admin is OFF, so a green run here means the standalone path works. It is
//! excluded from every admin-on build (default, CI `postgres_test`,
//! `doc_examples`), so it never breaks those jobs.
//!
//! KNOWN LIMITATION (pre-existing, unrelated to this fix): a *minimal* non-admin
//! build of `rustango` currently fails to compile because several always-on
//! modules (`template_extensions`, `i18n`, `template_views`) reference optional
//! deps (tera, tower, serde_urlencoded, …) that only a larger feature bundle
//! supplies — CI's minimal combos all enable `tenancy`, which pulls that bundle
//! (and `admin`), so the gap is invisible there. Until those modules are gated
//! tightly (or `manage` pulls what its always-on siblings need), the smallest
//! combo that compiles this test is roughly:
//!   cargo test -p rustango --no-default-features \
//!     --features "oauth2 manage template_views csrf forms serializer sqlite" \
//!     --test oauth2_router_standalone
//! The A1 cfg fix itself is verified on the admin path: `auth_demo` builds green
//! with `oauth2` + `manage` + `admin`, exercising `oauth2::router`.
#![cfg(all(feature = "oauth2", feature = "manage", not(feature = "admin")))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::response::Redirect;
use rustango::oauth2::router::{oauth2_router, OnAuthSuccess};
use rustango::oauth2::{providers, OAuth2Registry};
use tower::ServiceExt;

fn dummy_success() -> OnAuthSuccess {
    Arc::new(|_user, _tokens| Box::pin(async { Ok(Redirect::to("/dashboard")) }))
}

#[tokio::test]
async fn login_redirects_with_secure_flow_cookie_without_admin() {
    let registry = OAuth2Registry::new();
    registry.register("acme", providers::google("cid", "csec", "https://app/cb"));
    let app = oauth2_router(
        registry,
        b"flow-signing-secret".to_vec(),
        true,
        dummy_success(),
    );

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/auth/acme/google/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let loc = resp
        .headers()
        .get(header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        loc.contains("accounts.google.com"),
        "redirects to provider: {loc}"
    );
    let cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(cookie.contains("HttpOnly") && cookie.contains("Secure"));
}

#[tokio::test]
async fn unknown_provider_is_404_without_admin() {
    let registry = OAuth2Registry::new();
    let app = oauth2_router(
        registry,
        b"flow-signing-secret".to_vec(),
        true,
        dummy_success(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/auth/acme/google/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
