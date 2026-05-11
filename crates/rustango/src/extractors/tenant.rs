//! `Tenant` extractor — resolves the request's tenant + acquires a
//! tenant-scoped connection.

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::sql::sqlx;
use crate::tenancy::{
    operator_console::SessionSecret, ChainResolver, Org, OrgResolver, TenantConn, TenantPools,
};

/// Per-server context that the [`Tenant`] extractor reads out of
/// request extensions. Populated once by [`crate::server::Builder`]
/// and `Arc`-cloned into every request.
pub struct TenantContext {
    pub pools: Arc<TenantPools>,
    pub resolver: ChainResolver,
    /// The HMAC-SHA256 key used to sign tenant session cookies. Set by
    /// [`crate::server::Builder`] so that [`SessionUser`] can validate
    /// cookies on public routes without going through the admin router.
    pub session_secret: SessionSecret,
    /// The HMAC-SHA256 key used to sign operator session cookies.
    pub operator_secret: SessionSecret,
    /// Registry-level pool, used by [`SessionOperator`] to look up the
    /// operator row after validating the cookie.
    pub registry: sqlx::PgPool,
}

/// Extractor: resolves the request's tenant and acquires a connection
/// scoped to it. Handlers borrow the connection through
/// [`Tenant::conn`] for ORM calls.
///
/// ```ignore
/// pub async fn my_handler(mut t: Tenant) -> Result<Json<Vec<Post>>, StatusCode> {
///     let posts = Post::objects().fetch_on(t.conn()).await?;
///     Ok(Json(posts))
/// }
/// ```
pub struct Tenant {
    pub org: Org,
    conn: TenantConn,
}

impl Tenant {
    /// Borrow the tenant-scoped connection as `&mut PgConnection` —
    /// the executor type sqlx and rustango's `fetch_on` / `get_on`
    /// expect.
    pub fn conn(&mut self) -> &mut sqlx::PgConnection {
        &mut self.conn
    }

    /// Yield the underlying connection, releasing it back to the
    /// pool when dropped. Use for handlers that finished their DB
    /// work but still have long-running computation left.
    #[must_use]
    pub fn into_conn(self) -> TenantConn {
        self.conn
    }

    /// **Test-only** — construct a `Tenant` directly from an `Org`
    /// row + an already-acquired [`TenantConn`]. Bypasses the
    /// extractor flow that production handlers use.
    ///
    /// Gated behind the `test_utils` feature so production builds
    /// can't reach for it accidentally. The expected pattern in
    /// downstream crates' live tests:
    ///
    /// ```ignore
    /// let pools = TenantPools::new(registry_pool);
    /// let conn  = pools.acquire(&org).await?;
    /// let mut t = Tenant::for_test(org, conn);
    /// my_function_under_test(&mut t).await?;
    /// ```
    ///
    /// Going through `pools.acquire(&org)` ensures schema-mode
    /// tenants get `SET search_path` applied on the connection
    /// before any query — same ceremony the extractor runs.
    #[cfg(any(test, feature = "test_utils"))]
    #[must_use]
    pub fn for_test(org: Org, conn: TenantConn) -> Self {
        Self { org, conn }
    }
}

/// Failure modes for the [`Tenant`] extractor.
#[derive(Debug)]
pub enum TenantRejection {
    /// `TenantContext` extension missing — the server wasn't built
    /// via `rustango::server::Builder`.
    MissingContext,
    /// Resolver chain returned `Ok(None)` — no tenant matches the
    /// request host / header / path.
    NotFound,
    /// Resolver or pool acquire failed at the driver level.
    Internal(String),
}

impl IntoResponse for TenantRejection {
    fn into_response(self) -> Response {
        match self {
            Self::MissingContext => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "rustango::server::Builder did not run — Tenant extractor cannot find TenantContext",
            )
                .into_response(),
            Self::NotFound => (StatusCode::NOT_FOUND, "tenant not found").into_response(),
            Self::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response(),
        }
    }
}

impl<S> FromRequestParts<S> for Tenant
where
    S: Send + Sync,
{
    type Rejection = TenantRejection;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let ctx = parts
            .extensions
            .get::<Arc<TenantContext>>()
            .ok_or(TenantRejection::MissingContext)?
            .clone();
        let org = ctx
            .resolver
            .resolve(parts, &ctx.pools.registry_pool())
            .await
            .map_err(|e| TenantRejection::Internal(e.to_string()))?
            .ok_or(TenantRejection::NotFound)?;
        let conn = ctx
            .pools
            .acquire(&org)
            .await
            .map_err(|e| TenantRejection::Internal(e.to_string()))?;
        Ok(Tenant { org, conn })
    }
}
