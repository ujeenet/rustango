//! HTTP Basic Auth middleware for admin routes.
//!
//! A request passes when its `Authorization: Basic <base64>` header
//! decodes to the configured credentials. Otherwise it gets a 401 with
//! `WWW-Authenticate: Basic realm="Rustango Admin"`.
//!
//! Each request reads the [`crate::login_throttle`] limits (admin scope)
//! and only a miss is counted; both fields are compared in constant time.

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
    use crate::login_throttle::{ClientIp, LoginScope};
    let Some((user, pass)) = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(extract_basic)
    else {
        return challenge();
    };
    let ip = ClientIp::from_parts(request.extensions(), request.headers());
    // Sent on every request, so a success reserves and counts nothing.
    let throttle = crate::login_throttle::shared();
    if let Err(refused) = throttle.check(&LoginScope::Admin, &ip, &user).await {
        return refused.into_response();
    }
    // Both compares always run, so the timing does not say which field is wrong.
    let user_ok = crate::crypto::constant_time_compare(user.as_bytes(), creds.username.as_bytes());
    let pass_ok = crate::crypto::constant_time_compare(pass.as_bytes(), creds.password.as_bytes());
    if user_ok & pass_ok {
        throttle
            .succeeded_quietly(&LoginScope::Admin, &ip, &user)
            .await;
        return next.run(request).await;
    }
    match throttle.begin(&LoginScope::Admin, &ip, &user).await {
        Ok(attempt) => attempt.failed().await,
        Err(refused) => return refused.into_response(),
    }
    challenge()
}

fn challenge() -> Response {
    let mut resp = (StatusCode::UNAUTHORIZED, "Authentication required").into_response();
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static(r#"Basic realm="Rustango Admin""#),
    );
    resp
}

/// Wrap `router` so every request needs HTTP Basic Auth with these
/// credentials. The browser shows its own login dialog first.
/// The browser resends them cross-site, so this adds [`protect_with_csrf`].
pub fn protect_with_basic_auth(router: Router, username: &str, password: &str) -> Router {
    let creds = Arc::new(Creds {
        username: username.to_owned(),
        password: password.to_owned(),
    });
    protect_with_csrf(router).layer(middleware::from_fn_with_state(creds, basic_auth_middleware))
}

/// CSRF on every admin mutation, with the token in every form. Wrap an
/// admin behind your own cookie auth in it; session auth has it built in (#2131).
pub fn protect_with_csrf(router: Router) -> Router {
    router
        .layer(middleware::from_fn(super::csrf_context::csrf_context))
        .layer(crate::forms::csrf::layer())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use tower::ServiceExt as _;

    async fn call(app: &Router, user_pass: &str) -> StatusCode {
        let creds = base64::engine::general_purpose::STANDARD.encode(user_pass);
        let req = Request::builder()
            .uri("/")
            .header(header::AUTHORIZATION, format!("Basic {creds}"))
            .body(Body::empty())
            .unwrap();
        app.clone().oneshot(req).await.unwrap().status()
    }

    /// A right Basic header is sent on every request: a repeat writes
    /// nothing to the login gate's cache; a wrong one is still counted.
    #[tokio::test]
    async fn right_basic_credentials_write_nothing_to_the_login_gate() {
        let counting =
            std::sync::Arc::new(crate::login_throttle::counting::CountingWrites::default());
        let cache: crate::cache::BoxedCache = counting.clone();
        let installed = crate::login_throttle::configure_shared(
            crate::login_throttle::LoginThrottle::with_cache(
                crate::login_throttle::LoginLimits::default(),
                cache,
            ),
        );
        // nextest runs each test in its own process; another gate won here.
        if !installed {
            return;
        }
        let app = protect_with_basic_auth(
            Router::new().route("/", get(|| async { "ok" })),
            "basic-quiet-admin",
            "right",
        );
        let writes = || counting.writes();
        assert_eq!(call(&app, "basic-quiet-admin:right").await, StatusCode::OK);
        let first = writes();
        for _ in 0..3 {
            assert_eq!(call(&app, "basic-quiet-admin:right").await, StatusCode::OK);
        }
        assert_eq!(writes(), first, "a repeat success wrote to the login gate");
        assert_eq!(
            call(&app, "basic-quiet-admin:wrong").await,
            StatusCode::UNAUTHORIZED
        );
        assert!(writes() > 0, "a miss was not counted");
    }

    /// Wrong passwords lock the account, so the right one is refused after.
    #[tokio::test]
    async fn repeated_wrong_passwords_lock_the_basic_gate() {
        let app = protect_with_basic_auth(
            Router::new().route("/", get(|| async { "ok" })),
            "basic-lock-admin",
            "right",
        );
        assert_eq!(call(&app, "basic-lock-admin:right").await, StatusCode::OK);
        for _ in 0..crate::account_lockout::DEFAULT_MAX_ATTEMPTS {
            assert_eq!(
                call(&app, "basic-lock-admin:wrong").await,
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(
            call(&app, "basic-lock-admin:right").await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
}
