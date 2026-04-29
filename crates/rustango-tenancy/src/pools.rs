//! [`TenantPools`] — lazy connection registry for multi-tenant rustango.
//!
//! Two storage modes coexist:
//!
//! * **Schema mode** (`Org.storage_mode == "schema"`). Tenant data
//!   lives in a Postgres schema inside the registry DB. The registry
//!   pool is shared across all schema-mode tenants; per-checkout we
//!   issue `SET search_path TO <schema>, public` so queries see the
//!   tenant's schema. Cheap (one connection budget for all tenants).
//!
//! * **Database mode** (`Org.storage_mode == "database"`). Tenant data
//!   lives in a separate Postgres database. `Org.database_url` is a
//!   *secret reference* — [`SecretsResolver`] turns it into the actual
//!   connection URL, then we lazy-build a dedicated `PgPool` and cache
//!   it. Strong isolation, per-tenant connection budget.
//!
//! ## Cache shape
//!
//! Database-mode pools live in an `RwLock<HashMap<slug, Arc<PgPool>>>`.
//! Bounded by [`TenantPoolsConfig::max_cached_database_pools`] —
//! when the cache is full, the next `pool_for_org` for an
//! uncached org returns a [`TenancyError::Validation`] error. A real
//! LRU evictor lands in a follow-up; the bounded-with-error semantics
//! is the safest first version (silent eviction is its own footgun).
//! Schema-mode tenants don't consume cache slots — they always reuse
//! the registry pool.

use std::collections::HashMap;
use std::sync::Arc;

use rustango::sql::sqlx::postgres::{PgPool, PgPoolOptions};
use tokio::sync::RwLock;

use crate::error::TenancyError;
use crate::org::{Org, StorageMode};
use crate::secrets::{LiteralSecretsResolver, SecretsResolver};

/// Configuration for [`TenantPools`].
#[derive(Debug, Clone)]
pub struct TenantPoolsConfig {
    /// Maximum number of database-mode pools cached simultaneously.
    /// When the cache is full, the next uncached database-mode tenant
    /// errors out (no silent eviction). Schema-mode tenants don't
    /// count against this limit. Default: 64.
    pub max_cached_database_pools: usize,
    /// Per-pool `max_connections` for database-mode tenants. Keep
    /// small so a tenant fan-out doesn't exhaust Postgres'
    /// `max_connections`. Default: 4.
    pub database_pool_max_connections: u32,
}

impl Default for TenantPoolsConfig {
    fn default() -> Self {
        Self {
            max_cached_database_pools: 64,
            database_pool_max_connections: 4,
        }
    }
}

/// One tenant's pool reference. Schema-mode tenants share the
/// registry pool; database-mode tenants own their dedicated pool.
#[derive(Debug, Clone)]
pub enum TenantPool {
    /// Tenant data is in a schema in the registry DB. The pool is
    /// the registry pool; the schema name is set on each
    /// connection acquired through [`TenantPools::acquire`].
    Schema {
        schema: String,
        registry: PgPool,
    },
    /// Tenant data is in a dedicated DB. Pool is owned by this
    /// variant and shared via `Arc` so callers can clone cheaply.
    Database { pool: Arc<PgPool> },
}

impl TenantPool {
    /// The underlying `PgPool`. For schema mode this is the registry
    /// pool itself — callers must be aware that running queries
    /// through this pool **without first issuing
    /// `SET search_path`** will hit the wrong schema. Use
    /// [`TenantPools::acquire`] instead unless you know what you're
    /// doing.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        match self {
            Self::Schema { registry, .. } => registry,
            Self::Database { pool } => pool,
        }
    }

    /// `true` when this tenant's data lives in a Postgres schema
    /// inside the registry DB. `false` for database mode.
    #[must_use]
    pub fn is_schema(&self) -> bool {
        matches!(self, Self::Schema { .. })
    }
}

/// Lazy connection registry for multi-tenant rustango. Constructed
/// once at boot from the registry pool + config + secrets resolver;
/// hands out [`TenantPool`] references at request time.
pub struct TenantPools {
    registry: PgPool,
    config: TenantPoolsConfig,
    secrets: Arc<dyn SecretsResolver>,
    cache: RwLock<HashMap<String, Arc<PgPool>>>,
}

impl TenantPools {
    /// Construct with the default `LiteralSecretsResolver` (i.e.
    /// `Org.database_url` carries the literal URL).
    #[must_use]
    pub fn new(registry: PgPool) -> Self {
        Self::with_secrets(registry, LiteralSecretsResolver)
    }

    /// Construct with a user-supplied [`SecretsResolver`]. Use
    /// [`crate::EnvSecretsResolver`] / [`crate::ChainSecretsResolver`]
    /// for env-var lookup, or implement the trait yourself for vault
    /// integration.
    #[must_use]
    pub fn with_secrets<R: SecretsResolver>(registry: PgPool, secrets: R) -> Self {
        Self {
            registry,
            config: TenantPoolsConfig::default(),
            secrets: Arc::new(secrets),
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Replace the config. Returns `self` for builder ergonomics.
    #[must_use]
    pub fn config(mut self, config: TenantPoolsConfig) -> Self {
        self.config = config;
        self
    }

    /// The registry pool — for `migrate_registry`, operator-routes,
    /// and direct tenant lookups.
    #[must_use]
    pub fn registry(&self) -> &PgPool {
        &self.registry
    }

    /// Build (or fetch from cache) the pool for `org`. Schema-mode
    /// resolves immediately; database-mode reaches into the cache and
    /// builds-on-miss.
    ///
    /// # Errors
    /// * [`TenancyError::Validation`] when the Org row is malformed
    ///   (unrecognized `storage_mode`, missing `database_url` for
    ///   database mode, or cache full).
    /// * [`TenancyError::Secrets`] when the `database_url` reference
    ///   fails to resolve (vault outage, missing env var).
    /// * [`TenancyError::Driver`] when pool building fails (bad URL,
    ///   network, etc.).
    pub async fn pool_for_org(&self, org: &Org) -> Result<TenantPool, TenancyError> {
        let mode = StorageMode::parse(&org.storage_mode).map_err(|got| {
            TenancyError::Validation(format!(
                "org `{}` has unknown storage_mode `{got}` (expected `schema` or `database`)",
                org.slug
            ))
        })?;

        match mode {
            StorageMode::Schema => {
                let schema = org
                    .schema_name
                    .clone()
                    .unwrap_or_else(|| org.slug.clone());
                Ok(TenantPool::Schema {
                    schema,
                    registry: self.registry.clone(),
                })
            }
            StorageMode::Database => {
                let pool = self.pool_for_database_mode(org).await?;
                Ok(TenantPool::Database { pool })
            }
        }
    }

    /// Acquire a connection scoped to the tenant. For schema mode
    /// this issues `SET search_path TO <schema>, public` on the
    /// connection before handing it to the caller, so subsequent
    /// queries hit the tenant's tables. For database mode it just
    /// acquires from the dedicated pool.
    ///
    /// # Errors
    /// As [`Self::pool_for_org`] plus a [`TenancyError::Driver`] for
    /// the `SET search_path` SQL.
    pub async fn acquire(&self, org: &Org) -> Result<TenantConn, TenancyError> {
        let pool = self.pool_for_org(org).await?;
        match &pool {
            TenantPool::Schema { schema, registry } => {
                let mut conn = registry.acquire().await?;
                // `SET` (session-scoped) — every checkout from the
                // shared registry pool MUST set search_path before
                // any query, otherwise a connection that was last
                // used by tenant A would silently serve tenant B
                // queries from A's data. The TenantConn type is the
                // only way to acquire scoped connections; this is
                // the enforcement point.
                let stmt = format!(
                    "SET search_path TO {}, public",
                    quote_ident(schema)
                );
                rustango::sql::sqlx::query(&stmt)
                    .execute(&mut *conn)
                    .await?;
                Ok(TenantConn {
                    inner: conn,
                    schema: Some(schema.clone()),
                })
            }
            TenantPool::Database { pool } => {
                let conn = pool.acquire().await?;
                Ok(TenantConn { inner: conn, schema: None })
            }
        }
    }

    /// Drop a database-mode tenant's pool from the cache. Useful
    /// when the operator updates `Org.database_url` (vault rotation,
    /// migration to new server) and wants the next `pool_for_org`
    /// to rebuild from the new URL.
    pub async fn invalidate(&self, slug: &str) {
        let mut cache = self.cache.write().await;
        cache.remove(slug);
    }

    /// Number of database-mode pools currently cached. Schema-mode
    /// tenants don't count.
    pub async fn cached_database_pool_count(&self) -> usize {
        self.cache.read().await.len()
    }

    async fn pool_for_database_mode(&self, org: &Org) -> Result<Arc<PgPool>, TenancyError> {
        // Fast path: cache hit.
        {
            let cache = self.cache.read().await;
            if let Some(pool) = cache.get(&org.slug) {
                return Ok(Arc::clone(pool));
            }
        }
        // Resolve + connect outside the write lock so vault calls
        // don't block other tenants' lookups.
        let reference = org.database_url.as_deref().ok_or_else(|| {
            TenancyError::Validation(format!(
                "org `{}` is `storage_mode = database` but has no `database_url`",
                org.slug
            ))
        })?;
        let url = self.secrets.resolve(reference).await?;
        let pool = PgPoolOptions::new()
            .max_connections(self.config.database_pool_max_connections)
            .connect(&url)
            .await?;
        let pool = Arc::new(pool);

        // Insert under write lock; check for race + capacity.
        let mut cache = self.cache.write().await;
        if let Some(existing) = cache.get(&org.slug) {
            return Ok(Arc::clone(existing));
        }
        if cache.len() >= self.config.max_cached_database_pools {
            return Err(TenancyError::Validation(format!(
                "tenant pool cache is full ({} cached); raise \
                 `TenantPoolsConfig::max_cached_database_pools` or \
                 invalidate idle tenants",
                cache.len(),
            )));
        }
        cache.insert(org.slug.clone(), Arc::clone(&pool));
        Ok(pool)
    }
}

/// A connection scoped to a tenant. For schema mode the connection
/// was returned from the shared registry pool with `search_path`
/// pre-set; for database mode it came from the tenant's dedicated
/// pool. Either way, queries via this connection see the tenant's
/// data and only the tenant's data.
///
/// Implements `Deref` to the inner `PoolConnection` for use as a
/// sqlx executor. When dropped, the connection returns to its pool;
/// no explicit `RESET` is issued because the next checkout from a
/// schema-mode pool always issues a fresh `SET` before any query.
pub struct TenantConn {
    inner: rustango::sql::sqlx::pool::PoolConnection<rustango::sql::sqlx::Postgres>,
    schema: Option<String>,
}

impl TenantConn {
    /// `Some(schema)` for schema-mode connections, `None` for
    /// database-mode. Useful for diagnostics / logging.
    #[must_use]
    pub fn schema(&self) -> Option<&str> {
        self.schema.as_deref()
    }
}

impl std::ops::Deref for TenantConn {
    type Target = rustango::sql::sqlx::pool::PoolConnection<rustango::sql::sqlx::Postgres>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::ops::DerefMut for TenantConn {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

/// Quote a Postgres identifier — wrap in double-quotes, escape any
/// embedded double-quote. Used for schema names in
/// `SET search_path` to prevent malformed slugs from breaking the
/// statement (and to defuse the trivial injection vector that would
/// exist if we string-concatenated raw schema names into SQL).
fn quote_ident(name: &str) -> String {
    let escaped = name.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_ident_wraps_and_escapes() {
        assert_eq!(quote_ident("acme"), "\"acme\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_ident(""), "\"\"");
    }

    #[test]
    fn config_defaults_are_sane() {
        let c = TenantPoolsConfig::default();
        assert!(c.max_cached_database_pools >= 16);
        assert!(c.database_pool_max_connections >= 1);
    }
}
