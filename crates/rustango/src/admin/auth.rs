//! HTTP Basic Auth middleware for admin routes.
//!
//! A request passes when its `Authorization: Basic <base64>` header
//! decodes to the configured credentials. Otherwise it gets a 401 with
//! `WWW-Authenticate: Basic realm="Rustango Admin"`.
//!
//! The comparison is plain byte equality, **not** constant-time. That
//! is fine for an admin gate, but do not reuse this layer where a
//! timing side channel would matter.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, HeaderValue, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;
use base64::Engine;

#[derive(Clone)]
struct Creds {
    username: String,
    password: String,
}

fn extract_basic(header_value: &HeaderValue) -> Option<(String, String)> {
    let raw = header_value.to_str().ok()?.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(raw).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let mut parts = text.splitn(2, ':');
    let user = parts.next()?.to_owned();
    let pass = parts.next()?.to_owned();
    Some((user, pass))
}

async fn basic_auth_middleware(
    axum::extract::State(creds): axum::extract::State<Arc<Creds>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let ok = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(extract_basic)
        .is_some_and(|(u, p)| u == creds.username && p == creds.password);
    if ok {
        return next.run(request).await;
    }
    let mut resp = (StatusCode::UNAUTHORIZED, "Authentication required").into_response();
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static(r#"Basic realm="Rustango Admin""#),
    );
    resp
}

/// Wrap `router` so every request needs HTTP Basic Auth with these
/// credentials. The browser shows its own login dialog first.
pub fn protect_with_basic_auth(router: Router, username: &str, password: &str) -> Router {
    let creds = Arc::new(Creds {
        username: username.to_owned(),
        password: password.to_owned(),
    });
    router.layer(middleware::from_fn_with_state(creds, basic_auth_middleware))
}
