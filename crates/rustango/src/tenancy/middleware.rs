//! Axum authentication middlewares and extractors.
//!
//! # Usage
//!
//! ```ignore
//! use rustango::tenancy::auth_backends::{ModelBackend, ApiKeyBackend, AuthBackend};
//! use rustango::tenancy::middleware::{RouterAuthExt, CurrentUser};
//! use std::sync::Arc;
//!
//! let backends: Vec<Arc<dyn AuthBackend>> = vec![
//!     Arc::new(ModelBackend),
//!     Arc::new(ApiKeyBackend),
//! ];
//!
//! let app = Router::new()
//!     .route("/profile", get(profile))
//!     .route("/posts/new", post(create_post))
//!         .require_perm("post.add", pool.clone())   // inner — checked after auth
//!     .require_auth(backends, pool.clone());          // outer — checked first
//!
//! async fn profile(CurrentUser(user): CurrentUser) -> impl IntoResponse {
//!     match user {
//!         Some(u) => format!("hello {}", u.username).into_response(),
//!         None    => StatusCode::UNAUTHORIZED.into_response(),
//!     }
//! }
//! ```

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Router;

use crate::sql::Pool;

use super::auth_backends::{AuthError, AuthUser, BoxedBackend};
use super::permissions;

// ------------------------------------------------------------------ AuthenticatedUser

/// The resolved identity injected into request extensions by
/// [`RouterAuthExt::require_auth`]. Consume via the [`CurrentUser`]
/// extractor.
#[derive(Debug, Clone)]
pub struct AuthenticatedUser {
    pub id: i64,
    pub username: String,
    pub is_superuser: bool,
}

impl From<AuthUser> for AuthenticatedUser {
    fn from(u: AuthUser) -> Self {
        Self {
            id: u.id,
            username: u.username,
            is_superuser: u.is_superuser,
        }
    }
}

// ------------------------------------------------------------------ CurrentUser extractor

/// Axum extractor that reads the [`AuthenticatedUser`] injected by
/// [`RouterAuthExt::require_auth`]. Returns `None` for anonymous requests
/// (when the middleware is not in the stack).
///
/// ```ignore
/// async fn handler(CurrentUser(user): CurrentUser) -> impl IntoResponse {
///     match user {
///         Some(u) => format!("hello {}", u.username).into_response(),
///         None    => StatusCode::UNAUTHORIZED.into_response(),
///     }
/// }
/// ```
pub struct CurrentUser(pub Option<AuthenticatedUser>);

impl<S: Send + Sync> FromRequestParts<S> for CurrentUser {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(CurrentUser(
            parts.extensions.get::<AuthenticatedUser>().cloned(),
        ))
    }
}

// ------------------------------------------------------------------ Internal states

#[derive(Clone)]
struct AuthState {
    backends: Arc<Vec<BoxedBackend>>,
    pool: Pool,
    required: bool, // false = optional_auth
}

#[derive(Clone)]
struct PermState {
    codename: &'static str,
    pool: Pool,
}

// ------------------------------------------------------------------ Middleware handlers

async fn auth_middleware(
    State(state): State<AuthState>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    // Build a minimal Parts from this request's headers for backends.
    let headers = req.headers().clone();
    let uri = req.uri().clone();
    let method = req.method().clone();
    let mut builder = axum::http::Request::builder().method(&method).uri(&uri);
    for (k, v) in &headers {
        builder = builder.header(k, v);
    }
    let dummy = builder
        .body(())
        .unwrap_or_else(|_| axum::http::Request::new(()));
    let (dummy_parts, _) = dummy.into_parts();

    let mut authenticated: Option<AuthUser> = None;
    let mut error_response: Option<Response> = None;

    for backend in state.backends.iter() {
        match backend.authenticate(&dummy_parts, &state.pool).await {
            Ok(Some(user)) => {
                authenticated = Some(user);
                break;
            }
            Ok(None) => {}
            Err(AuthError::Inactive) => {
                error_response = Some((StatusCode::FORBIDDEN, "account inactive").into_response());
                break;
            }
            Err(e) => {
                error_response = Some((StatusCode::UNAUTHORIZED, e.to_string()).into_response());
                break;
            }
        }
    }

    if let Some(resp) = error_response {
        return resp;
    }

    match authenticated {
        Some(user) => {
            req.extensions_mut().insert(AuthenticatedUser::from(user));
            next.run(req).await
        }
        None if state.required => {
            (StatusCode::UNAUTHORIZED, "authentication required").into_response()
        }
        None => next.run(req).await,
    }
}

async fn perm_middleware(
    State(state): State<PermState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let user = req.extensions().get::<AuthenticatedUser>().cloned();
    let Some(user) = user else {
        return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
    };
    let ok = permissions::has_perm_pool(user.id, state.codename, &state.pool)
        .await
        .unwrap_or(false);
    if !ok {
        return (
            StatusCode::FORBIDDEN,
            format!("permission required: {}", state.codename),
        )
            .into_response();
    }
    next.run(req).await
}

// ------------------------------------------------------------------ RouterAuthExt

/// Extension trait that adds auth middleware to an axum `Router`.
///
/// Call order matters: outer layer runs first. The usual pattern:
///
/// ```text
/// .require_perm("post.add", pool)   // inner — runs after auth
/// .require_auth(backends, pool)      // outer — runs first
/// ```
pub trait RouterAuthExt<S> {
    /// Require a valid identity for all routes in this router. Injects
    /// [`AuthenticatedUser`] into extensions on success; returns 401 on failure.
    fn require_auth(self, backends: Vec<BoxedBackend>, pool: Pool) -> Self;

    /// Like [`require_auth`] but does NOT return 401 for anonymous requests.
    /// Useful for routes that serve both authenticated and anonymous users.
    fn optional_auth(self, backends: Vec<BoxedBackend>, pool: Pool) -> Self;

    /// Require `codename` permission on the already-resolved
    /// [`AuthenticatedUser`]. Must be placed inside (closer to handlers
    /// than) a `require_auth` layer.
    fn require_perm(self, codename: &'static str, pool: Pool) -> Self;
}

impl<S: Clone + Send + Sync + 'static> RouterAuthExt<S> for Router<S> {
    fn require_auth(self, backends: Vec<BoxedBackend>, pool: Pool) -> Self {
        let state = AuthState {
            backends: Arc::new(backends),
            pool,
            required: true,
        };
        self.layer(axum::middleware::from_fn_with_state(state, auth_middleware))
    }

    fn optional_auth(self, backends: Vec<BoxedBackend>, pool: Pool) -> Self {
        let state = AuthState {
            backends: Arc::new(backends),
            pool,
            required: false,
        };
        self.layer(axum::middleware::from_fn_with_state(state, auth_middleware))
    }

    fn require_perm(self, codename: &'static str, pool: Pool) -> Self {
        let state = PermState { codename, pool };
        self.layer(axum::middleware::from_fn_with_state(state, perm_middleware))
    }
}
