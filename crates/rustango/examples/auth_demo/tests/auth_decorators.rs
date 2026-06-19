//! Backing test for `docs/auth-decorators.md` — `login_required` & friends gate
//! handlers by the request's session. The gating decisions (302 redirect / 401)
//! need NO database: an anonymous `SessionUser` resolves to `None` without any
//! session or tenant setup. The authenticated happy-path (200) needs a real
//! session — see `docs/auth-sessions.md`.
//!
//! Run: `cargo test -p auth_demo --test auth_decorators`

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::routing::get;
use axum::Router;
use rustango::auth_decorators::{login_required, safe_next, superuser_required_or_403};
use tower::ServiceExt;

async fn protected() -> &'static str {
    "secret"
}

#[tokio::test]
async fn login_required_redirects_anonymous_with_next_preserved() {
    // Browser/HTML flow: anonymous users are 302'd to the login page, with the
    // page they wanted preserved in ?next= so login can send them back.
    let app = Router::new()
        .route("/profile", get(protected))
        .layer(login_required("/login"));

    let res = app
        .oneshot(
            Request::builder()
                .uri("/profile")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::FOUND);
    let loc = res
        .headers()
        .get(header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(loc, "/login?next=%2Fprofile");
}

#[tokio::test]
async fn api_gate_returns_401_for_anonymous_not_a_redirect() {
    // JSON-API flow: the `_or_403` family returns 401 (anonymous) / 403
    // (authenticated-but-unauthorized) instead of a 302 to an HTML page a
    // client can't render.
    let app = Router::new()
        .route("/api/admin", get(protected))
        .layer(superuser_required_or_403());

    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[test]
fn safe_next_blocks_open_redirects() {
    // The login handler MUST sanitize ?next= before redirecting back, or it
    // becomes an open-redirect (phishing) vector.
    assert_eq!(safe_next("/dashboard"), Some("/dashboard".to_owned()));
    assert_eq!(safe_next("https://evil.example/x"), None); // absolute URL
    assert_eq!(safe_next("//evil.example/x"), None); // scheme-relative
    assert_eq!(safe_next("%2F%2Fevil.example/x"), None); // decodes to //evil
}
