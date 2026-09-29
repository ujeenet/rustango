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
    session::SessionSecret, ChainResolver, DefaultTenantDb, Org, TenancyError, TenantConn,
    TenantPools,
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
    /// No connection yet; take one from the tenant's pool, the same
    /// one [`Tenant::pool`] erases, when asked.
    ///
    /// Only the SQLite and MySQL extractors build this variant, so a
    /// Postgres-only build never constructs it, but the arms that
    /// match it still have to compile.
    #[cfg_attr(not(any(feature = "sqlite", feature = "mysql")), allow(dead_code))]
    Deferred(sqlx::Pool<DB>),
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
        let pool = match &self.conn {
            TenantConnCell::Ready(_) => return Ok(()),
            TenantConnCell::Deferred(pool) => pool.clone(),
        };
        self.conn = TenantConnCell::Ready(TenantConn::database(pool.acquire().await?));
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

/// A `Tenant<DB>` with the backend erased, so a `<DB>`-generic handler can
/// hand off to a plain one whose future stays provably `Send` (#1778).
pub(crate) struct TenantScope {
    pub(crate) org: Org,
    pool: crate::sql::Pool,
}

impl TenantScope {
    pub(crate) fn pool(&self) -> &crate::sql::Pool {
        &self.pool
    }
}

impl<DB: Database> From<Tenant<DB>> for TenantScope {
    fn from(t: Tenant<DB>) -> Self {
        Self {
            org: t.org,
            pool: t.pool,
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
    /// No `TenantContext` extension (nor, on SQLite / MySQL, a
    /// `DatabaseTenantContext`), so the server was not built with
    /// `rustango::server::Builder`. Also when the context the request
    /// uses is another backend's (#1826).
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
                "extractors::tenant",
                "rustango::server::Builder did not run — Tenant extractor cannot find \
                 TenantContext or DatabaseTenantContext for its backend",
            ),
            Self::NotFound => ApiError::not_found("tenant not found"),
            Self::Internal(msg) => {
                ApiError::logged(StatusCode::INTERNAL_SERVER_ERROR, "extractors::tenant", msg)
            }
        }
        .into_response()
    }
}

#[cfg(test)]
mod rejection_tests {
    use super::*;

    /// A resolver failure is a 500 that withholds the driver text (#1193).
    #[tokio::test]
    async fn an_internal_rejection_withholds_its_cause() {
        let _env = crate::error::test_env::lock();
        let r =
            TenantRejection::Internal("pool timed out on registry-db:5432".into()).into_response();
        assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let b = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["error"], "internal_error");
        assert!(!v.to_string().contains("registry-db"), "{v}");
    }
}

/// #1826 — with two contexts mounted, an extractor serves the tenant
/// that `request_org` (auth, sessions) resolved, or rejects.
#[cfg(all(test, feature = "sqlite"))]
mod one_context_tests {
    use super::*;
    use crate::extractors::{DatabaseTenant, DatabaseTenantContext};
    use crate::tenancy::{BackendKind, DatabasePools, OrgResolver};

    struct Fixed(Org);

    #[async_trait::async_trait]
    impl OrgResolver for Fixed {
        async fn resolve(
            &self,
            _: &Parts,
            _: &crate::sql::Pool,
        ) -> Result<Option<Org>, TenancyError> {
            Ok(Some(self.0.clone()))
        }
    }

    fn org(slug: &str) -> Org {
        Org {
            slug: slug.into(),
            storage_mode: "database".into(),
            backend_kind: "sqlite".into(),
            database_url: Some("sqlite::memory:".into()),
            ..crate::testkit::org()
        }
    }

    fn secret() -> SessionSecret {
        SessionSecret::from_bytes(vec![7u8; 32])
    }

    async fn sqlite_ctx(slug: &str) -> Arc<TenantContext<sqlx::Sqlite>> {
        let reg = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        Arc::new(TenantContext {
            pools: Arc::new(TenantPools::new(reg)),
            resolver: ChainResolver::new().push(Fixed(org(slug))),
            session_secret: secret(),
            operator_secret: secret(),
        })
    }

    fn parts() -> Parts {
        axum::http::Request::new(()).into_parts().0
    }

    /// What `request_org` resolved, i.e. the tenant auth checks against.
    async fn auth_slug(p: &Parts) -> String {
        crate::tenancy::middleware::request_org(p, &p.extensions)
            .await
            .expect("a context is mounted")
            .unwrap()
            .expect("an org")
            .slug
    }

    #[tokio::test]
    async fn database_tenant_serves_the_tenant_auth_resolved() {
        let mut p = parts();
        p.extensions.insert(sqlite_ctx("full").await);
        p.extensions
            .insert(Arc::new(DatabaseTenantContext::<sqlx::Sqlite> {
                pools: Arc::new(DatabasePools::new(BackendKind::Sqlite)),
                resolver: ChainResolver::new().push(Fixed(org("other"))),
                session_secret: secret(),
                operator_secret: secret(),
                registry: crate::sql::Pool::connect("sqlite::memory:").await.unwrap(),
            }));
        assert_eq!(auth_slug(&p).await, "full");
        let got = DatabaseTenant::<sqlx::Sqlite>::from_request_parts(&mut p, &())
            .await
            .map(|t| t.org.slug);
        assert!(
            matches!(
                got,
                Err(crate::extractors::DatabaseTenantRejection::MissingContext)
            ),
            "auth resolved `full`, the handler got {got:?}"
        );
    }

    #[cfg(feature = "postgres")]
    #[tokio::test]
    async fn sqlite_tenant_serves_the_tenant_auth_resolved() {
        let reg = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://nobody@127.0.0.1:1/none")
            .unwrap();
        let mut p = parts();
        p.extensions
            .insert(Arc::new(TenantContext::<sqlx::Postgres> {
                pools: Arc::new(TenantPools::new(reg)),
                resolver: ChainResolver::new().push(Fixed(org("pg"))),
                session_secret: secret(),
                operator_secret: secret(),
            }));
        p.extensions.insert(sqlite_ctx("sqlite").await);
        assert_eq!(auth_slug(&p).await, "pg");
        let got = Tenant::<sqlx::Sqlite>::from_request_parts(&mut p, &())
            .await
            .map(|t| t.org.slug);
        assert!(
            matches!(got, Err(TenantRejection::MissingContext)),
            "auth resolved `pg`, the handler got {got:?}"
        );
    }
}

/// The request's Org from the mounted context, as a rejection on failure.
async fn resolved_org(
    mounted: super::MountedTenantContext<'_>,
    parts: &Parts,
) -> Result<Org, TenantRejection> {
    mounted
        .resolve(parts)
        .await
        .map_err(|e| TenantRejection::Internal(e.to_string()))?
        .ok_or(TenantRejection::NotFound)
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
        let mounted = super::MountedTenantContext::of(&parts.extensions)
            .ok_or(TenantRejection::MissingContext)?;
        let ctx = mounted
            .full::<sqlx::Postgres>()
            .ok_or(TenantRejection::MissingContext)?
            .clone();
        let org = resolved_org(mounted, parts).await?;
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

/// The SQLite / MySQL extractor. It reads `TenantContext<DB>`, or the
/// pure stack's `DatabaseTenantContext<DB>` (#1802). Database mode only.
///
/// It takes no connection yet. A handler that only uses `t.pool()` then
/// holds none at all; `pool_conn()` and `into_conn()` take one when
/// asked. Otherwise every concurrent request pins a connection it may
/// never use, and at `max_conn` later acquires time out.
#[cfg(any(feature = "sqlite", feature = "mysql"))]
async fn deferred_tenant<DB>(parts: &mut Parts) -> Result<Tenant<DB>, TenantRejection>
where
    DB: Database,
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let internal = |e: TenancyError| TenantRejection::Internal(e.to_string());
    let mounted = super::MountedTenantContext::of(&parts.extensions)
        .ok_or(TenantRejection::MissingContext)?;
    let full = mounted.full::<DB>().cloned();
    let db = mounted.database::<DB>().cloned();
    if full.is_none() && db.is_none() {
        return Err(TenantRejection::MissingContext);
    }
    let org = resolved_org(mounted, parts).await?;
    let pool = if let Some(ctx) = full {
        // Rejects a schema-mode org, which needs Postgres.
        #[cfg_attr(not(feature = "postgres"), allow(irrefutable_let_patterns))]
        let crate::tenancy::TenantPool::Database { pool } = ctx
            .pools
            .database_pool_for_org(&org)
            .await
            .map_err(internal)?
        else {
            unreachable!("database_pool_for_org rejects schema-mode")
        };
        (*pool).clone()
    } else {
        let ctx = db.ok_or(TenantRejection::MissingContext)?;
        let pool = ctx.pools.pool_for_org(&org).await.map_err(internal)?;
        pool.pool().clone()
    };
    Ok(Tenant {
        org,
        pool: crate::sql::Pool::from(pool.clone()),
        conn: TenantConnCell::Deferred(pool),
    })
}

#[cfg(feature = "sqlite")]
impl<S> FromRequestParts<S> for Tenant<sqlx::Sqlite>
where
    S: Send + Sync,
{
    type Rejection = TenantRejection;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        deferred_tenant(parts).await
    }
}

#[cfg(feature = "mysql")]
impl<S> FromRequestParts<S> for Tenant<sqlx::MySql>
where
    S: Send + Sync,
{
    type Rejection = TenantRejection;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        deferred_tenant(parts).await
    }
}
