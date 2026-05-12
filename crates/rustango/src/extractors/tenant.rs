//! `Tenant<DB>` extractor — resolves the request's tenant + acquires
//! a tenant-scoped connection on the configured backend.
//!
//! Default backend is Postgres (`Tenant` = `Tenant<sqlx::Postgres>`)
//! so existing call sites (`fn handler(t: Tenant)`) compile unchanged.
//!
//! **Schema-mode is Postgres-only by language**: the implementation
//! uses `SET search_path`. `Tenant<sqlx::Postgres>` supports both
//! schema-mode and database-mode tenants; `Tenant<sqlx::Sqlite>` /
//! `Tenant<sqlx::MySql>` support database-mode only — `SET search_path`
//! doesn't exist on those backends.

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sqlx::Database;

use crate::sql::sqlx;
use crate::tenancy::{
    session::SessionSecret, ChainResolver, Org, OrgResolver, TenantConn, TenantPools,
};

/// Per-server context that the [`Tenant`] extractor reads out of
/// request extensions. Generic over the tenant-data backend
/// (`DB = sqlx::Postgres` default keeps existing call sites compiling
/// unchanged). Populated once by [`crate::server::Builder`] and
/// `Arc`-cloned into every request.
pub struct TenantContext<DB: Database = sqlx::Postgres> {
    pub pools: Arc<TenantPools<DB>>,
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
/// scoped to it. Generic over the backend (`DB = sqlx::Postgres`
/// default — `fn handler(t: Tenant)` continues to mean
/// `Tenant<sqlx::Postgres>`). Handlers borrow the connection through
/// [`Tenant::conn`] for ORM calls.
///
/// ```ignore
/// pub async fn my_handler(mut t: Tenant) -> Result<Json<Vec<Post>>, StatusCode> {
///     let posts = Post::objects().fetch_on(t.conn()).await?;
///     Ok(Json(posts))
/// }
/// ```
pub struct Tenant<DB: Database = sqlx::Postgres> {
    pub org: Org,
    conn: TenantConn<DB>,
}

impl<DB: Database> Tenant<DB> {
    /// Borrow the tenant-scoped pool connection. Use this for sqlx
    /// query macros (`sqlx::query!(...).fetch_all(t.pool_conn())`)
    /// when working in a backend-agnostic context. PG-specific code
    /// can use [`Tenant::conn`] instead which derefs all the way to
    /// `&mut PgConnection` so the framework's `_on` helpers
    /// (`fetch_on`, `select_rows_on`) accept it directly.
    pub fn pool_conn(&mut self) -> &mut sqlx::pool::PoolConnection<DB> {
        &mut self.conn
    }

    /// Yield the underlying connection, releasing it back to the
    /// pool when dropped. Use for handlers that finished their DB
    /// work but still have long-running computation left.
    #[must_use]
    pub fn into_conn(self) -> TenantConn<DB> {
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
    pub fn for_test(org: Org, conn: TenantConn<DB>) -> Self {
        Self { org, conn }
    }
}

impl Tenant<sqlx::Postgres> {
    /// Borrow the tenant-scoped connection as `&mut PgConnection` —
    /// the executor type sqlx and rustango's `fetch_on` / `get_on`
    /// expect. PG-only; for generic backends, use
    /// [`Tenant::pool_conn`] and the sqlx query macros.
    pub fn conn(&mut self) -> &mut sqlx::PgConnection {
        &mut self.conn
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

// v0.38 — the FromRequestParts impl is wired for the PG default only.
// `Tenant<sqlx::Postgres>` goes through schema-mode-aware `TenantPools<Postgres>::acquire`.
// Sqlite/MySQL apps use `DatabaseTenant<DB>` until the non-PG runtime
// path lights up in a follow-up slice.
impl<S> FromRequestParts<S> for Tenant<sqlx::Postgres>
where
    S: Send + Sync,
{
    type Rejection = TenantRejection;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let ctx = parts
            .extensions
            .get::<Arc<TenantContext<sqlx::Postgres>>>()
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
