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
    session::SessionSecret, ChainResolver, DefaultTenantDb, Org, OrgResolver, TenantPools,
};

/// Per-server context that the [`Tenant`] extractor reads out of
/// request extensions. Generic over the tenant-data backend
/// (`DB = sqlx::Postgres` default keeps existing call sites compiling
/// unchanged). Populated once by [`crate::server::Builder`] and
/// `Arc`-cloned into every request.
pub struct TenantContext<DB: Database = DefaultTenantDb> {
    pub pools: Arc<TenantPools<DB>>,
    pub resolver: ChainResolver,
    /// The HMAC-SHA256 key used to sign tenant session cookies. Set by
    /// [`crate::server::Builder`] so that [`SessionUser`] can validate
    /// cookies on public routes without going through the admin router.
    pub session_secret: SessionSecret,
    /// The HMAC-SHA256 key used to sign operator session cookies.
    pub operator_secret: SessionSecret,
}

/// Extractor: resolves the request's tenant and exposes the
/// tenant-scoped pool. Generic over the backend (`DB = sqlx::Postgres`
/// default — `fn handler(t: Tenant)` continues to mean
/// `Tenant<sqlx::Postgres>`). Handlers query through the tri-dialect
/// ORM helpers (`fetch_pool` / `save_pool` / etc.) against
/// [`Tenant::pool`].
///
/// ```ignore
/// pub async fn my_handler(t: Tenant) -> Result<Json<Vec<Post>>, StatusCode> {
///     let posts = Post::objects().fetch_pool(t.pool()).await?;
///     Ok(Json(posts))
/// }
/// ```
///
/// # No held connection (v0.41.2)
///
/// Before v0.41.2 this extractor eagerly acquired a `TenantConn` and
/// held it for the handler's entire lifetime — a deliberate design
/// choice that turned out to deadlock under concurrent load. With
/// `database_pool_max_connections = N`, **N concurrent handlers
/// pinned every pool slot**; each handler's own inner `fetch_pool`
/// call then blocked at acquire, none of the held conns could be
/// released (because every holder was itself waiting), and the
/// whole admin surface stalled until the 30 s acquire timeout
/// exhausted every request. Symptom report:
/// [ujeenet/rustango-cms#280](https://github.com/ujeenet/rustango-cms/issues/280).
///
/// The extractor now resolves only the `(org, pool)` pair. Each
/// `fetch_pool` / `insert_pool` / `save_pool` call inside the
/// handler acquires + releases a connection as a transient — no
/// long-lived holder, no deadlock vector. Handlers that genuinely
/// need a held connection (e.g. a hand-rolled multi-statement
/// transaction) can ask for one explicitly:
///
/// ```ignore
/// pub async fn streaming_handler(t: Tenant) -> Result<…, …> {
///     let mut conn = t.pool().acquire().await?;
///     // use &mut *conn with sqlx::query! / fetch_on / etc.
/// }
/// ```
pub struct Tenant<DB: Database = DefaultTenantDb> {
    pub org: Org,
    /// v0.38 — backend-erasing pool reference for the tenant's storage.
    /// On PG schema-mode this wraps the registry pool (queries
    /// against it would hit the `public` schema unless `SET
    /// search_path` is applied first — schema-mode handlers should
    /// acquire + run `SET search_path` themselves before the first
    /// query, or use a schema-mode-aware ORM helper).
    /// On non-PG (and PG database-mode) this is the tenant's
    /// dedicated pool; handlers can run
    /// `Model::objects().fetch_pool(&t.pool)` for tri-dialect ORM
    /// queries with no extra ceremony.
    pool: crate::sql::Pool,
    // Backend phantom — the `DB` type parameter is preserved purely
    // so the existing `Tenant<DB>` shape compiles. The conn field
    // used to live here; v0.41.2 dropped it to close the deadlock
    // (see struct docs).
    _db: std::marker::PhantomData<DB>,
}

impl<DB: Database> Tenant<DB> {
    /// Borrow the tenant-scoped [`crate::sql::Pool`] enum. Use this
    /// when routing through the tri-dialect ORM (`fetch_pool` /
    /// `insert_pool` / `save_pool`) — every backend works through the
    /// same code path. Each call acquires + releases a connection
    /// internally; no holder is pinned to the handler's lifetime.
    ///
    /// **PG schema-mode note**: the pool wraps the shared registry
    /// pool; queries against it would hit `public` instead of the
    /// tenant schema unless `SET search_path` is applied first.
    /// Database-mode (any backend) is unaffected.
    #[must_use]
    pub fn pool(&self) -> &crate::sql::Pool {
        &self.pool
    }

    /// **Test-only** — construct a `Tenant` directly from an `Org`
    /// row + a pool. Bypasses the extractor flow that production
    /// handlers use. v0.41.2 — the conn parameter is gone; tests
    /// that genuinely need a held conn for their fixture should
    /// acquire from `pool` directly after construction.
    ///
    /// Gated behind the `test_utils` feature so production builds
    /// can't reach for it accidentally.
    #[cfg(any(test, feature = "test_utils"))]
    #[must_use]
    pub fn for_test(org: Org, pool: crate::sql::Pool) -> Self {
        Self {
            org,
            pool,
            _db: std::marker::PhantomData,
        }
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

// v0.41.2 — extractor resolves `(org, pool)` only. No eager
// `database_acquire` / `acquire` (would deadlock under concurrent
// load — see struct docs + ujeenet/rustango-cms#280).
#[cfg(feature = "postgres")]
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
        // v0.38 — resolve the backend-erasing Pool enum so
        // `t.pool()` lets handlers use tri-dialect ORM helpers
        // (fetch_pool / save_pool / etc.). Schema-mode picks the
        // shared registry pool (which requires SET search_path);
        // database-mode resolves to the dedicated tenant pool.
        let pool = ctx
            .pools
            .scoped_pool_dyn(&org)
            .await
            .map_err(|e| TenantRejection::Internal(e.to_string()))?;
        Ok(Tenant {
            org,
            pool,
            _db: std::marker::PhantomData,
        })
    }
}

// v0.41.2 — extractor is pool-only on Sqlite for the same reason as
// the PG impl (see struct docs).
#[cfg(feature = "sqlite")]
impl<S> FromRequestParts<S> for Tenant<sqlx::Sqlite>
where
    S: Send + Sync,
{
    type Rejection = TenantRejection;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let ctx = parts
            .extensions
            .get::<Arc<TenantContext<sqlx::Sqlite>>>()
            .ok_or(TenantRejection::MissingContext)?
            .clone();
        let org = ctx
            .resolver
            .resolve(parts, &ctx.pools.registry_pool())
            .await
            .map_err(|e| TenantRejection::Internal(e.to_string()))?
            .ok_or(TenantRejection::NotFound)?;
        let pool = ctx
            .pools
            .scoped_pool_dyn(&org)
            .await
            .map_err(|e| TenantRejection::Internal(e.to_string()))?;
        Ok(Tenant {
            org,
            pool,
            _db: std::marker::PhantomData,
        })
    }
}

// v0.41.2 — same for MySql.
#[cfg(feature = "mysql")]
impl<S> FromRequestParts<S> for Tenant<sqlx::MySql>
where
    S: Send + Sync,
{
    type Rejection = TenantRejection;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let ctx = parts
            .extensions
            .get::<Arc<TenantContext<sqlx::MySql>>>()
            .ok_or(TenantRejection::MissingContext)?
            .clone();
        let org = ctx
            .resolver
            .resolve(parts, &ctx.pools.registry_pool())
            .await
            .map_err(|e| TenantRejection::Internal(e.to_string()))?
            .ok_or(TenantRejection::NotFound)?;
        let pool = ctx
            .pools
            .scoped_pool_dyn(&org)
            .await
            .map_err(|e| TenantRejection::Internal(e.to_string()))?;
        Ok(Tenant {
            org,
            pool,
            _db: std::marker::PhantomData,
        })
    }
}
