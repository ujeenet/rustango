//! HTTP Basic Auth middleware for admin routes.
//!
//! A request passes when its `Authorization: Basic <base64>` header
//! decodes to the configured credentials. Otherwise it gets a 401 with
//! `WWW-Authenticate: Basic realm="Rustango Admin"`.
//!
//! Each request is admitted by [`crate::login_throttle`] (admin scope)
//! before the compare, and only a miss is counted; both fields are
//! compared in constant time.

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
    // Admitted before the compare; a right one from a known client writes nothing.
    let attempt = match crate::login_throttle::shared()
        .begin_per_request(&LoginScope::Admin, &ip, &user)
        .await
    {
        Ok(attempt) => attempt,
        Err(refused) => return refused.into_response(),
    };
    // Both compares always run, so the timing does not say which field is wrong.
    let user_ok = crate::crypto::constant_time_compare(user.as_bytes(), creds.username.as_bytes());
    let pass_ok = crate::crypto::constant_time_compare(pass.as_bytes(), creds.password.as_bytes());
    let right = user_ok & pass_ok;
    if let Err(refused) = attempt.settle(right).await {
        return refused.into_response();
    }
    if right {
        return next.run(request).await;
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

    /// [`call`] from one client address.
    async fn call_from(app: &Router, user_pass: &str) -> StatusCode {
        let creds = base64::engine::general_purpose::STANDARD.encode(user_pass);
        let req = Request::builder()
            .uri("/")
            .header(header::AUTHORIZATION, format!("Basic {creds}"))
            .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [10, 65, 0, 1],
                1,
            ))))
            .body(Body::empty())
            .unwrap();
        app.clone().oneshot(req).await.unwrap().status()
    }

    /// Parallel guesses through a slow shared cache, from a client that
    /// signed in before: the account lock still bounds the burst, so the
    /// right password sent last is refused.
    #[tokio::test]
    async fn a_parallel_burst_of_basic_guesses_is_bounded_by_the_lock() {
        let slow = std::sync::Arc::new(
            crate::login_throttle::counting::CountingWrites::with_read_delay(
                std::time::Duration::from_millis(100),
            ),
        );
        let installed = crate::login_throttle::configure_shared(
            crate::login_throttle::LoginThrottle::with_cache(
                crate::login_throttle::LoginLimits::default(),
                slow,
            ),
        );
        // nextest runs each test in its own process; another gate won here.
        if !installed {
            return;
        }
        let app = protect_with_basic_auth(
            Router::new().route("/", get(|| async { "ok" })),
            "burst-admin",
            "s3cret",
        );
        assert_eq!(call_from(&app, "burst-admin:s3cret").await, StatusCode::OK);
        let mut guesses: Vec<_> = (0..99)
            .map(|i| {
                let app = app.clone();
                tokio::spawn(async move { call_from(&app, &format!("burst-admin:guess{i}")).await })
            })
            .collect();
        // The right one arrives while the wrong ones are in flight.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let right = {
            let app = app.clone();
            tokio::spawn(async move { call_from(&app, "burst-admin:s3cret").await })
        };
        let mut misses = 0;
        for g in guesses.drain(..) {
            let status = g.await.unwrap();
            assert_ne!(status, StatusCode::OK);
            misses += usize::from(status == StatusCode::UNAUTHORIZED);
        }
        let max = crate::account_lockout::DEFAULT_MAX_ATTEMPTS as usize;
        assert!(misses <= max, "{misses} wrong guesses were checked");
        assert_eq!(right.await.unwrap(), StatusCode::TOO_MANY_REQUESTS);
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
        assert_eq!(
            call_from(&app, "basic-quiet-admin:right").await,
            StatusCode::OK
        );
        let first = writes();
        for _ in 0..3 {
            assert_eq!(
                call_from(&app, "basic-quiet-admin:right").await,
                StatusCode::OK
            );
        }
        assert_eq!(writes(), first, "a repeat success wrote to the login gate");
        assert_eq!(
            call_from(&app, "basic-quiet-admin:wrong").await,
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
