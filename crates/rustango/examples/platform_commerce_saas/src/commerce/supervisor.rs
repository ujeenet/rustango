//! One job queue per tenant, and a loop that keeps the set current.
//!
//! SaaS-only: the single-tenant twin has one pool and needs none of
//! this.
//!
//! ## Why one queue per tenant
//!
//! `docs/jobs.md`: *"One queue per tenant pool. A tenant id in the
//! payload is routing, not isolation."* `Job::run(&self)` receives only
//! the payload, so a job cannot scope itself — the pool the queue was
//! built on is the only thing that does. In database mode
//! `rustango_jobs` lives inside the tenant's own database; in Postgres
//! schema mode `scoped_pool_dyn` bakes `search_path` into the pool's
//! *connect options*, so a worker that lives for days stays scoped
//! without a per-query `SET`.
//!
//! ## The gap this works around (#1223)
//!
//! The framework has no per-tenant worker supervisor. Queues are built
//! at boot from the active-tenant list, so a tenant provisioned
//! afterwards gets **no workers until the process restarts** — its jobs
//! sit in its own `rustango_jobs` table, unqueued and unnoticed. The
//! refresh loop below is about forty lines and closes that, which makes
//! it the most useful thing in this example.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rustango::jobs::{DatabaseJobQueue, JobQueue as _};
use rustango::sql::{FetcherPool as _, Pool};
use rustango::tenancy::{DefaultTenantDb, Org, TenantPools};

use super::jobs;

/// Tenant pool sizing, from the environment (#1456).
///
/// Twenty tenants at the default 16 connections each, times a web and a
/// worker process, is 640 against a Postgres default `max_connections`
/// of 100 — so this has to be tunable *per deployment*, not per build.
/// It was not: `TenantPoolsConfig` existed but nothing on `Cli` reached
/// the `TenantPools` it built internally, and the soak found that
/// before it had sent a single request.
///
/// In the library rather than either binary, because the server and the
/// standalone worker must size their pools the same way. Two answers to
/// this question is how a fleet exhausts a database with every
/// individual process looking correctly configured.
#[must_use]
pub fn pool_config_from_env() -> rustango::tenancy::TenantPoolsConfig {
    fn var(name: &str) -> Option<u32> {
        std::env::var(name).ok().and_then(|v| v.parse().ok())
    }
    let mut cfg = rustango::tenancy::TenantPoolsConfig::default();
    if let Some(n) = var("TENANT_POOL_MAX_CONNECTIONS") {
        cfg.database_pool_max_connections = n;
    }
    if let Some(n) = var("TENANT_POOL_MIN_CONNECTIONS") {
        cfg.database_pool_min_connections = n;
    }
    if let Some(n) = var("TENANT_POOL_CACHE_MAX") {
        cfg.max_cached_database_pools = n as usize;
    }
    cfg
}

/// Queues by tenant slug.
///
/// `DatabaseJobQueue`'s `register`/`dispatch` are generic, so
/// `JobQueue` is not object-safe and this cannot be
/// `HashMap<String, Arc<dyn JobQueue>>`.
#[derive(Default)]
pub struct QueueMap {
    inner: RwLock<HashMap<String, Arc<DatabaseJobQueue>>>,
}

impl QueueMap {
    #[must_use]
    pub fn get(&self, slug: &str) -> Option<Arc<DatabaseJobQueue>> {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(slug)
            .cloned()
    }

    #[must_use]
    pub fn slugs(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        v.sort();
        v
    }

    fn contains(&self, slug: &str) -> bool {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(slug)
    }

    fn insert(&self, slug: String, q: Arc<DatabaseJobQueue>) {
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(slug, q);
    }

    fn remove(&self, slug: &str) -> Option<Arc<DatabaseJobQueue>> {
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(slug)
    }
}

/// Build and start a queue for one tenant. Idempotent by slug.
async fn ensure_queue(
    map: &QueueMap,
    pools: &TenantPools<DefaultTenantDb>,
    org: &Org,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    if map.contains(&org.slug) {
        return Ok(false);
    }
    // Scoped for the pool's whole lifetime, not per request.
    let pool: Pool = pools.scoped_pool_dyn(org).await?;
    DatabaseJobQueue::ensure_table_pool(&pool).await?;

    // The handlers receive no pool, so they look it up by slug.
    jobs::register_pool(&org.slug, pool.clone());

    let q = Arc::new(
        DatabaseJobQueue::with_workers_pool(pool, 2).poll_interval(Duration::from_millis(500)),
    );
    // ALL FOUR types. A worker that picks up a row whose name is not
    // registered *in this process* logs and returns without unlocking
    // it — stranded until a reclaim sweep frees it, whereupon it is
    // picked up and stranded again, and never visible in
    // `pending_count()`.
    jobs::register_all(&q).await;
    let slug_for_dl = org.slug.clone();
    q.on_dead_letter(move |dl| {
        let slug = slug_for_dl.clone();
        async move { jobs::log_dead_letter(Some(&slug), &dl) }
    })
    .await;
    q.start().await;
    tracing::info!(tenant = %org.slug, workers = 2, "tenant queue started");
    map.insert(org.slug.clone(), q);
    Ok(true)
}

/// Which running queues no longer belong to an active tenant.
///
/// A tenant that was deactivated or deleted is simply *absent* from the
/// registry query — there is no event to subscribe to, so this set
/// difference is the only signal there is. That is why the omission was
/// invisible: nothing errored, the queues just accumulated.
///
/// Taken against the live queue map rather than a remembered list, so a
/// queue started by an earlier tick is covered too.
pub fn retired_slugs(running: &[String], active: &[String]) -> Vec<String> {
    let active: std::collections::HashSet<&str> = active.iter().map(String::as_str).collect();
    running
        .iter()
        .filter(|s| !active.contains(s.as_str()))
        .cloned()
        .collect()
}

/// Stop and forget one tenant's queue.
///
/// The mirror of [`ensure_queue`], and the half this example was
/// missing. Adding without removing leaks in three ways, all of them
/// silent:
///
///   * the queue's two workers keep polling a database the tenant no
///     longer uses — and if it was dropped, every poll errors, forever;
///   * `jobs::POOLS` and `TenantPools`'s cache both keep the pool
///     alive, holding its connections against the server's limit. The
///     cache is capped at 64 with no eviction, so a long-lived process
///     that churns tenants eventually cannot open a pool at all;
///   * `shutdown_all` drains queues nobody is feeding, slowing every
///     deploy a little more.
///
/// Drain first, then evict: shutting the queue down lets in-flight jobs
/// finish on a pool that is still open. The reverse order kills them.
async fn retire_queue(map: &QueueMap, pools: &TenantPools<DefaultTenantDb>, slug: &str) {
    let Some(q) = map.remove(slug) else {
        return;
    };
    q.shutdown().await;
    jobs::unregister_pool(slug);
    // Drops the cached database-mode pool *and* the schema-mode scoped
    // pool; a tenant reactivated later gets a freshly built one, which
    // is also what makes a changed `database_url` take effect.
    pools.invalidate(slug).await;
    tracing::info!(tenant = %slug, "tenant queue retired");
}

/// Build queues for every currently-active tenant.
pub async fn boot(
    registry: &Pool,
    pools: &TenantPools<DefaultTenantDb>,
) -> Result<Arc<QueueMap>, Box<dyn std::error::Error + Send + Sync>> {
    let map = Arc::new(QueueMap::default());
    let orgs: Vec<Org> = Org::objects()
        .filter("active", true)
        .fetch(registry)
        .await?;
    for org in &orgs {
        match ensure_queue(&map, pools, org).await {
            Ok(_) => {}
            // One unreachable tenant must not stop the other nineteen
            // from getting workers.
            Err(e) => tracing::error!(tenant = %org.slug, error = %e, "queue boot failed"),
        }
    }
    tracing::info!(tenants = map.slugs().len(), "tenant job queues started");
    Ok(map)
}

/// Poll for tenants provisioned since boot and give them workers.
///
/// 15s: a tenant becomes *routable* at worst ~35s after creation (30s
/// resolver cache TTL plus the 5s fingerprint poll), so a 15s tick is
/// never the long pole. That matters — it means the latency the soak
/// measures for a new tenant is the resolver's, which is the number
/// worth knowing, rather than this loop's.
pub fn spawn_refresh(
    map: Arc<QueueMap>,
    registry: Pool,
    pools: Arc<TenantPools<DefaultTenantDb>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            tracing::debug!("tenant refresh tick");
            let orgs: Vec<Org> = match Org::objects().filter("active", true).fetch(&registry).await
            {
                Ok(o) => o,
                Err(e) => {
                    tracing::warn!(error = %e, "tenant refresh: registry query failed");
                    continue;
                }
            };
            for org in &orgs {
                match ensure_queue(&map, &pools, org).await {
                    Ok(true) => tracing::info!(tenant = %org.slug, "queue started for new tenant"),
                    Ok(false) => {}
                    Err(e) => {
                        tracing::error!(tenant = %org.slug, error = %e, "queue start failed");
                    }
                }
            }

            // And the other direction, which this loop used to skip.
            let active: Vec<String> = orgs.iter().map(|o| o.slug.clone()).collect();
            for slug in retired_slugs(&map.slugs(), &active) {
                retire_queue(&map, &pools, &slug).await;
            }
        }
    })
}

/// Drain every tenant's queue.
///
/// Called from `Cli::on_shutdown`, so it runs after the server stops
/// accepting and before the process exits — on SIGINT *and* SIGTERM
/// (#1409). `docker stop` sends SIGTERM, and each queue gives its
/// in-flight jobs a hard-coded 5s, which is why the compose file sets
/// `stop_grace_period` well above Docker's 10s default.
pub async fn shutdown_all(map: &QueueMap) {
    for slug in map.slugs() {
        if let Some(q) = map.get(&slug) {
            q.shutdown().await;
        }
    }
    tracing::info!("all tenant job queues drained");
}
