//! Per-tenant fan-out for background sweeps (#1226).
//!
//! Every "run this from the [`scheduler`](crate::scheduler)" helper the
//! framework ships — [`crate::media::MediaLibrary::purge_orphans`],
//! [`crate::audit::cleanup_older_than_pool`],
//! [`crate::prunable::prune_all`] — takes **one pool**. In a
//! single-tenant app that is the whole story. Under tenancy each of
//! those tables is per-tenant, so a sweep wired to one pool cleans one
//! tenant (or, on the registry pool in schema mode, only `public`) and
//! reports success while every other tenant's rows accumulate forever.
//!
//! The scheduler cannot help: [`crate::scheduler::Scheduler::every`]
//! takes `Fn() -> Future` with no context, so there is nowhere for a
//! tenant to come from. This module is the missing loop.
//!
//! ```ignore
//! use rustango::tenancy::sweep::for_each_tenant;
//!
//! // Nightly, one prune per tenant:
//! let sweep = for_each_tenant(&pools, |_org, pool| async move {
//!     rustango::prunable::prune_all(&pool, &opts).await
//! })
//! .await?;
//!
//! tracing::info!(ok = sweep.succeeded(), failed = sweep.failed(), "prune sweep");
//! ```
//!
//! ## Semantics
//!
//! - **Active tenants only.** Inactive orgs are skipped, matching
//!   [`crate::tenancy::migrate`]'s fan-out.
//! - **One tenant's failure does not stop the sweep.** A broken tenant
//!   (unreachable database, rotated credential, unknown storage mode)
//!   is recorded and the loop continues — the opposite of `?`, which
//!   would let one bad tenant starve every tenant after it.
//! - **Sequential.** Sweeps are background work competing with request
//!   traffic for the same upstream; a fan-out that opened N tenant pools
//!   at once is how you turn a nightly prune into an incident. Callers
//!   who want concurrency can drive [`active_tenants`] themselves.
//! - **Pools are resolved per tenant** through
//!   [`TenantPools::scoped_pool_dyn`], so schema-mode tenants get a pool
//!   with `search_path` in its connect options and database-mode tenants
//!   get their own pool. The registry pool is never handed to the
//!   closure.

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
/// Exposed so callers who need something this module's loop does not do
/// — concurrency, ordering, batching, a subset — can build it without
/// re-deriving the query.
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
/// Never short-circuits: a tenant whose pool cannot be resolved, or
/// whose closure errors, is recorded in the returned
/// [`TenantSweep`] and the loop moves on. The only `Err` this returns is
/// a failure to read the tenant list itself, which means there is no
/// sweep to run.
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
                    target: "crate::tenancy::sweep",
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
            // `E` is not `Display`-bound (sweep closures return whatever
            // their helper returns), so log the slug and let the caller
            // render the error from `TenantSweep::errors`.
            let _ = e;
            tracing::warn!(
                target: "crate::tenancy::sweep",
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

    /// `succeeded` / `failed` / `errors` / `values` partition the same
    /// set — the accounting a caller alerts on must not double-count.
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
