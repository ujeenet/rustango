//! Per-tenant fan-out for background sweeps.
//!
//! The framework's cleanup helpers — [`crate::media::MediaManager::purge_orphans`],
//! [`crate::audit::cleanup_older_than_pool`], [`crate::prunable::prune_all`]
//! — each take one pool. Under tenancy their tables are per-tenant, so
//! running one against a single pool cleans one tenant and leaves the
//! rest growing. [`crate::scheduler::Scheduler::every`] takes a closure
//! with no context, so it cannot supply the tenant. This module is that
//! missing loop.
//!
//! ```ignore
//! use rustango::tenancy::sweep::for_each_tenant;
//!
//! // Borrow `opts`: the closure is `Fn`, so an `async move` body can
//! // only capture `Copy` values, and `&PruneOptions` is one.
//! let opts = &opts;
//! let sweep = for_each_tenant(&pools, move |_org, pool| async move {
//!     rustango::prunable::prune_all(&pool, opts).await
//! })
//! .await?;
//!
//! // `Ok` means the sweep ran, not that every tenant succeeded.
//! // Always read the report.
//! tracing::info!(ok = sweep.succeeded(), failed = sweep.failed(), "prune sweep");
//! for (slug, err) in sweep.errors() {
//!     tracing::warn!(%slug, %err, "tenant prune failed");
//! }
//! ```
//!
//! ## What it does
//!
//! - Visits active tenants only, like [`crate::tenancy::migrate`].
//! - Never stops on one tenant's failure. A broken tenant is recorded
//!   and the loop goes on, so one bad tenant cannot starve the rest.
//! - Runs one tenant at a time. Opening N tenant pools at once turns a
//!   nightly prune into an incident. For concurrency, drive
//!   [`active_tenants`] yourself.
//! - Resolves each pool with [`TenantPools::scoped_pool_dyn`], so the
//!   closure never sees the registry pool.
//!
//! ## Watch the pool-cache cap
//!
//! Database-mode pools come from the `TenantPools` cache, capped by
//! `max_cached_database_pools` (default 64). The cache does not evict:
//! past the cap, resolving a pool errors.
//!
//! So with more active database-mode tenants than the cap, the tail of
//! the list fails on every run as [`SweepError::Pool`] while the sweep
//! still returns `Ok`. Raise the cap to at least the number of active
//! tenants before you schedule a sweep, and alert on a non-zero
//! [`TenantSweep::failed`].

use crate::core::Column as _;
use crate::sql::sqlx::Database;
use crate::sql::FetcherPool as _;

use super::error::TenancyError;
use super::org::Org;
use super::pools::TenantPools;

/// What happened for one tenant.
#[derive(Debug)]
pub struct TenantOutcome<T, E> {
    /// The tenant's slug.
    pub slug: String,
    /// `Ok` with the closure's value, `Err` if the closure failed or the
    /// tenant's pool could not be resolved (the latter arrives as
    /// [`SweepError::Pool`]).
    pub result: Result<T, SweepError<E>>,
}

/// Why one tenant's sweep did not produce a value.
#[derive(Debug, thiserror::Error)]
pub enum SweepError<E> {
    /// The tenant's pool could not be resolved — bad `storage_mode`,
    /// unresolvable `database_url`, pool cache full, upstream down.
    #[error("could not resolve pool: {0}")]
    Pool(TenancyError),
    /// The sweep closure itself returned an error for this tenant.
    #[error("sweep failed: {0}")]
    Sweep(E),
}

/// Per-tenant results of one sweep, in the order tenants were visited.
#[derive(Debug)]
pub struct TenantSweep<T, E> {
    /// One entry per active tenant.
    pub outcomes: Vec<TenantOutcome<T, E>>,
}

impl<T, E> TenantSweep<T, E> {
    /// Number of tenants the sweep completed.
    #[must_use]
    pub fn succeeded(&self) -> usize {
        self.outcomes.iter().filter(|o| o.result.is_ok()).count()
    }

    /// Number of tenants that failed. Non-zero is not fatal — inspect
    /// [`Self::errors`] to decide whether to alert.
    #[must_use]
    pub fn failed(&self) -> usize {
        self.outcomes.iter().filter(|o| o.result.is_err()).count()
    }

    /// `(slug, error)` for every tenant that failed.
    pub fn errors(&self) -> impl Iterator<Item = (&str, &SweepError<E>)> {
        self.outcomes.iter().filter_map(|o| match &o.result {
            Err(e) => Some((o.slug.as_str(), e)),
            Ok(_) => None,
        })
    }

    /// `(slug, value)` for every tenant that succeeded.
    pub fn values(&self) -> impl Iterator<Item = (&str, &T)> {
        self.outcomes.iter().filter_map(|o| match &o.result {
            Ok(v) => Some((o.slug.as_str(), v)),
            Err(_) => None,
        })
    }
}

/// Every active tenant, read from the registry.
///
/// Public so a caller who wants concurrency, ordering, batching or a
/// subset can build its own loop.
///
/// # Errors
/// Driver error reading `rustango_orgs` from the registry pool.
pub async fn active_tenants<DB>(pools: &TenantPools<DB>) -> Result<Vec<Org>, TenancyError>
where
    DB: Database,
    crate::sql::Pool: From<crate::sql::sqlx::Pool<DB>>,
{
    let registry = pools.registry_pool();
    Ok(Org::objects()
        .where_(Org::active.eq(true))
        .fetch(&registry)
        .await?)
}

/// Run `f` once per active tenant, against that tenant's own pool.
///
/// It never stops early. A tenant whose pool fails to resolve, or whose
/// closure errors, is recorded in the returned [`TenantSweep`] and the
/// loop continues. The only `Err` is a failure to read the tenant list,
/// which means there is no sweep to run at all.
///
/// # Errors
/// As [`active_tenants`].
pub async fn for_each_tenant<DB, F, Fut, T, E>(
    pools: &TenantPools<DB>,
    f: F,
) -> Result<TenantSweep<T, E>, TenancyError>
where
    DB: Database,
    crate::sql::Pool: From<crate::sql::sqlx::Pool<DB>>,
    F: Fn(Org, crate::sql::Pool) -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let orgs = active_tenants(pools).await?;
    let mut outcomes = Vec::with_capacity(orgs.len());

    for org in orgs {
        let slug = org.slug.clone();
        let pool = match pools.scoped_pool_dyn(&org).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    target: "rustango::tenancy::sweep",
                    slug = %slug,
                    error = %e,
                    "skipping tenant: could not resolve its pool",
                );
                outcomes.push(TenantOutcome {
                    slug,
                    result: Err(SweepError::Pool(e)),
                });
                continue;
            }
        };

        let result = match f(org, pool).await {
            Ok(v) => Ok(v),
            Err(e) => Err(SweepError::Sweep(e)),
        };
        if let Err(SweepError::Sweep(ref e)) = result {
            // `E` has no `Display` bound, so log only the slug; the
            // caller renders the error from `TenantSweep::errors`.
            let _ = e;
            tracing::warn!(
                target: "rustango::tenancy::sweep",
                slug = %slug,
                "tenant sweep returned an error; continuing with the remaining tenants",
            );
        }
        outcomes.push(TenantOutcome { slug, result });
    }

    Ok(TenantSweep { outcomes })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `succeeded`, `failed`, `errors` and `values` split the same set
    /// with no overlap, so an alert cannot double-count.
    #[test]
    fn sweep_accounting_partitions_outcomes() {
        let sweep: TenantSweep<u64, String> = TenantSweep {
            outcomes: vec![
                TenantOutcome {
                    slug: "acme".into(),
                    result: Ok(3),
                },
                TenantOutcome {
                    slug: "globex".into(),
                    result: Err(SweepError::Sweep("boom".to_owned())),
                },
                TenantOutcome {
                    slug: "initech".into(),
                    result: Ok(0),
                },
            ],
        };

        assert_eq!(sweep.succeeded(), 2);
        assert_eq!(sweep.failed(), 1);
        assert_eq!(sweep.succeeded() + sweep.failed(), sweep.outcomes.len());

        let values: Vec<_> = sweep.values().map(|(s, v)| (s, *v)).collect();
        assert_eq!(values, vec![("acme", 3), ("initech", 0)]);

        let errors: Vec<_> = sweep.errors().map(|(s, _)| s).collect();
        assert_eq!(errors, vec!["globex"]);
    }
}
