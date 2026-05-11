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

use crate::sql::sqlx::postgres::{PgPool, PgPoolOptions};
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

/// One tenant's pool reference. Schema-mode tenants share the
/// registry pool; database-mode tenants own their dedicated pool.
#[derive(Debug, Clone)]
pub enum TenantPool {
    /// Tenant data is in a schema in the registry DB. The pool is
    /// the registry pool; the schema name is set on each
    /// connection acquired through [`TenantPools::acquire`].
    Schema { schema: String, registry: PgPool },
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
    /// [`super::EnvSecretsResolver`] / [`super::ChainSecretsResolver`]
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

    /// Read access to the current config. Used by `Server::Builder`
    /// to decide whether to call [`Self::prewarm_database_tenants`]
    /// on boot. (#60, v0.27.7)
    #[must_use]
    pub fn pool_config(&self) -> &TenantPoolsConfig {
        &self.config
    }

    /// The registry pool — for `migrate_registry`, operator-routes,
    /// and direct tenant lookups.
    #[must_use]
    pub fn registry(&self) -> &PgPool {
        &self.registry
    }

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
        crate::sql::Pool::Postgres(self.registry.clone())
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
    /// Use this for one-shot tenant work (manage commands, audit
    /// cleanup) where carrying a `&mut PgConnection` everywhere would
    /// be unwieldy. For request-path code use [`Self::acquire`] —
    /// it reuses the registry pool without spawning a new pool.
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

    /// Drop a database-mode tenant's pool from the cache. Useful
    /// when the operator updates `Org.database_url` (vault rotation,
    /// migration to new server) and wants the next `pool_for_org`
    /// to rebuild from the new URL.
    pub async fn invalidate(&self, slug: &str) {
        let mut cache = self.cache.write().await;
        cache.remove(slug);
    }

    /// Pre-warm pools for every active database-mode tenant. Useful
    /// at boot so the *first* request per tenant doesn't pay TCP +
    /// TLS + auth + sqlx-ramp-up on the hot path. Bounded by
    /// `config.max_cached_database_pools` — extras beyond the cap
    /// are skipped with a `tracing::warn!`. Schema-mode tenants
    /// share the registry pool and are never pre-warmed.
    ///
    /// Failures on individual tenants don't abort the rest — the
    /// returned report counts successes / failures. Apps that
    /// require all-or-nothing pre-warm should inspect the report
    /// and exit on non-zero `failed`. (#60, v0.27.7)
    ///
    /// # Errors
    /// Returns [`TenancyError`] only for the registry-side `Org`
    /// query that lists active tenants. Per-tenant connect failures
    /// are surfaced in the returned [`PrewarmReport`] but don't
    /// abort the loop.
    pub async fn prewarm_database_tenants(&self) -> Result<PrewarmReport, TenancyError> {
        use crate::core::Column as _;
        use crate::sql::Fetcher;
        let span = tracing::info_span!("tenant_pools_prewarm");
        let _enter = span.enter();
        let started = std::time::Instant::now();
        let orgs: Vec<Org> = Org::objects()
            .where_(Org::storage_mode.eq("database".to_owned()))
            .where_(Org::active.eq(true))
            .fetch(&self.registry)
            .await?;
        let total = orgs.len();
        let mut report = PrewarmReport {
            total_active: total,
            warmed: 0,
            failed: 0,
            skipped_cap: 0,
        };
        for org in orgs {
            // Capacity check — don't blow the cache cap.
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

    /// Resolve `org.database_url` through the configured
    /// [`SecretsResolver`] and return the literal connection URL.
    /// Called by `purge-tenant --purge-database` so it can reach the
    /// admin URL needed to issue `DROP DATABASE`. Schema-mode orgs
    /// have no database_url — passing one returns
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

    async fn pool_for_database_mode(&self, org: &Org) -> Result<Arc<PgPool>, TenancyError> {
        // Fast path: cache hit.
        {
            let cache = self.cache.read().await;
            if let Some(pool) = cache.get(&org.slug) {
                return Ok(Arc::clone(pool));
            }
        }
        // Cache miss — instrument so the cold path is visible in
        // tracing output. This is the most common cause of
        // "first-request feels slow"; the span makes it
        // grep-able. (#60, v0.27.7)
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
        let pool = build_database_pool(&url, &self.config).await?;
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

/// Build a single database-mode tenant pool with the timeout /
/// keepalive / lifetime settings from `config`. Extracted so the
/// pre-warm path uses the same options as the lazy hot-path build.
/// (#60, v0.27.7)
async fn build_database_pool(
    url: &str,
    config: &TenantPoolsConfig,
) -> Result<PgPool, TenancyError> {
    let mut opts = PgPoolOptions::new()
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

    #[test]
    fn prewarm_report_zeroed_default() {
        let r = PrewarmReport::default();
        assert_eq!(r.total_active, 0);
        assert_eq!(r.warmed, 0);
        assert_eq!(r.failed, 0);
        assert_eq!(r.skipped_cap, 0);
    }
}
