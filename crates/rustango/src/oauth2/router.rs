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
//! use rustango::oauth2::{providers, router::oauth2_router, OAuth2Registry};
//! use axum::response::Redirect;
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
//!     Arc::new(|user, _tokens| Box::pin(async move {
//!         // Persist or look up your user record by user.email / user.provider_user_id
//!         tracing::info!(email = ?user.email, "logged in");
//!         Ok(Redirect::to("/dashboard"))
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
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;

use super::{open_flow, seal_flow, NormalizedUser, OAuth2Registry, OAuthError, TokenResponse};

const FLOW_COOKIE: &str = "rustango_oauth_flow";

/// Per-app callback. Receives the resolved user + token bag and returns
/// the response to send the browser. Typical implementations look up or
/// create a user record, set a session cookie, and redirect to a UI route.
pub type OnAuthSuccess = Arc<
    dyn Fn(
            NormalizedUser,
            TokenResponse,
        ) -> Pin<Box<dyn Future<Output = Result<Redirect, AuthError>> + Send>>
        + Send
        + Sync,
>;

/// Application-side error from the [`OnAuthSuccess`] hook. Whatever is
/// `Display`-able will be returned in the `502 Bad Gateway` body —
/// keep it user-safe.
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
}

/// Build the router.
///
/// `flow_secret` signs the per-flow cookie — keep it stable and out of
/// source. 32+ bytes from a CSPRNG is plenty.
#[must_use]
pub fn oauth2_router(
    registry: OAuth2Registry,
    flow_secret: Vec<u8>,
    on_success: OnAuthSuccess,
) -> Router {
    let state = RouterState {
        registry,
        flow_secret: Arc::new(flow_secret),
        on_success,
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

async fn login_handler(
    State(state): State<RouterState>,
    Path((tenant, provider_name)): Path<(String, String)>,
) -> Response {
    let Some(provider) = state.registry.get(&tenant, &provider_name) else {
        return (StatusCode::NOT_FOUND, "unknown provider").into_response();
    };

    let (auth_url, flow) = provider.begin();
    let sealed = seal_flow(&flow, &state.flow_secret);
    // 5-minute window — if the user takes longer to log in we issue a fresh flow.
    let cookie = format!(
        "{FLOW_COOKIE}={sealed}; Path=/; HttpOnly; SameSite=Lax; Max-Age=300"
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        cookie.parse().expect("valid cookie header"),
    );
    headers.insert(
        header::LOCATION,
        auth_url.parse().expect("valid location header"),
    );
    (StatusCode::SEE_OTHER, headers).into_response()
}

async fn callback_handler(
    State(state): State<RouterState>,
    Path((tenant, provider_name)): Path<(String, String)>,
    Query(params): Query<CallbackParams>,
    headers: HeaderMap,
) -> Response {
    if let Some(err) = params.error.as_deref() {
        let desc = params.error_description.as_deref().unwrap_or("");
        return (
            StatusCode::BAD_REQUEST,
            format!("provider returned error: {err} {desc}"),
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
    let Some(sealed) = read_cookie(&headers, FLOW_COOKIE) else {
        return (
            StatusCode::BAD_REQUEST,
            "missing flow cookie — start at /login",
        )
            .into_response();
    };
    let flow = match open_flow(&sealed, &state.flow_secret) {
        Ok(f) => f,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("invalid flow cookie: {e}"))
                .into_response()
        }
    };

    let (user, tokens) = match provider.complete(&flow, &code, &callback_state).await {
        Ok(out) => out,
        Err(OAuthError::StateMismatch) => {
            return (StatusCode::BAD_REQUEST, "CSRF state mismatch").into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, provider = %provider_name, "oauth2 callback failed");
            return (StatusCode::BAD_GATEWAY, format!("auth failed: {e}")).into_response();
        }
    };

    match (state.on_success)(user, tokens).await {
        Ok(redirect) => {
            // Wipe the flow cookie on the way out.
            let mut resp = redirect.into_response();
            resp.headers_mut().insert(
                header::SET_COOKIE,
                format!("{FLOW_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
                    .parse()
                    .expect("valid clear cookie"),
            );
            resp
        }
        Err(AuthError(msg)) => (StatusCode::BAD_GATEWAY, msg).into_response(),
    }
}

fn read_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let cookie_header = headers.get(header::COOKIE)?.to_str().ok()?;
    for kv in cookie_header.split(';') {
        let kv = kv.trim();
        if let Some(rest) = kv.strip_prefix(&format!("{name}=")) {
            return Some(rest.to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth2::providers;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn dummy_success() -> OnAuthSuccess {
        Arc::new(|_user, _tokens| Box::pin(async { Ok(Redirect::to("/")) }))
    }

    #[tokio::test]
    async fn login_route_redirects_with_cookie_and_location() {
        let registry = OAuth2Registry::new();
        registry.register(
            "acme",
            providers::google("cid", "csec", "https://app/cb"),
        );
        let app = oauth2_router(registry, b"signing".to_vec(), dummy_success());

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
        let loc = resp.headers().get(header::LOCATION).unwrap().to_str().unwrap();
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
    }

    #[tokio::test]
    async fn login_unknown_provider_returns_404() {
        let registry = OAuth2Registry::new();
        let app = oauth2_router(registry, b"signing".to_vec(), dummy_success());
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
    async fn callback_propagates_provider_error_param() {
        let registry = OAuth2Registry::new();
        registry.register(
            "acme",
            providers::google("cid", "csec", "https://app/cb"),
        );
        let app = oauth2_router(registry, b"signing".to_vec(), dummy_success());

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
        let body = axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains("access_denied"));
    }

    #[tokio::test]
    async fn callback_without_cookie_rejects() {
        let registry = OAuth2Registry::new();
        registry.register(
            "acme",
            providers::google("cid", "csec", "https://app/cb"),
        );
        let app = oauth2_router(registry, b"signing".to_vec(), dummy_success());
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
        let body = axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        assert!(std::str::from_utf8(&body).unwrap().contains("flow cookie"));
    }

    #[tokio::test]
    async fn callback_with_tampered_cookie_rejects() {
        let registry = OAuth2Registry::new();
        registry.register(
            "acme",
            providers::google("cid", "csec", "https://app/cb"),
        );
        let app = oauth2_router(registry, b"signing".to_vec(), dummy_success());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/auth/acme/google/callback?code=abc&state=xyz")
                    .header(
                        header::COOKIE,
                        format!("{FLOW_COOKIE}=garbage.value"),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn read_cookie_extracts_named_value() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            "session=abc; rustango_oauth_flow=xyz; theme=dark".parse().unwrap(),
        );
        assert_eq!(
            read_cookie(&headers, FLOW_COOKIE).as_deref(),
            Some("xyz")
        );
        assert!(read_cookie(&headers, "missing").is_none());
    }
}
