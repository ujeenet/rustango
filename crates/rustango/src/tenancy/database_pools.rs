//! Database-mode tenant pool registry for non-Postgres backends.
//!
//! Postgres tenants — including schema-mode — keep using the existing
//! [`super::TenantPools`]. This module ships the parallel structure
//! for `BackendKind::MySql` and `BackendKind::Sqlite`, both of which
//! are database-mode only (schema-mode is rejected at validation time
//! by [`super::BackendKind::validate_storage_mode`]).
//!
//! ## Why a parallel type
//!
//! `TenantPools` has 111 call sites today; making it generic over
//! `sqlx::Database` would force trait bounds onto every caller and
//! break the v0.30 dep-flip stability we just settled into. The
//! database-mode-only shape is also much simpler — no `SET
//! search_path` dance, no schema cache, no scoped-pool builder —
//! so a smaller parallel struct is the right size for the smaller
//! responsibility.
//!
//! ## Lifecycle
//!
//! - At boot the host app picks one backend type for the entire
//!   process (`Cli::tenancy::<MySql>()` or
//!   `Cli::tenancy::<Sqlite>()` — Slice 6 wires these). A single
//!   `DatabasePools<DB>` lives in the request-router state.
//! - Per request, the `Tenant<DB>` extractor (Slice 3) reads the
//!   resolved `Org` from request extensions, dispatches to
//!   [`DatabasePools::acquire`], and hands the resulting connection
//!   to the handler.
//! - The cache is keyed on `org.slug`. Builds-on-miss; evictable via
//!   [`DatabasePools::invalidate`] when `database_url` changes.

use std::collections::HashMap;
use std::sync::Arc;

use rustango::sql::sqlx;
use sqlx::pool::{Pool, PoolConnection};
use sqlx::Database;
use tokio::sync::RwLock;

use super::error::TenancyError;
use super::org::{BackendKind, Org, StorageMode};
use super::pools::TenantPoolsConfig;
use super::secrets::{LiteralSecretsResolver, SecretsResolver};

/// One tenant's pool (database-mode only, parameterized by backend
/// type). Holds an `Arc<Pool<DB>>` so callers can clone cheaply for
/// `axum::extract::State`-style sharing.
#[derive(Debug)]
pub struct DatabasePool<DB: Database> {
    pool: Arc<Pool<DB>>,
}

impl<DB: Database> Clone for DatabasePool<DB> {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
        }
    }
}

impl<DB: Database> DatabasePool<DB> {
    /// The underlying pool. Use `Pool::acquire` for connection
    /// checkout, or hand `&Pool<DB>` to sqlx query macros directly.
    #[must_use]
    pub fn pool(&self) -> &Pool<DB> {
        &self.pool
    }

    /// `Arc` clone — cheap enough that it's safe to pull out and
    /// stash in app state per request.
    #[must_use]
    pub fn pool_arc(&self) -> Arc<Pool<DB>> {
        self.pool.clone()
    }
}

/// Owned, tenant-scoped connection. Database-mode only — there is no
/// schema variant. Deref's to `PoolConnection<DB>` so the inner sqlx
/// connection is reachable for query macros / ORM helpers.
pub struct DatabaseConn<DB: Database> {
    inner: PoolConnection<DB>,
}

impl<DB: Database> std::ops::Deref for DatabaseConn<DB> {
    type Target = PoolConnection<DB>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<DB: Database> std::ops::DerefMut for DatabaseConn<DB> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

/// Lazy database-mode pool registry. Generic over the sqlx backend
/// (`Postgres` works too but the existing [`super::TenantPools`] is
/// strictly nicer for Postgres because it supports schema-mode;
/// keep this type for MySQL / SQLite).
pub struct DatabasePools<DB: Database> {
    config: TenantPoolsConfig,
    secrets: Arc<dyn SecretsResolver>,
    cache: RwLock<HashMap<String, Arc<Pool<DB>>>>,
    /// The backend type this registry serves. Validated against each
    /// `org.backend_kind` on `pool_for_org` so a mis-routed tenant
    /// fails loudly instead of silently building the wrong pool.
    backend: BackendKind,
    /// Optional URL template for tenants that don't carry an explicit
    /// `database_url`. Useful for SQLite: set
    /// `"sqlite:./tenants/{slug}.db?mode=rwc"` and every SQLite tenant
    /// gets its own file under `./tenants/` keyed by slug. `None`
    /// means "require explicit `database_url` per org" — the v0.5
    /// default and the safe-by-default production setting.
    ///
    /// The `{slug}` placeholder is replaced with `org.slug`; no other
    /// placeholders are recognized. The template is only consulted
    /// when `org.database_url is None`.
    url_template: Option<String>,
}

impl<DB: Database> DatabasePools<DB> {
    /// Construct with the default literal secrets resolver — i.e.
    /// `Org.database_url` is treated as a verbatim connection string.
    /// Pair with [`super::EnvSecretsResolver`] /
    /// [`super::ChainSecretsResolver`] via [`Self::with_secrets`]
    /// when you want env-var or vault indirection.
    #[must_use]
    pub fn new(backend: BackendKind) -> Self {
        Self::with_secrets(backend, LiteralSecretsResolver)
    }

    /// Construct with a user-supplied secrets resolver.
    #[must_use]
    pub fn with_secrets<R: SecretsResolver>(backend: BackendKind, secrets: R) -> Self {
        Self {
            config: TenantPoolsConfig::default(),
            secrets: Arc::new(secrets),
            cache: RwLock::new(HashMap::new()),
            backend,
            url_template: None,
        }
    }

    /// Set a URL template used when an org has no explicit
    /// `database_url`. The `{slug}` placeholder is replaced with the
    /// org's slug at acquire time.
    ///
    /// Most useful for SQLite, where the natural per-tenant shape is
    /// one file per slug:
    ///
    /// ```ignore
    /// let pools: DatabasePools<sqlx::Sqlite> =
    ///     DatabasePools::new(BackendKind::Sqlite)
    ///         .with_url_template("sqlite:./tenants/{slug}.db?mode=rwc");
    /// // Orgs with database_url=None now auto-route to
    /// // ./tenants/<slug>.db; sqlite creates the file on first
    /// // write thanks to `mode=rwc`.
    /// ```
    ///
    /// Returns `self` for builder chaining.
    #[must_use]
    pub fn with_url_template(mut self, template: impl Into<String>) -> Self {
        self.url_template = Some(template.into());
        self
    }

    /// Replace the config. Returns `self` for builder ergonomics.
    #[must_use]
    pub fn config(mut self, config: TenantPoolsConfig) -> Self {
        self.config = config;
        self
    }

    /// Read access to the current config.
    #[must_use]
    pub fn pool_config(&self) -> &TenantPoolsConfig {
        &self.config
    }

    /// Which backend driver this registry serves. Used by the
    /// `Tenant<DB>` extractor (Slice 3) to verify the request's
    /// `Org.backend_kind` matches the process's configured backend.
    #[must_use]
    pub fn backend_kind(&self) -> BackendKind {
        self.backend
    }

    /// Resolve (or build-on-miss) the pool for `org`.
    ///
    /// Rejects orgs whose `backend_kind` doesn't match this
    /// registry's configured backend, or whose `storage_mode` is
    /// `Schema` (this struct is database-mode-only — schema-mode is a
    /// Postgres feature, [`super::TenantPools`] handles it). Built
    /// pools are cached keyed on `slug`.
    ///
    /// # Errors
    /// * [`TenancyError::Validation`] when `org.backend_kind` doesn't
    ///   match this registry, `storage_mode` isn't `Database`, or
    ///   `database_url` is missing.
    /// * [`TenancyError::Secrets`] when the database-URL reference
    ///   fails to resolve.
    /// * [`TenancyError::Driver`] for pool-build failure.
    pub async fn pool_for_org(&self, org: &Org) -> Result<DatabasePool<DB>, TenancyError> {
        // Backend-kind match — refuse to silently build the wrong
        // backend's pool for a mis-routed tenant.
        let want = BackendKind::parse(&org.backend_kind).map_err(|got| {
            TenancyError::Validation(format!(
                "org `{}` has unknown backend_kind `{got}` \
                 (expected `postgres`, `mysql`, or `sqlite`)",
                org.slug
            ))
        })?;
        if want != self.backend {
            return Err(TenancyError::Validation(format!(
                "org `{}` is configured for backend `{want}` \
                 but this DatabasePools is serving `{}`. \
                 The process boots one backend; route through the \
                 matching server instance.",
                org.slug, self.backend
            )));
        }

        let mode = StorageMode::parse(&org.storage_mode).map_err(|got| {
            TenancyError::Validation(format!(
                "org `{}` has unknown storage_mode `{got}` \
                 (expected `schema` or `database`)",
                org.slug
            ))
        })?;
        if mode != StorageMode::Database {
            return Err(TenancyError::Validation(format!(
                "org `{}` storage_mode is `{mode}` but DatabasePools \
                 only supports database-mode. \
                 (Schema-mode is Postgres-only — use TenantPools.)",
                org.slug
            )));
        }

        // Fast path — cache hit.
        {
            let cache = self.cache.read().await;
            if let Some(pool) = cache.get(&org.slug) {
                return Ok(DatabasePool { pool: pool.clone() });
            }
        }

        // Resolve the database_url — explicit `org.database_url`
        // wins; otherwise fall back to the configured URL template
        // with `{slug}` substituted. Templates make per-tenant SQLite
        // files trivial: one config line, N orgs.
        let url_ref = match org.database_url.as_deref() {
            Some(u) => u.to_owned(),
            None => match &self.url_template {
                Some(tpl) => tpl.replace("{slug}", &org.slug),
                None => {
                    return Err(TenancyError::Validation(format!(
                        "org `{}` is database-mode but has no database_url \
                         and no url_template is configured on DatabasePools \
                         (call `.with_url_template(...)` at boot)",
                        org.slug
                    )));
                }
            },
        };
        let resolved = self
            .secrets
            .resolve(&url_ref)
            .await
            .map_err(TenancyError::Secrets)?;

        // Build the pool. The sqlx::Pool::connect path is generic
        // over DB — every backend gets the same connection-limit
        // config from TenantPoolsConfig.
        let pool = self.build_pool(&resolved).await?;
        let pool_arc = Arc::new(pool);

        // Upgrade to write lock — guard against multiple
        // concurrent builds racing.
        let mut cache = self.cache.write().await;
        // Re-check under write lock (race-loser case).
        if let Some(existing) = cache.get(&org.slug) {
            return Ok(DatabasePool {
                pool: existing.clone(),
            });
        }
        // Cap enforcement: when the cache is full and this tenant
        // isn't already present, refuse to grow. Same policy as
        // TenantPools for consistency.
        if cache.len() >= self.config.max_cached_database_pools {
            return Err(TenancyError::Validation(format!(
                "database-pool cache is at cap ({}); \
                 bump TenantPoolsConfig::max_cached_database_pools",
                self.config.max_cached_database_pools
            )));
        }
        cache.insert(org.slug.clone(), pool_arc.clone());
        Ok(DatabasePool { pool: pool_arc })
    }

    /// Acquire a connection scoped to `org`. Thin wrapper around
    /// `pool_for_org` + `pool.acquire`.
    ///
    /// # Errors
    /// As [`Self::pool_for_org`] plus [`TenancyError::Driver`] for
    /// the underlying connection checkout.
    pub async fn acquire(&self, org: &Org) -> Result<DatabaseConn<DB>, TenancyError> {
        let pool = self.pool_for_org(org).await?;
        let inner = pool.pool().acquire().await?;
        Ok(DatabaseConn { inner })
    }

    /// Drop the cached pool for `org` (if any). Used by the operator
    /// console's `database_url` edit handler — the next request
    /// rebuilds the pool with the new URL.
    pub async fn invalidate(&self, slug: &str) {
        let mut cache = self.cache.write().await;
        cache.remove(slug);
    }

    /// Build a fresh pool for `url`. Generic across backends — sqlx
    /// dispatches on the `DB` parameter.
    /// Build a fresh pool for `url` honoring every knob in
    /// `TenantPoolsConfig` — min/max connections, acquire timeout,
    /// idle timeout, max lifetime. Generic across backends; sqlx
    /// dispatches on the `DB` parameter. The Postgres-side
    /// `TenantPools` runs the same tuning; this brings MySQL and
    /// SQLite to parity.
    async fn build_pool(&self, url: &str) -> Result<Pool<DB>, TenancyError> {
        let mut opts = sqlx::pool::PoolOptions::<DB>::new()
            .max_connections(self.config.database_pool_max_connections)
            .min_connections(self.config.database_pool_min_connections)
            .acquire_timeout(self.config.database_pool_acquire_timeout);
        if let Some(idle) = self.config.database_pool_idle_timeout {
            opts = opts.idle_timeout(idle);
        }
        if let Some(lifetime) = self.config.database_pool_max_lifetime {
            opts = opts.max_lifetime(lifetime);
        }
        opts.connect(url).await.map_err(TenancyError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Compile-time tests — these prove the API surface for each
    // backend exists. Functional tests require a live DB and ship in
    // Slice 5's `tenancy_sqlite_live.rs` / `tenancy_mysql_live.rs`.

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_pool_registry_constructible() {
        let _: DatabasePools<sqlx::Sqlite> = DatabasePools::new(BackendKind::Sqlite);
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn mysql_pool_registry_constructible() {
        let _: DatabasePools<sqlx::MySql> = DatabasePools::new(BackendKind::MySql);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn backend_kind_round_trips() {
        let p: DatabasePools<sqlx::Sqlite> = DatabasePools::new(BackendKind::Sqlite);
        assert_eq!(p.backend_kind(), BackendKind::Sqlite);
    }
}
