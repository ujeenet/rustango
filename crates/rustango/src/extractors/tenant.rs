//! The `Tenant<DB>` extractor: it finds the request's tenant and
//! gives the handler a connection scoped to it.
//!
//! `DB` defaults to Postgres, so a plain `Tenant` means
//! `Tenant<sqlx::Postgres>`.
//!
//! **Schema mode is Postgres-only**, because it relies on
//! `SET search_path`. `Tenant<sqlx::Postgres>` handles both schema
//! and database mode; `Tenant<sqlx::Sqlite>` and
//! `Tenant<sqlx::MySql>` handle database mode only.

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

/// What the [`Tenant`] extractor reads from request extensions.
/// [`crate::server::Builder`] builds it once and clones the `Arc`
/// into every request.
pub struct TenantContext<DB: Database = DefaultTenantDb> {
    pub pools: Arc<TenantPools<DB>>,
    pub resolver: ChainResolver,
    /// Key that signs tenant session cookies. Set by
    /// [`crate::server::Builder`] so `SessionUser` can check a cookie
    /// on a public route, without the admin router.
    pub session_secret: SessionSecret,
    /// Key that signs operator session cookies.
    pub operator_secret: SessionSecret,
}

/// Where a [`Tenant`] keeps its connection.
///
/// SQLite and MySQL start `Deferred` and take a connection only when
/// a handler asks, through [`Tenant::pool_conn`] or
/// [`Tenant::into_conn`]. A handler that queries only through
/// [`Tenant::pool`] then holds no connection at all, so a request
/// cannot pin a pool slot it never uses. That used to deadlock the
/// pool once concurrency reached `max_conn`. Postgres stays eager,
/// so schema mode can apply `SET search_path` up front.
enum TenantConnCell<DB: Database> {
    /// A connection is held: always on Postgres, and on the other
    /// backends after the first `pool_conn` or `into_conn` call.
    Ready(TenantConn<DB>),
    /// No connection yet; take one from these pools when asked.
    ///
    /// Only the SQLite and MySQL extractors build this variant, so a
    /// Postgres-only build never constructs it, but the arms that
    /// match it still have to compile.
    #[cfg_attr(not(any(feature = "sqlite", feature = "mysql")), allow(dead_code))]
    Deferred(Arc<TenantPools<DB>>),
}

/// Finds the request's tenant and gives the handler a connection
/// scoped to it. `DB` defaults to Postgres, so `fn handler(t: Tenant)`
/// means `Tenant<sqlx::Postgres>`. Borrow the connection with
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
    /// The tenant's pool, with the backend type erased. It is always
    /// tenant-scoped. In PG schema mode
    /// [`TenantPools::scoped_pool`] builds a dedicated pool with
    /// `search_path` in its connect options, so every checkout is
    /// scoped with no per-query `SET`. Everywhere else it is the
    /// tenant's own pool. Either way a handler can run
    /// `Model::objects().fetch(&t.pool)`.
    pool: crate::sql::Pool,
}

impl<DB: Database> Tenant<DB> {
    /// Borrow the tenant's connection, taking one from the pool on
    /// first use in database mode. Use it with the sqlx query macros
    /// when the code must work on any backend. PG-only code can use
    /// [`Tenant::conn`], which derefs to `&mut PgConnection` and so
    /// works with the `_on` helpers directly.
    ///
    /// # Errors
    /// The pool acquire can fail when the connection was deferred.
    pub async fn pool_conn(&mut self) -> Result<&mut sqlx::pool::PoolConnection<DB>, TenancyError> {
        self.ensure_conn().await?;
        match &mut self.conn {
            TenantConnCell::Ready(conn) => Ok(conn),
            TenantConnCell::Deferred(_) => {
                unreachable!("ensure_conn just populated the connection")
            }
        }
    }

    /// Take a connection if none is held yet. Does nothing once the
    /// cell is `Ready`, which on Postgres it always is.
    async fn ensure_conn(&mut self) -> Result<(), TenancyError> {
        let pools = match &self.conn {
            TenantConnCell::Ready(_) => return Ok(()),
            TenantConnCell::Deferred(pools) => Arc::clone(pools),
        };
        let conn = pools.database_acquire(&self.org).await?;
        self.conn = TenantConnCell::Ready(conn);
        Ok(())
    }

    /// Take the connection out, so the pool gets it back when it is
    /// dropped. Use it in a handler that is done with the database
    /// but still has slow work to do. Takes a connection first if the
    /// extractor deferred one.
    ///
    /// # Errors
    /// The pool acquire can fail when the connection was deferred.
    pub async fn into_conn(mut self) -> Result<TenantConn<DB>, TenancyError> {
        self.ensure_conn().await?;
        match self.conn {
            TenantConnCell::Ready(conn) => Ok(conn),
            TenantConnCell::Deferred(_) => {
                unreachable!("ensure_conn just populated the connection")
            }
        }
    }

    /// Borrow the tenant's [`crate::sql::Pool`]. Use it for ORM calls
    /// such as `fetch` or `save_pool`, which work the same on every
    /// backend.
    ///
    /// The pool is tenant-scoped in both storage modes. In schema
    /// mode [`TenantPools::scoped_pool`] puts `search_path` in the
    /// connect options of a dedicated pool, so it never borrows from
    /// the shared registry pool and cannot pick up another tenant's
    /// session state. Use [`Tenant::conn`] instead when you want the
    /// request's one pinned connection.
    ///
    /// **Cost in schema mode:** one pool build on the first miss,
    /// then free — until the cache fills. `scoped_pool` caches per
    /// tenant slug up to `max_cached_scoped_pools`, 64 by default.
    /// Past that it warns and returns the new pool without caching
    /// it, so every tenant outside the cache rebuilds a pool on every
    /// request. Raise the cap if you have many tenants. Database mode
    /// reuses the tenant's own pool throughout.
    #[must_use]
    pub fn pool(&self) -> &crate::sql::Pool {
        &self.pool
    }

    /// **For tests only.** Build a `Tenant` from an `Org` and a
    /// connection you already hold, skipping the extractor. It is
    /// behind the `test_utils` feature so a production build cannot
    /// reach it.
    ///
    /// ```ignore
    /// let pools = TenantPools::new(registry_pool);
    /// let conn  = pools.acquire(&org).await?;
    /// let mut t = Tenant::for_test(org, conn);
    /// my_function_under_test(&mut t).await?;
    /// ```
    ///
    /// Go through `pools.acquire(&org)`, as the example does. That is
    /// what applies `SET search_path` for a schema-mode tenant, just
    /// as the extractor would.
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
    /// Borrow the connection as `&mut PgConnection`, the type sqlx
    /// and the `fetch_on` / `get_on` helpers want. Postgres only; on
    /// other backends use [`Tenant::pool_conn`].
    pub fn conn(&mut self) -> &mut sqlx::PgConnection {
        match &mut self.conn {
            TenantConnCell::Ready(conn) => conn,
            TenantConnCell::Deferred(_) => {
                unreachable!("Tenant<Postgres> acquires its connection eagerly")
            }
        }
    }
}

/// Why the [`Tenant`] extractor failed.
#[derive(Debug)]
pub enum TenantRejection {
    /// No `TenantContext` extension, so the server was not built
    /// with `rustango::server::Builder`.
    MissingContext,
    /// No tenant matches the request's host, header or path.
    NotFound,
    /// The resolver or the pool acquire failed in the driver.
    Internal(String),
}

impl IntoResponse for TenantRejection {
    fn into_response(self) -> Response {
        use crate::api_errors::ApiError;
        match self {
            Self::MissingContext => ApiError::logged(
                StatusCode::INTERNAL_SERVER_ERROR,
                "rustango::server::Builder did not run — Tenant extractor cannot find TenantContext",
            ),
            Self::NotFound => ApiError::not_found("tenant not found"),
            Self::Internal(msg) => ApiError::logged(StatusCode::INTERNAL_SERVER_ERROR, msg),
        }
        .into_response()
    }
}

// `Tenant<sqlx::Postgres>` goes through `TenantPools::acquire`,
// which applies `SET search_path` for a schema-mode tenant before
// the handler sees the connection. Database mode uses the same path.
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
        // Also resolve the `Pool` enum, so `t.pool()` works with the
        // ORM helpers. Schema mode gets a dedicated pool carrying
        // `search_path`, not the shared registry pool; database mode
        // gets the tenant's own.
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

// `Tenant<sqlx::Sqlite>` uses `TenantPools::database_acquire`.
// Database mode only, since schema mode needs Postgres. Routing is
// set up as in the PG case, just with no `SET search_path`.
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
        // Take no connection yet. A handler that only uses
        // `t.pool()` then holds none at all; `pool_conn()` and
        // `into_conn()` take one when asked. Otherwise every
        // concurrent request pins a connection it may never use, and
        // at `max_conn` later acquires time out.
        Ok(Tenant {
            org,
            conn: TenantConnCell::Deferred(Arc::clone(&ctx.pools)),
            pool,
        })
    }
}

// `Tenant<sqlx::MySql>` works the same way. Database mode only.
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
        // Take no connection yet. A handler that only uses
        // `t.pool()` then holds none at all; `pool_conn()` and
        // `into_conn()` take one when asked. Otherwise every
        // concurrent request pins a connection it may never use, and
        // at `max_conn` later acquires time out.
        Ok(Tenant {
            org,
            conn: TenantConnCell::Deferred(Arc::clone(&ctx.pools)),
            pool,
        })
    }
}
