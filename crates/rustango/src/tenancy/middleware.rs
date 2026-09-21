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
//!         .require_perm("post.add")   // inner — checked after auth
//!     .require_auth(backends);        // outer — checked first
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

// ------------------------------------------------------------------ Tenant pool resolution

/// The pool this request's credential must be checked against — the
/// tenant resolved from **this request's** host, never one captured
/// when the router was built.
///
/// Erased over the backend because the two context types are generic
/// and this middleware is not; whichever the app mounted answers.
///
/// **Fails closed.** No context, no tenant, or a resolver error all
/// reject — an unresolvable tenant is exactly the case where guessing
/// is what the vulnerability was.
async fn tenant_pool(parts: &Parts, ext: &axum::http::Extensions) -> Result<Pool, Response> {
    use crate::extractors::{DatabaseTenantContext, TenantContext};
    use crate::tenancy::resolver::OrgResolver as _;

    macro_rules! try_ctx {
        ($db:ty) => {
            if let Some(ctx) = ext.get::<Arc<TenantContext<$db>>>() {
                let org = ctx
                    .resolver
                    .resolve(parts, &ctx.pools.registry_pool())
                    .await
                    .map_err(|e| {
                        tracing::error!(error = %e, "require_auth: tenant resolution failed");
                        (StatusCode::INTERNAL_SERVER_ERROR, "tenant resolution failed")
                            .into_response()
                    })?
                    .ok_or_else(|| {
                        (StatusCode::NOT_FOUND, "unknown tenant").into_response()
                    })?;
                return ctx.pools.scoped_pool_dyn(&org).await.map_err(|e| {
                    tracing::error!(error = %e, slug = %org.slug, "require_auth: tenant pool failed");
                    (StatusCode::INTERNAL_SERVER_ERROR, "tenant pool unavailable").into_response()
                });
            }
        };
    }

    // The pure-SQLite / MySQL stack mounts `DatabaseTenantContext`
    // instead, with its own registry handle and no schema mode. Missing
    // it out would fail those deployments closed — safe, but broken.
    macro_rules! try_db_ctx {
        ($db:ty, $variant:path) => {
            if let Some(ctx) = ext.get::<Arc<DatabaseTenantContext<$db>>>() {
                let org = ctx
                    .resolver
                    .resolve(parts, &ctx.registry)
                    .await
                    .map_err(|e| {
                        tracing::error!(error = %e, "require_auth: tenant resolution failed");
                        (StatusCode::INTERNAL_SERVER_ERROR, "tenant resolution failed")
                            .into_response()
                    })?
                    .ok_or_else(|| {
                        (StatusCode::NOT_FOUND, "unknown tenant").into_response()
                    })?;
                let dbp = ctx.pools.pool_for_org(&org).await.map_err(|e| {
                    tracing::error!(error = %e, slug = %org.slug, "require_auth: tenant pool failed");
                    (StatusCode::INTERNAL_SERVER_ERROR, "tenant pool unavailable").into_response()
                })?;
                return Ok($variant(dbp.pool().clone()));
            }
        };
    }

    #[cfg(feature = "postgres")]
    try_ctx!(sqlx::Postgres);
    #[cfg(feature = "sqlite")]
    try_ctx!(sqlx::Sqlite);
    #[cfg(feature = "mysql")]
    try_ctx!(sqlx::MySql);

    #[cfg(feature = "postgres")]
    try_db_ctx!(sqlx::Postgres, Pool::Postgres);
    #[cfg(feature = "sqlite")]
    try_db_ctx!(sqlx::Sqlite, Pool::Sqlite);
    #[cfg(feature = "mysql")]
    try_db_ctx!(sqlx::MySql, Pool::Mysql);

    tracing::error!(
        "require_auth ran without a tenant context in request extensions — \
         mount the tenancy layer (server::Builder::tenant_pools) ahead of it"
    );
    Err((StatusCode::INTERNAL_SERVER_ERROR, "tenant context missing").into_response())
}

// ------------------------------------------------------------------ Internal states

#[derive(Clone)]
struct AuthState {
    backends: Arc<Vec<BoxedBackend>>,
    required: bool, // false = optional_auth
}

#[derive(Clone)]
struct PermState {
    codename: &'static str,
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

    // The tenant for THIS request, not for the router.
    let pool = match tenant_pool(&dummy_parts, req.extensions()).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    let mut authenticated: Option<AuthUser> = None;
    let mut error_response: Option<Response> = None;

    for backend in state.backends.iter() {
        match backend.authenticate(&dummy_parts, &pool).await {
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
    // Same tenant as the credential was checked against —
    // a permission read against the wrong database is how tenant A's
    // admin became tenant B's admin.
    let (parts, body) = req.into_parts();
    let pool = match tenant_pool(&parts, &parts.extensions).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let req = Request::from_parts(parts, body);
    let ok = permissions::has_perm_pool(user.id, state.codename, &pool)
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
/// .require_perm("post.add")   // inner — runs after auth
/// .require_auth(backends)     // outer — runs first
/// ```
///
/// # These took a `Pool` until 0.57.11
///
/// Dropping the argument is the migration:
///
/// ```text
/// .require_auth(backends, pool.clone())  →  .require_auth(backends)
/// .require_perm("post.add", pool)        →  .require_perm("post.add")
/// ```
///
/// The pool now comes from the tenant resolved for each request. A
/// caller-supplied one authenticated every host against one database,
/// so no variant accepts one.
///
/// The tenancy layer must be mounted outside these — without a
/// `TenantContext` in extensions they fail closed with a 500 rather
/// than guess a database.
pub trait RouterAuthExt<S> {
    /// Require a valid identity for all routes in this router. Injects
    /// [`AuthenticatedUser`] into extensions on success; returns 401 on failure.
    fn require_auth(self, backends: Vec<BoxedBackend>) -> Self;

    /// Like [`RouterAuthExt::require_auth`] but does NOT return 401 for
    /// anonymous requests. Useful for routes that serve both
    /// authenticated and anonymous users.
    fn optional_auth(self, backends: Vec<BoxedBackend>) -> Self;

    /// Require `codename` permission on the already-resolved
    /// [`AuthenticatedUser`]. Must be placed inside (closer to handlers
    /// than) a `require_auth` layer.
    fn require_perm(self, codename: &'static str) -> Self;
}

impl<S: Clone + Send + Sync + 'static> RouterAuthExt<S> for Router<S> {
    fn require_auth(self, backends: Vec<BoxedBackend>) -> Self {
        let state = AuthState {
            backends: Arc::new(backends),
            required: true,
        };
        self.layer(axum::middleware::from_fn_with_state(state, auth_middleware))
    }

    fn optional_auth(self, backends: Vec<BoxedBackend>) -> Self {
        let state = AuthState {
            backends: Arc::new(backends),
            required: false,
        };
        self.layer(axum::middleware::from_fn_with_state(state, auth_middleware))
    }

    fn require_perm(self, codename: &'static str) -> Self {
        let state = PermState { codename };
        self.layer(axum::middleware::from_fn_with_state(state, perm_middleware))
    }
}
