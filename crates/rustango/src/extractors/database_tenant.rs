//! The `DatabaseTenant<DB>` extractor, for tenants whose data lives
//! in SQLite or MySQL. Database mode only. Postgres tenants use
//! [`super::Tenant`] instead.
//!
//! The app installs one [`DatabaseTenantContext<DB>`] at boot, so the
//! whole process uses a single backend type for tenant data.
//!
//! ```ignore
//! use rustango::extractors::DatabaseTenant;
//! use rustango::sql::sqlx;
//!
//! async fn list_posts(mut t: DatabaseTenant<sqlx::Sqlite>) -> impl IntoResponse {
//!     let rows = sqlx::query!("SELECT id, title FROM post")
//!         .fetch_all(t.conn()).await?;
//!     Json(rows)
//! }
//! ```

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sqlx::Database;

use crate::sql::sqlx;
use crate::tenancy::{
    session::SessionSecret, ChainResolver, DatabaseConn, DatabasePools, Org, OrgResolver,
};

/// What the [`DatabaseTenant`] extractor reads from request
/// extensions. `DB` is the tenant-data backend, `sqlx::Sqlite` or
/// `sqlx::MySql`. The registry pool is the backend-erasing
/// [`crate::sql::Pool`], so an app can be all SQLite, or mix a
/// Postgres registry with SQLite tenants.
///
/// Built once at boot and cloned into every request.
pub struct DatabaseTenantContext<DB: Database> {
    /// Tenant-data pools for this backend.
    pub pools: Arc<DatabasePools<DB>>,
    /// Resolver chain. It works on any backend.
    pub resolver: ChainResolver,
    /// Key that signs tenant session cookies.
    pub session_secret: SessionSecret,
    /// Key that signs operator session cookies.
    pub operator_secret: SessionSecret,
    /// The registry pool, on any backend, so an all-SQLite stack
    /// needs no Postgres at all.
    pub registry: crate::sql::Pool,
}

/// Resolves the request's tenant from the registry, then hands the
/// handler a connection on the configured backend. The extractor owns
/// that connection and returns it to the pool when the handler ends.
///
/// This is the non-Postgres twin of [`super::Tenant`]. Pick the one
/// that matches your backend.
pub struct DatabaseTenant<DB: Database> {
    pub org: Org,
    conn: DatabaseConn<DB>,
}

impl<DB: Database> DatabaseTenant<DB> {
    /// Borrow the tenant's connection. Inside is a
    /// `sqlx::pool::PoolConnection<DB>`, which works as an sqlx
    /// executor through `&mut **t.conn()`, or straight with the query
    /// macros that take `&mut Connection`.
    pub fn conn(&mut self) -> &mut DatabaseConn<DB> {
        &mut self.conn
    }

    /// Take the connection out. Use it in a handler that is done with
    /// the database but still has slow work to do.
    #[must_use]
    pub fn into_conn(self) -> DatabaseConn<DB> {
        self.conn
    }

    /// **For tests only.** Build one from an `Org` and a connection,
    /// skipping the resolver chain. Mirrors `Tenant::for_test`.
    #[cfg(any(test, feature = "test_utils"))]
    #[must_use]
    pub fn for_test(org: Org, conn: DatabaseConn<DB>) -> Self {
        Self { org, conn }
    }
}

/// Why the [`DatabaseTenant`] extractor failed. It has the same
/// shape as [`super::TenantRejection`], but is a separate type so
/// the compiler catches a mix-up.
#[derive(Debug)]
pub enum DatabaseTenantRejection {
    /// No `DatabaseTenantContext` extension, so the server was not
    /// built for this backend.
    MissingContext,
    /// No `Org` matches the request.
    NotFound,
    /// The resolver or the pool acquire failed in the driver.
    Internal(String),
}

impl IntoResponse for DatabaseTenantRejection {
    fn into_response(self) -> Response {
        match self {
            Self::MissingContext => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "DatabaseTenantContext not installed — the server wasn't built \
                 with `Cli::tenants::<DB>()` for the matching backend.",
            )
                .into_response(),
            Self::NotFound => (StatusCode::NOT_FOUND, "tenant not found").into_response(),
            Self::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response(),
        }
    }
}

impl<S, DB> FromRequestParts<S> for DatabaseTenant<DB>
where
    S: Send + Sync,
    DB: Database + 'static,
{
    type Rejection = DatabaseTenantRejection;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let ctx = parts
            .extensions
            .get::<Arc<DatabaseTenantContext<DB>>>()
            .ok_or(DatabaseTenantRejection::MissingContext)?
            .clone();
        let org = ctx
            .resolver
            .resolve(parts, &ctx.registry)
            .await
            .map_err(|e| DatabaseTenantRejection::Internal(e.to_string()))?
            .ok_or(DatabaseTenantRejection::NotFound)?;
        let conn = ctx
            .pools
            .acquire(&org)
            .await
            .map_err(|e| DatabaseTenantRejection::Internal(e.to_string()))?;
        Ok(DatabaseTenant { org, conn })
    }
}
