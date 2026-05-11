//! `DatabaseTenant<DB>` extractor — parallel to [`super::Tenant`] but
//! generic over the sqlx backend, for tenants whose data lives in
//! SQLite or MySQL (database-mode only). Postgres tenants continue
//! using [`super::Tenant`].
//!
//! At boot the host app installs ONE
//! [`DatabaseTenantContext<DB>`] in the router state — i.e. the
//! whole process is committed to a single backend type for its
//! tenant-data pools. The registry pool stays Postgres for now (the
//! v0.34 work generalizes the registry; v0.33 keeps it pinned).
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
    operator_console::SessionSecret, ChainResolver, DatabaseConn, DatabasePools, Org, OrgResolver,
};

/// Per-server context that the [`DatabaseTenant`] extractor reads out
/// of request extensions. Generic over the **tenant-data** backend
/// (`sqlx::Sqlite` or `sqlx::MySql`); the registry pool is the
/// backend-erasing [`rustango::sql::Pool`] enum so apps can run
/// pure-SQLite (registry + tenant data on SQLite) or hybrid
/// (Postgres registry + SQLite tenants).
///
/// Populated once at boot by `crate::server::Builder` (or the manual
/// router-setup path) and `Arc`-cloned into every request.
pub struct DatabaseTenantContext<DB: Database> {
    /// Tenant-data pool registry parameterized over the backend.
    pub pools: Arc<DatabasePools<DB>>,
    /// Resolver chain — backend-agnostic (v0.34 B.1).
    pub resolver: ChainResolver,
    /// HMAC-SHA256 key used to sign tenant session cookies.
    pub session_secret: SessionSecret,
    /// HMAC-SHA256 key used to sign operator session cookies.
    pub operator_secret: SessionSecret,
    /// Registry pool — backend-erasing `Pool` enum. v0.33 Phase A
    /// pinned this to Postgres; v0.34 B.2 makes it generic so a
    /// pure-SQLite stack works without any Postgres dependency.
    pub registry: crate::sql::Pool,
}

/// Extractor: resolves the request's tenant via the registry, then
/// hands the handler a database-mode connection on the configured
/// backend. The connection is owned by the extractor and released
/// to the pool when the handler returns.
///
/// Symmetric with [`super::Tenant`] for the Postgres path; pick the
/// extractor that matches your backend.
pub struct DatabaseTenant<DB: Database> {
    pub org: Org,
    conn: DatabaseConn<DB>,
}

impl<DB: Database> DatabaseTenant<DB> {
    /// Borrow the tenant-scoped connection. The inner type is
    /// `sqlx::pool::PoolConnection<DB>` — works as an sqlx executor
    /// via `&mut **t.conn()` or directly with the query macros that
    /// take `&mut Connection`.
    pub fn conn(&mut self) -> &mut DatabaseConn<DB> {
        &mut self.conn
    }

    /// Yield the underlying connection. Use when the handler is
    /// done with DB work but still has long-running computation.
    #[must_use]
    pub fn into_conn(self) -> DatabaseConn<DB> {
        self.conn
    }

    /// **Test-only** — construct directly from `(Org, DatabaseConn)`.
    /// Bypasses the resolver chain that production handlers use.
    /// Mirrors `Tenant::for_test`.
    #[cfg(any(test, feature = "test_utils"))]
    #[must_use]
    pub fn for_test(org: Org, conn: DatabaseConn<DB>) -> Self {
        Self { org, conn }
    }
}

/// Failure modes for [`DatabaseTenant`]. Same shape as
/// [`super::TenantRejection`] but lives in this module so the type
/// system catches "tried to use the wrong rejection".
#[derive(Debug)]
pub enum DatabaseTenantRejection {
    /// `DatabaseTenantContext` extension missing — the server wasn't
    /// built via the v0.33 multi-backend `Builder` flow.
    MissingContext,
    /// Resolver returned `None` — no Org matches the request.
    NotFound,
    /// Resolver or pool acquire failed at the driver level.
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
        // Resolver + context registry are both backend-erasing
        // since v0.34 B.2 — no inline wrap needed.
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
