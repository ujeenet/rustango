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

#[cfg(feature = "postgres")]
use crate::sql::sqlx::postgres::{PgPool, PgPoolOptions};
use crate::sql::sqlx::{self, Database};
use tokio::sync::RwLock;

use super::error::TenancyError;
use super::org::{Org, StorageMode};
use super::secrets::{LiteralSecretsResolver, SecretsResolver};

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

    // v0.27.7 — connection-time tuning (#60). Pre-fix, every tenant
    // pool was built with `PgPoolOptions::new().max_connections(...)`
    // and nothing else, leaving sqlx's defaults to drive timeout /
    // lifetime / idle behavior. Defaults are reasonable but apps
    // that hit slow upstreams (vault-resolved DSNs, distant
    // databases) had no way to tune them without bypassing
    // TenantPools entirely.
    /// Per-pool `min_connections` for database-mode tenants. When
    /// non-zero, sqlx keeps that many connections warm at all
    /// times — first-request latency drops because the TCP /
    /// TLS / auth round-trip is paid at boot rather than on the
    /// hot path. Recommend `1` for production tenants that get
    /// regular traffic, `0` for cold tenants with sparse hits
    /// (the default; preserves pre-0.27.7 behavior). Default: 0.
    pub database_pool_min_connections: u32,

    /// `acquire_timeout` for database-mode tenant pools — how long
    /// `pool.acquire()` waits for an available connection before
    /// erroring with `PoolTimedOut`. Sqlx's default is 30s.
    /// Default: 30s.
    pub database_pool_acquire_timeout: std::time::Duration,

    /// `idle_timeout` — close connections that have sat idle this
    /// long. `None` keeps idle connections forever (sqlx default).
    /// Set when running against a load balancer / Postgres with
    /// `idle_in_transaction_session_timeout` to avoid stale-
    /// connection errors. Default: `Some(10 minutes)`.
    pub database_pool_idle_timeout: Option<std::time::Duration>,

    /// `max_lifetime` — force a connection to be recycled after
    /// this duration, regardless of activity. Helps with rolling
    /// PG credential rotations (vault leases, cloud IAM tokens).
    /// `None` disables. Default: `Some(30 minutes)`.
    pub database_pool_max_lifetime: Option<std::time::Duration>,

    /// When `true`, [`TenantPools`] eagerly builds pools for every
    /// active database-mode tenant at construction time (`new()` /
    /// `with_secrets()`). Bounded by `max_cached_database_pools`.
    /// Schema-mode tenants are never pre-warmed (they share the
    /// registry pool which is already up). Default: `false`.
    pub prewarm_active_tenants: bool,
}

impl Default for TenantPoolsConfig {
    fn default() -> Self {
        Self {
            max_cached_database_pools: 64,
            database_pool_max_connections: 4,
            // Below: zeros / None preserve pre-0.27.7 behavior so
            // existing apps don't see surprise behavior on upgrade.
            // Apps that want hot pools opt in via `.config(...)`.
            database_pool_min_connections: 0,
            database_pool_acquire_timeout: std::time::Duration::from_secs(30),
            database_pool_idle_timeout: Some(std::time::Duration::from_secs(10 * 60)),
            database_pool_max_lifetime: Some(std::time::Duration::from_secs(30 * 60)),
            prewarm_active_tenants: false,
        }
    }
}

/// Outcome of [`TenantPools::prewarm_database_tenants`]. Counts —
/// not lists — so the type stays small enough to log + persist.
/// Per-tenant errors are written to the tracing log during
/// pre-warm; consumers needing them should subscribe to the
/// `crate::tenancy::pools` target.
#[derive(Debug, Clone, Copy, Default)]
pub struct PrewarmReport {
    /// Number of active database-mode tenants the registry returned.
    pub total_active: usize,
    /// Number of pools successfully built and cached.
    pub warmed: usize,
    /// Number of tenants whose pool build failed (skipped, not
    /// fatal — see tracing logs for details).
    pub failed: usize,
    /// Number of tenants skipped because the cache cap was already
    /// reached. Bump `TenantPoolsConfig::max_cached_database_pools`
    /// to pre-warm more.
    pub skipped_cap: usize,
}

/// One tenant's pool reference. Generic over the backend
/// (`DB = sqlx::Postgres` by default — keeps existing call sites
/// compiling unchanged). Schema-mode is fundamentally Postgres-only:
/// the variant always carries a `PgPool` and is only constructed by
/// `impl TenantPools<sqlx::Postgres>::pool_for_org`. Database-mode
/// uses `Arc<sqlx::Pool<DB>>` so a sqlite-only or MySQL-only stack
/// can use this enum without any Postgres dependency at the field
/// level.
/// Default backend for `TenantPool<DB>` / `TenantPools<DB>` /
/// `TenantConn<DB>` so existing PG call sites that write `TenantPools`
/// (no param) keep compiling. On non-PG builds the default is the
/// first available backend in priority order: sqlite → mysql.
#[cfg(feature = "postgres")]
pub type DefaultTenantDb = sqlx::Postgres;
#[cfg(all(not(feature = "postgres"), feature = "sqlite"))]
pub type DefaultTenantDb = sqlx::Sqlite;
#[cfg(all(not(feature = "postgres"), not(feature = "sqlite"), feature = "mysql"))]
pub type DefaultTenantDb = sqlx::MySql;

#[derive(Debug)]
pub enum TenantPool<DB: Database = DefaultTenantDb> {
    /// Tenant data is in a schema in the (Postgres) registry DB. The
    /// pool is the registry pool; the schema name is set on each
    /// connection acquired through [`TenantPools::acquire`]. PG-only
    /// by language — `SET search_path` doesn't exist on MySQL or
    /// SQLite. Constructed only by `impl TenantPools<sqlx::Postgres>`.
    #[cfg(feature = "postgres")]
    Schema { schema: String, registry: PgPool },
    /// Tenant data is in a dedicated DB. Pool is owned by this variant
    /// and shared via `Arc` so callers can clone cheaply.
    Database { pool: Arc<sqlx::Pool<DB>> },
}

// Manual Clone so we don't need `DB: Clone` (sqlx::Pool<DB> is
// already cheap-Arc-clone).
impl<DB: Database> Clone for TenantPool<DB> {
    fn clone(&self) -> Self {
        match self {
            #[cfg(feature = "postgres")]
            Self::Schema { schema, registry } => Self::Schema {
                schema: schema.clone(),
                registry: registry.clone(),
            },
            Self::Database { pool } => Self::Database {
                pool: Arc::clone(pool),
            },
        }
    }
}

impl<DB: Database> TenantPool<DB> {
    /// `true` when this tenant's data lives in a Postgres schema
    /// inside the registry DB. `false` for database mode (or any
    /// non-PG backend, where Schema is unreachable).
    #[must_use]
    pub fn is_schema(&self) -> bool {
        #[cfg(feature = "postgres")]
        {
            matches!(self, Self::Schema { .. })
        }
        #[cfg(not(feature = "postgres"))]
        {
            false
        }
    }
}

#[cfg(feature = "postgres")]
impl TenantPool<sqlx::Postgres> {
    /// PG-typed pool accessor — schema-mode returns the shared
    /// registry pool, database-mode returns the dedicated tenant
    /// pool. Callers running through the pool **without first issuing
    /// `SET search_path`** will hit the wrong schema in schema-mode;
    /// prefer [`TenantPools::acquire`].
    ///
    /// PG-only — use the per-variant accessors below for non-PG
    /// `TenantPool<DB>` instances (database-mode only).
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        match self {
            Self::Schema { registry, .. } => registry,
            Self::Database { pool } => pool,
        }
    }
}

/// Lazy connection registry for multi-tenant rustango. Constructed
/// once at boot from the registry pool + config + secrets resolver;
/// hands out [`TenantPool`] references at request time.
///
/// Generic over the backend (`DB = sqlx::Postgres` by default so
/// existing call sites compile unchanged). Schema-mode methods
/// (which require `SET search_path` and other Postgres-only SQL)
/// live on `impl TenantPools<sqlx::Postgres>` only — the type
/// system forbids schema-mode on non-PG. Database-mode methods are
/// generic and work on any backend.
pub struct TenantPools<DB: Database = DefaultTenantDb> {
    registry: sqlx::Pool<DB>,
    config: TenantPoolsConfig,
    secrets: Arc<dyn SecretsResolver>,
    cache: RwLock<HashMap<String, Arc<sqlx::Pool<DB>>>>,
}

impl<DB: Database> TenantPools<DB> {
    /// Construct with the default `LiteralSecretsResolver` (i.e.
    /// `Org.database_url` carries the literal URL). Existing
    /// `TenantPools::new(pg_pool)` call sites continue to work
    /// — `DB` is inferred from the pool type.
    #[must_use]
    pub fn new(registry: sqlx::Pool<DB>) -> Self {
        Self::with_secrets(registry, LiteralSecretsResolver)
    }

    /// Construct with a user-supplied [`SecretsResolver`]. Use
    /// [`super::EnvSecretsResolver`] / [`super::ChainSecretsResolver`]
    /// for env-var lookup, or implement the trait yourself for vault
    /// integration.
    #[must_use]
    pub fn with_secrets<R: SecretsResolver>(registry: sqlx::Pool<DB>, secrets: R) -> Self {
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

    /// Read access to the current config. Used by `Server::Builder`
    /// to decide whether to call [`Self::prewarm_database_tenants`]
    /// on boot. (#60, v0.27.7)
    #[must_use]
    pub fn pool_config(&self) -> &TenantPoolsConfig {
        &self.config
    }

    /// v0.38 — backend-typed accessor for the registry `sqlx::Pool<DB>`.
    /// Cheap clone (sqlx pools are Arc-shaped). Used by
    /// `tenancy::manage::server::run_server_cmd` to rebuild a fresh
    /// `Arc<TenantPools<DB>>` for the server closures without losing
    /// the original `pools`' database-mode cache.
    #[must_use]
    pub fn registry_inner(&self) -> &sqlx::Pool<DB> {
        &self.registry
    }
}

#[cfg(feature = "postgres")]
impl TenantPools<sqlx::Postgres> {
    /// The Postgres registry pool — for the legacy `&PgPool` API.
    /// Available only when the registry IS Postgres. For
    /// backend-agnostic access use [`Self::registry_pool`] (returns
    /// `&crate::sql::Pool` which dispatches per-backend through
    /// `fetch_pool` / `insert_pool` / `save_pool`).
    #[must_use]
    pub fn registry(&self) -> &PgPool {
        &self.registry
    }
}

impl<DB: Database> TenantPools<DB>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    /// Same registry pool, wrapped in the backend-erasing
    /// [`rustango::sql::Pool`] enum. Used by the v0.34 resolver
    /// chain which is generic across backends — keeps a single
    /// `TenantPools` API while letting the resolver implementations
    /// route through `fetch_pool` / `insert_pool`.
    ///
    /// Cheap: `Pool` is `Arc`-shaped under sqlx, so the clone is a
    /// reference bump.
    #[must_use]
    pub fn registry_pool(&self) -> crate::sql::Pool {
        crate::sql::Pool::from(self.registry.clone())
    }
}

// ============================================================ generic methods (any backend)

impl<DB: Database> TenantPools<DB> {
    /// Database-mode pool for `org`. Errors on schema-mode orgs —
    /// schema-mode dispatch lives on `impl TenantPools<Postgres>`
    /// only because `SET search_path` is Postgres-specific.
    ///
    /// For PG apps the public `pool_for_org` (on the PG impl) wraps
    /// this with schema-mode handling. Non-PG apps call this directly.
    ///
    /// # Errors
    /// * [`TenancyError::Validation`] for schema-mode orgs (non-PG),
    ///   malformed `storage_mode`, missing `database_url`, or cache
    ///   full.
    /// * [`TenancyError::Secrets`] / [`TenancyError::Driver`] from
    ///   the secret resolve + pool build path.
    pub async fn database_pool_for_org(&self, org: &Org) -> Result<TenantPool<DB>, TenancyError> {
        let mode = StorageMode::parse(&org.storage_mode).map_err(|got| {
            TenancyError::Validation(format!(
                "org `{}` has unknown storage_mode `{got}` (expected `schema` or `database`)",
                org.slug
            ))
        })?;
        match mode {
            StorageMode::Schema => Err(TenancyError::Validation(format!(
                "org `{}` is schema-mode but TenantPools<{}> is non-Postgres — schema-mode \
                 tenants require a Postgres registry",
                org.slug,
                std::any::type_name::<DB>(),
            ))),
            StorageMode::Database => {
                let pool = self.pool_for_database_mode(org).await?;
                Ok(TenantPool::Database { pool })
            }
        }
    }

    /// Database-mode tenant connection — errors on schema-mode orgs.
    /// Generic counterpart of [`Self::acquire`] (PG-only).
    ///
    /// # Errors
    /// As [`Self::database_pool_for_org`].
    pub async fn database_acquire(&self, org: &Org) -> Result<TenantConn<DB>, TenancyError> {
        let pool = self.database_pool_for_org(org).await?;
        let TenantPool::Database { pool } = pool else {
            unreachable!("database_pool_for_org rejects schema-mode")
        };
        let conn = pool.acquire().await?;
        Ok(TenantConn {
            inner: conn,
            schema: None,
        })
    }

    /// Drop a database-mode tenant's pool from the cache. Useful
    /// when the operator updates `Org.database_url` (vault rotation,
    /// migration to new server) and wants the next acquire to
    /// rebuild from the new URL.
    pub async fn invalidate(&self, slug: &str) {
        let mut cache = self.cache.write().await;
        cache.remove(slug);
    }

    /// Resolve `org.database_url` through the configured
    /// [`SecretsResolver`] and return the literal connection URL.
    /// Schema-mode orgs have no database_url — passing one returns
    /// [`TenancyError::Validation`].
    ///
    /// # Errors
    /// * [`TenancyError::Validation`] when `org.database_url` is `None`.
    /// * [`TenancyError::Secrets`] when the secret reference fails to
    ///   resolve.
    pub async fn resolved_database_url(&self, org: &Org) -> Result<String, TenancyError> {
        let reference = org.database_url.as_deref().ok_or_else(|| {
            TenancyError::Validation(format!(
                "org `{}` has no `database_url` to resolve (schema mode?)",
                org.slug
            ))
        })?;
        let url = self.secrets.resolve(reference).await?;
        Ok(url)
    }

    /// Number of database-mode pools currently cached. Schema-mode
    /// tenants don't count.
    pub async fn cached_database_pool_count(&self) -> usize {
        self.cache.read().await.len()
    }

    async fn pool_for_database_mode(&self, org: &Org) -> Result<Arc<sqlx::Pool<DB>>, TenancyError> {
        // Fast path: cache hit.
        {
            let cache = self.cache.read().await;
            if let Some(pool) = cache.get(&org.slug) {
                return Ok(Arc::clone(pool));
            }
        }
        // Cache miss — instrument so the cold path is visible in
        // tracing output. (#60, v0.27.7)
        let span = tracing::info_span!("tenant_pool_init", slug = %org.slug, mode = "database");
        let _enter = span.enter();
        let resolve_start = std::time::Instant::now();
        // Resolve + connect outside the write lock so vault calls
        // don't block other tenants' lookups.
        let reference = org.database_url.as_deref().ok_or_else(|| {
            TenancyError::Validation(format!(
                "org `{}` is `storage_mode = database` but has no `database_url`",
                org.slug
            ))
        })?;
        let url = self.secrets.resolve(reference).await?;
        tracing::debug!(
            target: "crate::tenancy::pools",
            slug = %org.slug,
            elapsed_ms = resolve_start.elapsed().as_millis() as u64,
            "secrets resolver resolved tenant URL",
        );
        let connect_start = std::time::Instant::now();
        let pool = build_database_pool::<DB>(&url, &self.config).await?;
        tracing::info!(
            target: "crate::tenancy::pools",
            slug = %org.slug,
            elapsed_ms = connect_start.elapsed().as_millis() as u64,
            min_conn = self.config.database_pool_min_connections,
            max_conn = self.config.database_pool_max_connections,
            "tenant pool connected (database mode)",
        );
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

impl<DB: Database> TenantPools<DB>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    /// v0.38 — backend-agnostic counterpart of
    /// [`Self::scoped_pool`] (PG-only). For schema-mode tenants on
    /// Postgres this returns `Pool::Postgres(scoped_pool)` where the
    /// pool has `search_path` baked into its connect options. For
    /// database-mode tenants (any backend) it wraps the cached
    /// tenant pool as a `crate::sql::Pool` enum. The schema-mode
    /// branch can only fire when `DB = sqlx::Postgres`; on other
    /// backends a schema-mode org returns the same
    /// [`TenancyError::Validation`] [`Self::database_pool_for_org`]
    /// would emit.
    ///
    /// # Errors
    /// As [`Self::database_pool_for_org`].
    pub async fn scoped_pool_dyn(&self, org: &Org) -> Result<crate::sql::Pool, TenancyError> {
        let mode = StorageMode::parse(&org.storage_mode).map_err(|got| {
            TenancyError::Validation(format!(
                "org `{}` has unknown storage_mode `{got}` (expected `schema` or `database`)",
                org.slug
            ))
        })?;
        match mode {
            StorageMode::Schema => {
                #[cfg(feature = "postgres")]
                {
                    // Only PG TenantPools<Postgres> can build a
                    // schema-mode scoped pool; on other backends
                    // schema-mode is forbidden by `database_pool_for_org`.
                    // Erase to `Pool::Postgres` if our DB happens to be PG.
                    if let Some(pg_pools) =
                        (self as &dyn std::any::Any).downcast_ref::<TenantPools<sqlx::Postgres>>()
                    {
                        let scoped = pg_pools.scoped_pool(org).await?;
                        return Ok(crate::sql::Pool::Postgres(scoped));
                    }
                }
                Err(TenancyError::Validation(format!(
                    "org `{}` is schema-mode but TenantPools<{}> is non-Postgres — schema-mode \
                     tenants require a Postgres registry",
                    org.slug,
                    std::any::type_name::<DB>(),
                )))
            }
            StorageMode::Database => {
                let pool = self.pool_for_database_mode(org).await?;
                Ok(crate::sql::Pool::from((*pool).clone()))
            }
        }
    }

    /// Pre-warm pools for every active database-mode tenant. Useful
    /// at boot so the *first* request per tenant doesn't pay TCP +
    /// TLS + auth + sqlx-ramp-up on the hot path. Bounded by
    /// `config.max_cached_database_pools` — extras beyond the cap
    /// are skipped with a `tracing::warn!`. Schema-mode tenants
    /// share the registry pool and are never pre-warmed.
    ///
    /// Failures on individual tenants don't abort the rest — the
    /// returned report counts successes / failures. (#60, v0.27.7)
    ///
    /// # Errors
    /// Returns [`TenancyError`] only for the registry-side `Org`
    /// query that lists active tenants. Per-tenant connect failures
    /// are surfaced in [`PrewarmReport`] without aborting the loop.
    pub async fn prewarm_database_tenants(&self) -> Result<PrewarmReport, TenancyError> {
        use crate::core::Column as _;
        use crate::sql::FetcherPool as _;
        let span = tracing::info_span!("tenant_pools_prewarm");
        let _enter = span.enter();
        let started = std::time::Instant::now();
        let registry_pool = self.registry_pool();
        let orgs: Vec<Org> = Org::objects()
            .where_(Org::storage_mode.eq("database".to_owned()))
            .where_(Org::active.eq(true))
            .fetch_pool(&registry_pool)
            .await?;
        let total = orgs.len();
        let mut report = PrewarmReport {
            total_active: total,
            warmed: 0,
            failed: 0,
            skipped_cap: 0,
        };
        for org in orgs {
            if self.cache.read().await.len() >= self.config.max_cached_database_pools {
                tracing::warn!(
                    target: "crate::tenancy::pools",
                    slug = %org.slug,
                    cap = self.config.max_cached_database_pools,
                    "skipping pre-warm: cache cap reached",
                );
                report.skipped_cap += 1;
                continue;
            }
            match self.pool_for_database_mode(&org).await {
                Ok(_) => report.warmed += 1,
                Err(e) => {
                    tracing::warn!(
                        target: "crate::tenancy::pools",
                        slug = %org.slug,
                        error = %e,
                        "pre-warm failed for tenant",
                    );
                    report.failed += 1;
                }
            }
        }
        tracing::info!(
            target: "crate::tenancy::pools",
            elapsed_ms = started.elapsed().as_millis() as u64,
            total = report.total_active,
            warmed = report.warmed,
            failed = report.failed,
            skipped_cap = report.skipped_cap,
            "prewarm complete",
        );
        Ok(report)
    }
}

// ============================================================ PG-only schema-mode methods

#[cfg(feature = "postgres")]
impl TenantPools<sqlx::Postgres> {
    /// Build (or fetch from cache) the pool for `org`. Schema-mode
    /// resolves immediately to the shared registry pool; database-mode
    /// reaches into the cache and builds-on-miss.
    ///
    /// PG-only because schema mode requires `SET search_path` which
    /// only Postgres has. For non-PG apps, call
    /// [`Self::database_pool_for_org`] directly.
    ///
    /// # Errors
    /// As [`Self::database_pool_for_org`].
    pub async fn pool_for_org(
        &self,
        org: &Org,
    ) -> Result<TenantPool<sqlx::Postgres>, TenancyError> {
        let mode = StorageMode::parse(&org.storage_mode).map_err(|got| {
            TenancyError::Validation(format!(
                "org `{}` has unknown storage_mode `{got}` (expected `schema` or `database`)",
                org.slug
            ))
        })?;
        match mode {
            StorageMode::Schema => {
                let schema = org.schema_name.clone().unwrap_or_else(|| org.slug.clone());
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
    pub async fn acquire(&self, org: &Org) -> Result<TenantConn<sqlx::Postgres>, TenancyError> {
        let pool = self.pool_for_org(org).await?;
        match &pool {
            TenantPool::Schema { schema, registry } => {
                let mut conn = registry.acquire().await?;
                let stmt = format!("SET search_path TO {}, public", quote_ident(schema));
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
                Ok(TenantConn {
                    inner: conn,
                    schema: None,
                })
            }
        }
    }

    /// Owned, tenant-scoped [`PgPool`]. In schema mode the underlying
    /// registry pool is shared and has no `search_path` set, so handing
    /// it to ORM helpers that take `&PgPool` would route queries to
    /// `public` instead of the tenant schema. This builds a small
    /// dedicated pool with `search_path` baked into connect options so
    /// every checkout is correctly scoped. Database mode just clones
    /// the cached pool.
    ///
    /// # Errors
    /// As [`Self::pool_for_org`] plus [`TenancyError::Driver`] for
    /// the schema-mode dedicated pool build.
    pub async fn scoped_pool(&self, org: &Org) -> Result<PgPool, TenancyError> {
        match self.pool_for_org(org).await? {
            TenantPool::Schema { schema, registry } => {
                let mut opts = (*registry.connect_options()).clone();
                opts = opts.options([("search_path", &format!("{schema},public") as &str)]);
                let scoped = PgPoolOptions::new()
                    .max_connections(2)
                    .connect_with(opts)
                    .await?;
                Ok(scoped)
            }
            TenantPool::Database { pool } => Ok((*pool).clone()),
        }
    }
}

/// Build a single database-mode tenant pool with the timeout /
/// keepalive / lifetime settings from `config`. Generic over the
/// backend — uses sqlx's generic `PoolOptions<DB>` so PG / MySQL /
/// SQLite all build through the same code path.
async fn build_database_pool<DB: Database>(
    url: &str,
    config: &TenantPoolsConfig,
) -> Result<sqlx::Pool<DB>, TenancyError> {
    let mut opts = sqlx::pool::PoolOptions::<DB>::new()
        .max_connections(config.database_pool_max_connections)
        .min_connections(config.database_pool_min_connections)
        .acquire_timeout(config.database_pool_acquire_timeout);
    if let Some(idle) = config.database_pool_idle_timeout {
        opts = opts.idle_timeout(idle);
    }
    if let Some(lifetime) = config.database_pool_max_lifetime {
        opts = opts.max_lifetime(lifetime);
    }
    Ok(opts.connect(url).await?)
}

/// A connection scoped to a tenant. Generic over the backend
/// (`DB = sqlx::Postgres` default — keeps existing call sites
/// compiling). For schema mode the connection was returned from the
/// shared registry pool with `search_path` pre-set (PG-only); for
/// database mode it came from the tenant's dedicated pool.
///
/// Implements `Deref` to the inner [`sqlx::pool::PoolConnection`] for
/// use as a sqlx executor.
pub struct TenantConn<DB: Database = DefaultTenantDb> {
    inner: sqlx::pool::PoolConnection<DB>,
    schema: Option<String>,
}

impl<DB: Database> TenantConn<DB> {
    /// `Some(schema)` for schema-mode connections, `None` for
    /// database-mode. Useful for diagnostics / logging.
    #[must_use]
    pub fn schema(&self) -> Option<&str> {
        self.schema.as_deref()
    }
}

impl<DB: Database> std::ops::Deref for TenantConn<DB> {
    type Target = sqlx::pool::PoolConnection<DB>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<DB: Database> std::ops::DerefMut for TenantConn<DB> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

/// Quote a Postgres identifier — wrap in double-quotes, escape any
/// embedded double-quote. Used for schema names in
/// `SET search_path` to prevent malformed slugs from breaking the
/// statement (and to defuse the trivial injection vector that would
/// exist if we string-concatenated raw schema names into SQL).
#[cfg(feature = "postgres")]
fn quote_ident(name: &str) -> String {
    let escaped = name.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "postgres")]
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

    // v0.27.7 (#60) — guard the new pool-timeout fields'
    // backward-compatible defaults. Pre-warm must default off so
    // upgrading apps don't add boot-time latency surprise; min
    // connections must default 0 so existing pools don't get
    // chatty against tiny PG instances; acquire timeout must be
    // a non-trivial duration so apps don't see PoolTimedOut on
    // a slow first connect.
    #[test]
    fn config_pool_timeout_defaults_preserve_pre_0_27_7_behavior() {
        let c = TenantPoolsConfig::default();
        assert!(!c.prewarm_active_tenants);
        assert_eq!(c.database_pool_min_connections, 0);
        assert!(c.database_pool_acquire_timeout >= std::time::Duration::from_secs(5));
        assert!(c.database_pool_idle_timeout.is_some());
        assert!(c.database_pool_max_lifetime.is_some());
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn prewarm_report_zeroed_default() {
        let r = PrewarmReport::default();
        assert_eq!(r.total_active, 0);
        assert_eq!(r.warmed, 0);
        assert_eq!(r.failed, 0);
        assert_eq!(r.skipped_cap, 0);
    }
}
