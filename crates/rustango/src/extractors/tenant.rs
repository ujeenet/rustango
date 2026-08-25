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
    session::SessionSecret, ChainResolver, DefaultTenantDb, Org, OrgResolver, TenancyError,
    TenantConn, TenantPools,
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

/// Backing store for a [`Tenant`]'s connection.
///
/// Database-mode extractors (SQLite / MySQL) start out `Deferred` and
/// only check a connection out of the pool when a handler actually asks
/// for one via [`Tenant::pool_conn`] / [`Tenant::into_conn`]. Handlers
/// that query solely through [`Tenant::pool`] (the tri-dialect ORM
/// path) hold zero idle connections, so a request no longer pins a pool
/// slot it never uses — which is what caused pool-exhaustion deadlock
/// once concurrency reached `max_conn`. Postgres stays eager (`Ready`)
/// so schema-mode `SET search_path` is applied up front.
enum TenantConnCell<DB: Database> {
    /// Connection already checked out — Postgres always, database-mode
    /// after the first `pool_conn()`/`into_conn()` call.
    Ready(TenantConn<DB>),
    /// No connection held yet; acquire from these pools on demand.
    ///
    /// Only the SQLite and MySQL extractors construct this (Postgres is eager,
    /// per the note above), so in a Postgres-only build it is legitimately
    /// unconstructed rather than dead — the arms that match it still have to
    /// compile.
    #[cfg_attr(not(any(feature = "sqlite", feature = "mysql")), allow(dead_code))]
    Deferred(Arc<TenantPools<DB>>),
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
pub struct Tenant<DB: Database = DefaultTenantDb> {
    pub org: Org,
    conn: TenantConnCell<DB>,
    /// v0.38 — backend-erasing pool reference for the tenant's storage.
    /// On PG schema-mode this wraps the registry pool (queries through
    /// it would hit the `public` schema unless `SET search_path` is
    /// applied — for that path prefer [`Tenant::conn`]). On non-PG
    /// (and PG database-mode), this is the tenant's dedicated pool;
    /// handlers can run `Model::objects().fetch(&t.pool)` for
    /// tri-dialect ORM queries.
    pool: crate::sql::Pool,
}

impl<DB: Database> Tenant<DB> {
    /// Borrow the tenant-scoped pool connection, acquiring it on first
    /// use for database-mode tenants (the extractor defers the checkout
    /// so `t.pool()`-only handlers never pin a slot). Use this for sqlx
    /// query macros (`sqlx::query!(...).fetch_all(t.pool_conn().await?)`)
    /// in a backend-agnostic context. PG-specific code can use
    /// [`Tenant::conn`] instead which derefs all the way to
    /// `&mut PgConnection` so the framework's `_on` helpers
    /// (`fetch_on`, `select_rows_on`) accept it directly.
    ///
    /// # Errors
    /// Pool acquire failure for a deferred (database-mode) connection.
    pub async fn pool_conn(&mut self) -> Result<&mut sqlx::pool::PoolConnection<DB>, TenancyError> {
        self.ensure_conn().await?;
        match &mut self.conn {
            TenantConnCell::Ready(conn) => Ok(conn),
            TenantConnCell::Deferred(_) => {
                unreachable!("ensure_conn just populated the connection")
            }
        }
    }

    /// Acquire the deferred connection if one isn't held yet. No-op
    /// once the cell is [`TenantConnCell::Ready`] (always, for PG).
    async fn ensure_conn(&mut self) -> Result<(), TenancyError> {
        let pools = match &self.conn {
            TenantConnCell::Ready(_) => return Ok(()),
            TenantConnCell::Deferred(pools) => Arc::clone(pools),
        };
        let conn = pools.database_acquire(&self.org).await?;
        self.conn = TenantConnCell::Ready(conn);
        Ok(())
    }

    /// Yield the underlying connection, releasing it back to the
    /// pool when dropped. Use for handlers that finished their DB
    /// work but still have long-running computation left. Acquires the
    /// connection first if the extractor deferred it (database-mode).
    ///
    /// # Errors
    /// Pool acquire failure for a deferred (database-mode) connection.
    pub async fn into_conn(mut self) -> Result<TenantConn<DB>, TenancyError> {
        self.ensure_conn().await?;
        match self.conn {
            TenantConnCell::Ready(conn) => Ok(conn),
            TenantConnCell::Deferred(_) => {
                unreachable!("ensure_conn just populated the connection")
            }
        }
    }

    /// Borrow the tenant-scoped [`crate::sql::Pool`] enum. Use this
    /// when routing through the tri-dialect ORM (`fetch` /
    /// `insert_pool` / `save_pool`) — every backend works through the
    /// same code path.
    ///
    /// **PG schema-mode note**: the pool wraps the shared registry
    /// pool; queries against it hit `public` instead of the tenant
    /// schema. For schema-mode-on-PG paths use [`Tenant::conn`] (which
    /// has `SET search_path` applied). Database-mode (any backend) is
    /// unaffected.
    ///
    /// That `public` is a guarantee, not a coincidence:
    /// `TenantConn` resets `search_path` when it is released, so this
    /// pool never hands out a connection still scoped to whichever
    /// tenant borrowed it last (#1224). Wrong-namespace queries here
    /// fail loudly rather than reading another tenant's rows.
    #[must_use]
    pub fn pool(&self) -> &crate::sql::Pool {
        &self.pool
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
    pub fn for_test(org: Org, conn: TenantConn<DB>, pool: crate::sql::Pool) -> Self {
        Self {
            org,
            conn: TenantConnCell::Ready(conn),
            pool,
        }
    }
}

#[cfg(feature = "postgres")]
impl Tenant<sqlx::Postgres> {
    /// Borrow the tenant-scoped connection as `&mut PgConnection` —
    /// the executor type sqlx and rustango's `fetch_on` / `get_on`
    /// expect. PG-only; for generic backends, use
    /// [`Tenant::pool_conn`] and the sqlx query macros.
    pub fn conn(&mut self) -> &mut sqlx::PgConnection {
        match &mut self.conn {
            TenantConnCell::Ready(conn) => conn,
            TenantConnCell::Deferred(_) => {
                unreachable!("Tenant<Postgres> acquires its connection eagerly")
            }
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

// v0.38 — `Tenant<sqlx::Postgres>` goes through the schema-mode-aware
// `TenantPools<Postgres>::acquire` so PG schema-mode tenants get
// `SET search_path` applied before the connection is handed to the
// handler. Database-mode PG tenants go through the same path.
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
        let conn = ctx
            .pools
            .acquire(&org)
            .await
            .map_err(|e| TenantRejection::Internal(e.to_string()))?;
        // v0.38 — also resolve the backend-erasing Pool enum so
        // `t.pool()` lets handlers use tri-dialect ORM helpers
        // (fetch / save_pool / etc.). Schema-mode picks the
        // shared registry pool (which requires SET search_path);
        // database-mode resolves to the dedicated tenant pool.
        let pool = ctx
            .pools
            .scoped_pool_dyn(&org)
            .await
            .map_err(|e| TenantRejection::Internal(e.to_string()))?;
        Ok(Tenant {
            org,
            conn: TenantConnCell::Ready(conn),
            pool,
        })
    }
}

// v0.38 — `Tenant<sqlx::Sqlite>` routes through the generic
// `TenantPools::database_acquire` (database-mode only — schema-mode
// is PG-only by language). The user assembles routing the same way
// as the PG case; `TenantContext<sqlx::Sqlite>` carries
// `TenantPools<sqlx::Sqlite>`. Same shape, no `SET search_path`.
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
        // Lazy connection (database-mode): don't pin a pool slot at
        // extraction. Handlers using only `t.pool()` hold zero idle
        // connections; `pool_conn()` / `into_conn()` check one out on
        // demand. Prevents the pool-exhaustion deadlock where each
        // concurrent request pinned an unused connection — at
        // `>= max_conn` every later `t.pool()` acquire then timed out.
        Ok(Tenant {
            org,
            conn: TenantConnCell::Deferred(Arc::clone(&ctx.pools)),
            pool,
        })
    }
}

// v0.38 — same for `Tenant<sqlx::MySql>`. Database-mode only.
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
        // Lazy connection (database-mode): don't pin a pool slot at
        // extraction. Handlers using only `t.pool()` hold zero idle
        // connections; `pool_conn()` / `into_conn()` check one out on
        // demand. Prevents the pool-exhaustion deadlock where each
        // concurrent request pinned an unused connection — at
        // `>= max_conn` every later `t.pool()` acquire then timed out.
        Ok(Tenant {
            org,
            conn: TenantConnCell::Deferred(Arc::clone(&ctx.pools)),
            pool,
        })
    }
}
