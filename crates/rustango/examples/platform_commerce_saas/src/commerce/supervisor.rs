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
        async move {
            tracing::error!(
                tenant = %slug,
                job = dl.name,
                attempts = dl.attempts,
                error = %dl.error,
                "job dead-lettered"
            );
        }
    })
    .await;
    q.start().await;
    map.insert(org.slug.clone(), q);
    Ok(true)
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
