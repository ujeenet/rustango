//! axum router for OAuth2 login + callback.
//!
//! Two routes per provider:
//! - `GET /auth/{tenant}/{provider}/login` — kicks off the flow,
//!   redirects to the provider's authorize URL.
//! - `GET /auth/{tenant}/{provider}/callback` — exchanges the code,
//!   fetches userinfo, calls your [`OnAuthSuccess`] hook.
//!
//! The router is **transport-agnostic for flow state**: it seals the
//! `OAuth2Flow` with HMAC and stuffs it into a cookie. No server-side
//! session needed, but works fine alongside one.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::oauth2::{providers, router::{oauth2_router, AuthSuccess}, OAuth2Registry};
//! use axum::response::{IntoResponse, Redirect};
//! use std::sync::Arc;
//!
//! let registry = OAuth2Registry::new();
//! registry.register("", providers::google(
//!     std::env::var("GOOGLE_CLIENT_ID").unwrap(),
//!     std::env::var("GOOGLE_CLIENT_SECRET").unwrap(),
//!     "https://app.example.com/auth//google/callback".to_owned(),
//! ));
//!
//! let app = axum::Router::new().merge(oauth2_router(
//!     registry,
//!     b"flow-signing-secret-keep-me-safe".to_vec(),
//!     true, // Secure flow cookie (HTTPS); use false only for local HTTP dev
//!     Arc::new(|login: AuthSuccess| Box::pin(async move {
//!         // Look up your user by `login.identity_key()`, never by email or
//!         // `sub` alone: a tenant's IdP vouches only for that tenant.
//!         tracing::info!(tenant = %login.tenant, "logged in");
//!         // Set your session cookie on this response.
//!         Ok(Redirect::to("/dashboard").into_response())
//!     })),
//! ));
//! ```
//!
//! For a single-tenant app, use `""` as the tenant in both the
//! registry key and the URL.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;

use super::{
    open_flow, seal_flow, FlowPurpose, FlowScope, NormalizedUser, OAuth2Registry, OAuthError,
    TokenResponse,
};

const FLOW_COOKIE: &str = "rustango_oauth_flow";
const INVALID_FLOW_COOKIE: &str = "invalid flow cookie — restart at /login";

/// Per-app callback. Receives the completed login and returns the response
/// to send the browser: typically it finds or creates the user, sets a
/// session cookie and redirects. The router adds its flow-cookie wipe.
pub type OnAuthSuccess = Arc<
    dyn Fn(AuthSuccess) -> Pin<Box<dyn Future<Output = Result<Response, AuthError>> + Send>>
        + Send
        + Sync,
>;

/// A completed login, as the [`OnAuthSuccess`] hook sees it (#1989).
/// No `Debug`: it carries the provider's tokens.
#[non_exhaustive]
pub struct AuthSuccess {
    /// The registry tenant whose provider vouched for `user` (`""` when
    /// single-tenant). Its IdP speaks for no other tenant. It is the URL's
    /// tenant, sealed in at login: check it is the host's tenant too.
    pub tenant: String,
    /// The identity the provider returned.
    pub user: NormalizedUser,
    /// The provider's token bag.
    pub tokens: TokenResponse,
}

impl AuthSuccess {
    /// Build one, e.g. to test a hook.
    #[must_use]
    pub fn new(tenant: impl Into<String>, user: NormalizedUser, tokens: TokenResponse) -> Self {
        Self {
            tenant: tenant.into(),
            user,
            tokens,
        }
    }

    /// `(tenant, provider, subject)`: the key to find a user by. An email
    /// or `sub` alone lets one tenant's IdP sign in as another's user.
    #[must_use]
    pub fn identity_key(&self) -> (&str, &str, &str) {
        (
            &self.tenant,
            &self.user.provider,
            &self.user.provider_user_id,
        )
    }
}

/// Application-side error from the [`OnAuthSuccess`] hook. It is logged and
/// the `502 Bad Gateway` body is generic, unless `RUSTANGO_DISCLOSE_ERRORS`
/// is on: a `?` on a database error would otherwise show the driver text.
#[derive(Debug)]
pub struct AuthError(pub String);

impl<E: std::fmt::Display> From<E> for AuthError {
    fn from(e: E) -> Self {
        Self(e.to_string())
    }
}

#[derive(Clone)]
struct RouterState {
    registry: OAuth2Registry,
    flow_secret: Arc<Vec<u8>>,
    on_success: OnAuthSuccess,
    /// Audit H2 — when true, the flow cookie (which carries the PKCE
    /// verifier + CSRF `state`) is marked `Secure` so it's only sent
    /// over HTTPS. Set `false` only for local plain-HTTP dev.
    secure: bool,
}

/// Build the router.
///
/// `flow_secret` signs the per-flow cookie — keep it stable and out of
/// source. 32+ bytes from a CSPRNG is plenty.
///
/// `secure` marks the flow cookie `Secure` (HTTPS-only). Pass `true`
/// in production; `false` only for local plain-HTTP development.
#[must_use]
pub fn oauth2_router(
    registry: OAuth2Registry,
    flow_secret: Vec<u8>,
    secure: bool,
    on_success: OnAuthSuccess,
) -> Router {
    let state = RouterState {
        registry,
        flow_secret: Arc::new(flow_secret),
        on_success,
        secure,
    };
    Router::new()
        .route("/auth/{tenant}/{provider}/login", get(login_handler))
        .route("/auth/{tenant}/{provider}/callback", get(callback_handler))
        .with_state(state)
}

#[derive(Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// A callback admitted by the SSO throttle (needs `admin`; else nothing).
#[cfg(feature = "admin")]
type Attempt = crate::login_throttle::LoginAttempt;
#[cfg(not(feature = "admin"))]
type Attempt = ();

/// This router's own SSO bucket per tenant.
#[cfg(feature = "admin")]
fn throttle_scope(tenant: &str) -> crate::login_throttle::LoginScope {
    crate::login_throttle::LoginScope::Sso(format!("app/{tenant}"))
}

/// Refuse a start from an IP whose SSO failures are spent.
#[allow(clippy::unused_async)]
async fn throttle_check(
    tenant: &str,
    headers: &HeaderMap,
    extensions: &axum::http::Extensions,
) -> Result<(), Response> {
    #[cfg(feature = "admin")]
    {
        let ip = crate::login_throttle::ClientIp::from_parts(extensions, headers);
        crate::login_throttle::shared()
            .check(&throttle_scope(tenant), &ip, "")
            .await
            .map_err(|r| r.into_response())
    }
    #[cfg(not(feature = "admin"))]
    {
        let _ = (tenant, headers, extensions);
        Ok(())
    }
}

/// Admit one callback.
#[allow(clippy::unused_async)]
async fn throttle_begin(
    tenant: &str,
    headers: &HeaderMap,
    extensions: &axum::http::Extensions,
) -> Result<Attempt, Response> {
    #[cfg(feature = "admin")]
    {
        let ip = crate::login_throttle::ClientIp::from_parts(extensions, headers);
        crate::login_throttle::shared()
            .begin(&throttle_scope(tenant), &ip, "")
            .await
            .map_err(|r| r.into_response())
    }
    #[cfg(not(feature = "admin"))]
    {
        let _ = (tenant, headers, extensions);
        Ok(())
    }
}

/// Report a callback's outcome: a failure counts against the IP.
#[allow(clippy::unused_async)]
async fn report(attempt: Attempt, ok: bool) {
    #[cfg(feature = "admin")]
    if ok {
        attempt.succeeded().await;
    } else {
        attempt.failed().await;
    }
    #[cfg(not(feature = "admin"))]
    let _ = (attempt, ok);
}

async fn login_handler(
    State(state): State<RouterState>,
    Path((tenant, provider_name)): Path<(String, String)>,
    headers: HeaderMap,
    extensions: axum::http::Extensions,
) -> Response {
    if let Err(refused) = throttle_check(&tenant, &headers, &extensions).await {
        return refused;
    }
    let Some(provider) = state.registry.get(&tenant, &provider_name) else {
        return (StatusCode::NOT_FOUND, "unknown provider").into_response();
    };

    let (auth_url, flow) = provider.begin();
    let sealed = seal_flow(
        &flow,
        FlowScope::new(FlowPurpose::OAuth2, &tenant, &provider_name),
        &state.flow_secret,
    );
    let secure = if state.secure { "; Secure" } else { "" };
    // 5-minute window — if the user takes longer to log in we issue a fresh flow.
    let cookie =
        format!("{FLOW_COOKIE}={sealed}; Path=/; HttpOnly; SameSite=Lax; Max-Age=300{secure}");
    // Provider config can yield bytes a header rejects: a 500, not a panic (#1541).
    let (Ok(cookie), Ok(location)) = (cookie.parse(), auth_url.parse()) else {
        tracing::error!(target: "rustango::error", provider = %provider_name, "oauth2 login: authorize URL or cookie is not a valid header value");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let mut headers = HeaderMap::new();
    headers.insert(header::SET_COOKIE, cookie);
    headers.insert(header::LOCATION, location);
    (StatusCode::SEE_OTHER, headers).into_response()
}

async fn callback_handler(
    State(state): State<RouterState>,
    Path((tenant, provider_name)): Path<(String, String)>,
    Query(params): Query<CallbackParams>,
    headers: HeaderMap,
    extensions: axum::http::Extensions,
) -> Response {
    let attempt = match throttle_begin(&tenant, &headers, &extensions).await {
        Ok(a) => a,
        Err(refused) => return refused,
    };
    if let Some(err) = params.error.as_deref() {
        // Audit L3 — the `error` / `error_description` query params are
        // attacker-influenceable (anyone can craft a callback URL). Don't
        // reflect them into the response body; log server-side and return
        // a fixed generic message instead.
        tracing::warn!(
            provider = %provider_name,
            error = %err,
            error_description = params.error_description.as_deref().unwrap_or(""),
            "oauth2 provider returned an error on callback",
        );
        return (
            StatusCode::BAD_REQUEST,
            "authentication failed at the identity provider",
        )
            .into_response();
    }
    let Some(code) = params.code else {
        return (StatusCode::BAD_REQUEST, "missing `code` query param").into_response();
    };
    let Some(callback_state) = params.state else {
        return (StatusCode::BAD_REQUEST, "missing `state` query param").into_response();
    };
    let Some(provider) = state.registry.get(&tenant, &provider_name) else {
        return (StatusCode::NOT_FOUND, "unknown provider").into_response();
    };
    let Some(sealed) = crate::cookies::cookie_from_headers(&headers, FLOW_COOKIE) else {
        return (
            StatusCode::BAD_REQUEST,
            "missing flow cookie — start at /login",
        )
            .into_response();
    };
    let flow = match open_flow(
        sealed,
        FlowScope::new(FlowPurpose::OAuth2, &tenant, &provider_name),
        &state.flow_secret,
    ) {
        Ok(f) => f,
        Err(e) => {
            // The reason helps a forger more than the user (#2087).
            tracing::warn!(error = %e, provider = %provider_name, "oauth2 callback: bad flow cookie");
            return (StatusCode::BAD_REQUEST, INVALID_FLOW_COOKIE).into_response();
        }
    };

    let (user, tokens) = match provider.complete(&flow, &code, &callback_state).await {
        Ok(out) => out,
        Err(OAuthError::StateMismatch) => {
            return (StatusCode::BAD_REQUEST, "CSRF state mismatch").into_response()
        }
        // Audit N7b — a stale flow is a client/timeout condition, not an
        // upstream-IdP failure: return 400 (like StateMismatch), not the
        // catch-all 502 below.
        Err(OAuthError::FlowExpired) => {
            return (
                StatusCode::BAD_REQUEST,
                "login flow expired — restart at /login",
            )
                .into_response()
        }
        Err(e) => {
            // `e` can carry the IdP body or upstream addresses: log it only (#1847).
            tracing::warn!(error = %e, provider = %provider_name, "oauth2 callback failed");
            report(attempt, false).await;
            return (
                StatusCode::BAD_GATEWAY,
                "authentication failed at the identity provider",
            )
                .into_response();
        }
    };

    finish(
        &state,
        AuthSuccess {
            tenant,
            user,
            tokens,
        },
        attempt,
    )
    .await
}

/// Run the hook, then wipe the flow cookie next to any cookie it set. A
/// hook refusal counts against the client's SSO throttle.
async fn finish(state: &RouterState, login: AuthSuccess, attempt: Attempt) -> Response {
    let outcome = (state.on_success)(login).await;
    report(attempt, outcome.is_ok()).await;
    match outcome {
        Ok(mut resp) => {
            let secure = if state.secure { "; Secure" } else { "" };
            // `append`: the hook's own session cookie must survive.
            resp.headers_mut().append(
                header::SET_COOKIE,
                format!("{FLOW_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{secure}")
                    .parse()
                    .expect("valid clear cookie"),
            );
            resp
        }
        Err(AuthError(msg)) => hook_error(&msg).into_response(),
    }
}

/// The 502 for a failed hook: logged, and its text withheld from the client.
fn hook_error(msg: &str) -> (StatusCode, String) {
    (
        StatusCode::BAD_GATEWAY,
        crate::error::server_error_body("oauth2::on_success", &msg),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth2::providers;
    use axum::body::Body;
    use axum::http::Request;
    use axum::response::Redirect;
    use tower::ServiceExt;

    fn dummy_success() -> OnAuthSuccess {
        Arc::new(|_login| Box::pin(async { Ok(Redirect::to("/").into_response()) }))
    }

    /// #1989 — the hook learns the tenant, and its `Set-Cookie` survives
    /// the router's flow-cookie wipe.
    #[tokio::test]
    async fn the_hook_sees_the_tenant_and_keeps_its_cookie() {
        let hook: OnAuthSuccess = Arc::new(|login: AuthSuccess| {
            Box::pin(async move {
                let (tenant, provider, sub) = login.identity_key();
                let mut resp = Redirect::to("/").into_response();
                resp.headers_mut().insert(
                    header::SET_COOKIE,
                    format!("session={tenant}|{provider}|{sub}")
                        .parse()
                        .unwrap(),
                );
                Ok(resp)
            })
        });
        let state = RouterState {
            registry: OAuth2Registry::new(),
            flow_secret: Arc::new(b"signing".to_vec()),
            on_success: hook,
            secure: true,
        };
        let user = NormalizedUser {
            provider: "google".into(),
            provider_user_id: "42".into(),
            email: None,
            email_verified: false,
            name: None,
            avatar_url: None,
            raw: serde_json::Value::Null,
        };
        let tokens: TokenResponse =
            serde_json::from_value(serde_json::json!({"access_token": "at"})).unwrap();
        let login = AuthSuccess::new("acme", user, tokens);
        let resp = finish(
            &state,
            login,
            throttle_begin("acme", &HeaderMap::new(), &Default::default())
                .await
                .unwrap(),
        )
        .await;
        let cookies: Vec<_> = resp
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap().to_owned())
            .collect();
        assert!(
            cookies.contains(&"session=acme|google|42".to_owned()),
            "{cookies:?}"
        );
        assert!(
            cookies
                .iter()
                .any(|c| c.starts_with(&format!("{FLOW_COOKIE}=;"))),
            "{cookies:?}"
        );
    }

    /// A hook's `?` on a driver error does not reach the browser.
    #[test]
    fn a_hook_error_is_logged_not_sent() {
        let _g = crate::error::test_env::lock();
        crate::error::test_env::with(crate::error::DISCLOSE_ENV, None, || {
            let err = AuthError::from("duplicate key violates rustango_users_email_key");
            let (status, body) = hook_error(&err.0);
            assert_eq!(status, StatusCode::BAD_GATEWAY);
            assert!(!body.contains("rustango_users"), "{body}");
        });
    }

    #[tokio::test]
    async fn login_route_redirects_with_cookie_and_location() {
        let registry = OAuth2Registry::new();
        registry.register("acme", providers::google("cid", "csec", "https://app/cb"));
        let app = oauth2_router(registry, b"signing".to_vec(), true, dummy_success());

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
        assert!(loc.contains("accounts.google.com"));
        let cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(cookie.starts_with(&format!("{FLOW_COOKIE}=")));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        // Audit H2 — flow cookie is Secure when the router is built with
        // `secure = true` (carries the PKCE verifier + CSRF state).
        assert!(
            cookie.contains("; Secure"),
            "flow cookie must be Secure: {cookie}"
        );
    }

    #[tokio::test]
    async fn login_unknown_provider_returns_404() {
        let registry = OAuth2Registry::new();
        let app = oauth2_router(registry, b"signing".to_vec(), true, dummy_success());
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

    #[tokio::test]
    async fn callback_provider_error_returns_generic_400_without_reflection() {
        // Audit L3 — the provider error params must NOT be echoed into
        // the response body (attacker-influenceable); a fixed generic
        // message is returned instead.
        let registry = OAuth2Registry::new();
        registry.register("acme", providers::google("cid", "csec", "https://app/cb"));
        let app = oauth2_router(registry, b"signing".to_vec(), true, dummy_success());

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/auth/acme/google/callback?error=access_denied&error_description=user_cancelled")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(
            !body.contains("access_denied") && !body.contains("user_cancelled"),
            "provider error params must not be reflected: {body}"
        );
        assert!(body.contains("authentication failed"));
    }

    #[tokio::test]
    async fn callback_without_cookie_rejects() {
        let registry = OAuth2Registry::new();
        registry.register("acme", providers::google("cid", "csec", "https://app/cb"));
        let app = oauth2_router(registry, b"signing".to_vec(), true, dummy_success());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/auth/acme/google/callback?code=abc&state=xyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        assert!(std::str::from_utf8(&body).unwrap().contains("flow cookie"));
    }

    #[tokio::test]
    async fn callback_with_tampered_cookie_rejects() {
        let registry = OAuth2Registry::new();
        registry.register("acme", providers::google("cid", "csec", "https://app/cb"));
        let app = oauth2_router(registry, b"signing".to_vec(), true, dummy_success());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/auth/acme/google/callback?code=abc&state=xyz")
                    .header(header::COOKIE, format!("{FLOW_COOKIE}=garbage.value"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        // A reader that never finds the cookie says "missing" instead;
        // the open error's text is not echoed (#2087).
        assert_eq!(std::str::from_utf8(&body).unwrap(), INVALID_FLOW_COOKIE);
    }

    /// #1847 — an upstream failure's text (here a blocked address) stays out of the 502.
    #[tokio::test]
    async fn callback_upstream_error_is_not_echoed() {
        let registry = OAuth2Registry::new();
        let mut p = providers::google("cid", "csec", "https://app/cb");
        p.token_url = "https://10.9.8.7/token".into();
        registry.register("acme", p);
        let app = oauth2_router(registry, b"signing".to_vec(), true, dummy_success());
        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/auth/acme/google/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let loc = login
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        let state = loc
            .split("state=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap();
        let set = login.headers().get(header::SET_COOKIE).unwrap();
        let pair = set.to_str().unwrap().split(';').next().unwrap().to_owned();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/auth/acme/google/callback?code=abc&state={state}"))
                    .header(header::COOKIE, pair)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert_eq!(body, "authentication failed at the identity provider");
    }

    /// #1992 — a flow begun at one tenant's provider is refused at another's callback.
    #[tokio::test]
    async fn callback_refuses_a_flow_begun_elsewhere() {
        let registry = OAuth2Registry::new();
        for (tenant, name) in [("acme", "google"), ("globex", "google"), ("acme", "okta")] {
            let mut p = providers::google("cid", "csec", "https://app/cb");
            p.name = name.into();
            // Blocked, so getting past the cookie fails fast with a 502.
            p.token_url = "https://10.9.8.7/token".into();
            registry.register(tenant, p);
        }
        let app = oauth2_router(registry, b"signing".to_vec(), true, dummy_success());
        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/auth/acme/google/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let loc = login.headers()[header::LOCATION].to_str().unwrap();
        let state = loc
            .split("state=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap();
        let set = login.headers()[header::SET_COOKIE].to_str().unwrap();
        let pair = set.split(';').next().unwrap().to_owned();
        for path in ["globex/google", "acme/okta"] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/auth/{path}/callback?code=abc&state={state}"))
                        .header(header::COOKIE, pair.clone())
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{path}");
        }
    }

    /// The cookie set by `/login` is read back and opened on callback: a
    /// wrong `state` then fails the CSRF check, not the cookie lookup.
    #[tokio::test]
    async fn callback_reads_the_flow_cookie_from_login() {
        let registry = OAuth2Registry::new();
        registry.register("acme", providers::google("cid", "csec", "https://app/cb"));
        let app = oauth2_router(registry, b"signing".to_vec(), true, dummy_success());
        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/auth/acme/google/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let set = login.headers().get(header::SET_COOKIE).unwrap();
        let pair = set.to_str().unwrap().split(';').next().unwrap().to_owned();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/auth/acme/google/callback?code=abc&state=not-the-state")
                    .header(header::COOKIE, format!("other=1; {pair}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "CSRF state mismatch");
    }
}
