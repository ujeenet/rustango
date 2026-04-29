//! Tenant-aware admin — wraps `rustango-admin` with per-request
//! resolver dispatch.
//!
//! The headline UX (after Slice 6 lands per-tenant auth):
//!
//! ```ignore
//! let app = Router::new()
//!     .nest("/operator", rustango::admin::router(pools.registry().clone()))
//!     .merge(rustango_tenancy::admin::TenantAdminBuilder::new(
//!         pools.clone(),
//!         registry_url,
//!         ChainResolver::standard("app.example.com"),
//!     ).read_only(["audit_log"]).build());
//! ```
//!
//! Per-request flow:
//!
//! 1. Resolver runs against `request.parts + registry`.
//! 2. `Ok(None)` → 404.
//! 3. `Ok(Some(org))` →
//!    * **Database mode**: clones the tenant's cached `PgPool` and
//!      builds a one-shot `rustango-admin` router with it.
//!    * **Schema mode**: spins up a *short-lived* `PgPool` with an
//!      `after_connect` hook setting `search_path` so admin queries
//!      hit the tenant's schema. Dropped after the request.
//! 4. The inner router's response is returned verbatim.
//!
//! ## Costs
//!
//! Per request:
//! * 1 SQL lookup for resolver (`Org` row). v0.6+ will likely add a
//!   small TTL cache — none in slice 4.
//! * Database-mode: 0 extra connections; cached pool re-used.
//! * Schema-mode: 1+ Postgres connections per request (the
//!   short-lived pool's `after_connect` runs `SET search_path` on
//!   every fresh connection it opens; sqlx may reuse them within
//!   the request). Real cost; v0.6 may switch to a connection-level
//!   model that avoids the per-request pool build.
//! * 1 small allocator hit for the inner Router construction.
//!
//! ## What's NOT in slice 4
//!
//! * Per-tenant auth — slice 6.
//! * Operator UI bypass at the apex — caller composes via
//!   `Router::nest("/operator", admin::router(registry))` themselves.
//! * Schema-mode connection caching — slice 4 builds + drops per
//!   request. Acceptable for the demo audience; v0.6 will optimize.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use rustango::admin as rustango_admin;
use rustango::sql::sqlx::postgres::{PgPool, PgPoolOptions};
use tower::ServiceExt;
use tracing::warn;

use crate::error::TenancyError;
use crate::org::{Org, StorageMode};
use crate::pools::TenantPools;
use crate::resolver::OrgResolver;

/// Builder for the tenant-aware admin router.
pub struct TenantAdminBuilder {
    pools: Arc<TenantPools>,
    registry_url: String,
    resolver: Arc<dyn OrgResolver>,
    show_only: Option<Vec<String>>,
    read_only: Vec<String>,
}

impl TenantAdminBuilder {
    /// Build a tenant-aware admin handler.
    ///
    /// `registry_url` is the connection string used to spin up
    /// short-lived schema-mode admin pools. Database-mode tenants
    /// don't need it (their pool comes from `TenantPools`); pass
    /// any valid URL if you only have database-mode tenants.
    #[must_use]
    pub fn new(
        pools: Arc<TenantPools>,
        registry_url: impl Into<String>,
        resolver: impl OrgResolver,
    ) -> Self {
        Self {
            pools,
            registry_url: registry_url.into(),
            resolver: Arc::new(resolver),
            show_only: None,
            read_only: Vec::new(),
        }
    }

    /// Restrict the admin to these tables. Same semantics as
    /// `rustango_admin::Builder::show_only`.
    #[must_use]
    pub fn show_only<I, S>(mut self, tables: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.show_only = Some(tables.into_iter().map(Into::into).collect());
        self
    }

    /// Mark these tables read-only. Same semantics as
    /// `rustango_admin::Builder::read_only`.
    #[must_use]
    pub fn read_only<I, S>(mut self, tables: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.read_only.extend(tables.into_iter().map(Into::into));
        self
    }

    /// Build the tenant-aware `axum::Router`. Catches every request
    /// via a fallback handler — mount it under whatever prefix you
    /// want via `Router::nest`.
    #[must_use]
    pub fn build(self) -> Router {
        let pools = self.pools;
        let registry_url = Arc::new(self.registry_url);
        let resolver = self.resolver;
        let show_only = Arc::new(self.show_only);
        let read_only = Arc::new(self.read_only);

        Router::new().fallback(move |req: Request<Body>| {
            let pools = pools.clone();
            let registry_url = registry_url.clone();
            let resolver = resolver.clone();
            let show_only = show_only.clone();
            let read_only = read_only.clone();
            async move {
                handle_request(req, &pools, &registry_url, &*resolver, &show_only, &read_only).await
            }
        })
    }
}

async fn handle_request(
    req: Request<Body>,
    pools: &TenantPools,
    registry_url: &str,
    resolver: &dyn OrgResolver,
    show_only: &Option<Vec<String>>,
    read_only: &[String],
) -> Response {
    let (parts, body) = req.into_parts();
    let org = match resolver.resolve(&parts, pools.registry()).await {
        Ok(Some(o)) => o,
        Ok(None) => return (StatusCode::NOT_FOUND, "tenant not found").into_response(),
        Err(e) => {
            warn!(target: "rustango_tenancy::admin", error = %e, "resolver error");
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    let pool = match build_admin_pool_for_tenant(&org, pools, registry_url).await {
        Ok(p) => p,
        Err(e) => {
            warn!(
                target: "rustango_tenancy::admin",
                slug = %org.slug,
                error = %e,
                "tenant pool build failed",
            );
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    let admin_router = build_inner_admin_router(pool.pg_pool().clone(), show_only, read_only);

    let inner_req = Request::from_parts(parts, body);
    let response = match admin_router.oneshot(inner_req).await {
        Ok(r) => r,
        Err(_infallible) => unreachable!("axum::Router service is Infallible"),
    };

    // Schema-mode pool is dropped here when `pool` falls out of
    // scope; database-mode pools are reference-counted and stay
    // cached.
    drop(pool);
    response
}

/// Wrapper around the tenant's PgPool that owns the schema-mode
/// short-lived pool's lifetime; for database-mode it just holds an
/// `Arc<PgPool>`.
enum AdminPool {
    /// Cached database-mode pool — cheap clone of an Arc.
    Database(Arc<PgPool>),
    /// Short-lived schema-mode pool — closed when dropped.
    Schema(PgPool),
}

impl AdminPool {
    fn pg_pool(&self) -> &PgPool {
        match self {
            Self::Database(p) => p,
            Self::Schema(p) => p,
        }
    }
}

impl Drop for AdminPool {
    fn drop(&mut self) {
        // For schema-mode we'd ideally `pool.close().await` — but
        // Drop can't be async. sqlx's PgPool background reaper will
        // eventually close idle connections; not ideal but
        // acceptable for slice 4. v0.6 may move to a per-request
        // connection (no pool) to avoid this entirely.
    }
}

async fn build_admin_pool_for_tenant(
    org: &Org,
    pools: &TenantPools,
    registry_url: &str,
) -> Result<AdminPool, TenancyError> {
    let mode = StorageMode::parse(&org.storage_mode).map_err(|got| {
        TenancyError::Validation(format!(
            "org `{}` has unknown storage_mode `{got}`",
            org.slug
        ))
    })?;
    match mode {
        StorageMode::Database => {
            let tp = pools.pool_for_org(org).await?;
            match tp {
                crate::pools::TenantPool::Database { pool } => Ok(AdminPool::Database(pool)),
                crate::pools::TenantPool::Schema { .. } => unreachable!(
                    "StorageMode::Database parsed but pool_for_org returned Schema"
                ),
            }
        }
        StorageMode::Schema => {
            let schema = org.schema_name.clone().unwrap_or_else(|| org.slug.clone());
            let pool = build_short_lived_schema_pool(registry_url, &schema).await?;
            Ok(AdminPool::Schema(pool))
        }
    }
}

/// Build a short-lived `PgPool` whose every connection has its
/// `search_path` set to `<schema>, public`. Used for one admin
/// request, then dropped. Mirrors the migration helper in
/// [`crate::migrate`] but with a smaller pool size — admin
/// requests typically issue 1-3 queries.
async fn build_short_lived_schema_pool(
    registry_url: &str,
    schema: &str,
) -> Result<PgPool, TenancyError> {
    let schema_owned: Arc<str> = Arc::from(schema);
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |conn, _meta| {
            let schema = Arc::clone(&schema_owned);
            Box::pin(async move {
                let stmt = format!(
                    "SET search_path TO {}, public",
                    quote_ident(&schema)
                );
                rustango::sql::sqlx::query(&stmt).execute(conn).await?;
                Ok(())
            })
        })
        .connect(registry_url)
        .await?;
    Ok(pool)
}

fn build_inner_admin_router(
    pool: PgPool,
    show_only: &Option<Vec<String>>,
    read_only: &[String],
) -> Router {
    let mut builder = rustango_admin::Builder::new(pool);
    if let Some(allow) = show_only {
        builder = builder.show_only(allow.iter().cloned());
    }
    if !read_only.is_empty() {
        builder = builder.read_only(read_only.iter().cloned());
    }
    builder.build()
}

fn quote_ident(name: &str) -> String {
    let escaped = name.replace('"', "\"\"");
    format!("\"{escaped}\"")
}
