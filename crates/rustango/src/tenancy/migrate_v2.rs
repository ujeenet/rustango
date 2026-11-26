//! Engine v2 under tenancy (8.1-8.4): the registry chain once, on the
//! registry database, then each tenant's chain on its schema (PG) or its
//! database, under one fleet lock and record.

use std::path::Path;

use sqlx::Database;

use super::error::TenancyError;
use super::org::{Org, StorageMode};
use super::pools::TenantPools;
use crate::migrate::v2::run::{self, Options, RunAs};
use crate::migrate::LockScope;

/// One tenant's v2 run: its pool, its lock and its login role.
pub(crate) struct Target {
    pub(crate) slug: String,
    pub(crate) pool: crate::sql::Pool,
    pub(crate) scope: LockScope,
    /// The tenant's own login, which gets no write access to the ledger.
    pub(crate) role: Option<String>,
    /// The fleet made this pool (schema mode) and closes it after.
    owned: bool,
}

/// `org`'s v2 target. A schema tenant gets a pool whose path is its schema
/// alone: nothing falls through to `public` (decision 1).
///
/// # Errors
/// An unreachable database, a foreign schema, or schema mode off PG.
pub(crate) async fn target<DB: Database>(
    pools: &TenantPools<DB>,
    org: &Org,
    registry_url: &str,
) -> Result<Target, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    if org.storage_mode == StorageMode::Schema {
        #[cfg(feature = "postgres")]
        {
            let pg = (pools as &dyn std::any::Any)
                .downcast_ref::<TenantPools<sqlx::Postgres>>()
                .ok_or_else(|| {
                    TenancyError::Validation(format!(
                        "org `{}` is schema-mode but the registry is not Postgres",
                        org.slug
                    ))
                })?;
            let schema = org.effective_schema().to_owned();
            let timeout = pg.pool_config().database_pool_acquire_timeout;
            super::migrate::ensure_tenant_schema(registry_url, &schema, timeout).await?;
            let pool = schema_only_pool(registry_url, &schema, timeout).await?;
            let role = pg.ensure_tenant_role(&schema).await?;
            return Ok(Target {
                slug: org.slug.clone(),
                pool: pool.into(),
                scope: LockScope::Schema(schema),
                role,
                owned: true,
            });
        }
        #[cfg(not(feature = "postgres"))]
        return Err(TenancyError::Validation(format!(
            "org `{}` is schema-mode, which needs Postgres",
            org.slug
        )));
    }
    let _ = registry_url;
    let pool = match pools.database_pool_for_org(org).await? {
        super::pools::TenantPool::Database { pool } => crate::sql::Pool::from((*pool).clone()),
        #[cfg(feature = "postgres")]
        super::pools::TenantPool::Schema { .. } => {
            unreachable!("database_pool_for_org refuses schema mode")
        }
    };
    Ok(Target {
        slug: org.slug.clone(),
        pool,
        scope: LockScope::Session,
        role: None,
        owned: false,
    })
}

/// A small pool on the registry database whose every connection's path is
/// `schema` alone.
#[cfg(feature = "postgres")]
async fn schema_only_pool(
    registry_url: &str,
    schema: &str,
    timeout: std::time::Duration,
) -> Result<sqlx::PgPool, TenancyError> {
    use crate::sql::Dialect as _;
    let path = format!(
        "SET search_path TO {}",
        crate::sql::Postgres.quote_ident(schema)
    );
    Ok(sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(timeout)
        .after_connect(move |conn, _| {
            let path = path.clone();
            Box::pin(async move {
                use sqlx::Executor as _;
                conn.execute(sqlx::raw_sql(&path)).await.map(drop)
            })
        })
        .connect(registry_url)
        .await?)
}

/// How a fleet run goes.
#[derive(Debug, Clone, Default)]
pub(crate) struct FleetOptions {
    /// Stop after more than this many tenants failed; `None` runs them all.
    pub(crate) max_failures: Option<usize>,
}

/// One tenant's result.
#[derive(Debug, Clone)]
pub(crate) struct TenantOutcome {
    pub(crate) slug: String,
    pub(crate) applied: Vec<String>,
    /// The error class (as the registry cache holds it).
    pub(crate) error_class: Option<String>,
    /// The full error, for the operator's log only.
    pub(crate) error: Option<String>,
}

/// What a fleet run did.
#[derive(Debug, Clone, Default)]
pub(crate) struct FleetReport {
    pub(crate) registry: Vec<String>,
    pub(crate) tenants: Vec<TenantOutcome>,
    /// `--max-failures` stopped the run; the tenants after it did not run.
    pub(crate) stopped: bool,
}

impl FleetReport {
    /// The tenants that failed.
    pub(crate) fn failed(&self) -> impl Iterator<Item = &TenantOutcome> {
        self.tenants.iter().filter(|t| t.error_class.is_some())
    }
}

fn migrate_err(e: run::RunError) -> TenancyError {
    TenancyError::Migrate(e.into())
}

/// The registry chain, then every active tenant's, under the fleet lock
/// (8.3). A registry failure is an error; a tenant failure is recorded in
/// the registry cache (its error class only) and the run goes on, within
/// `max_failures`. Every tenant is planned first: one blocker refuses them
/// all before any change.
///
/// # Errors
/// The registry chain, a blocker in any tenant, or reading the orgs.
pub(crate) async fn run_fleet<DB: Database>(
    pools: &TenantPools<DB>,
    dir: &Path,
    registry_url: &str,
    fo: &FleetOptions,
) -> Result<FleetReport, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    use crate::core::Column as _;
    let registry = pools.registry_pool();
    let result: Result<Result<FleetReport, TenancyError>, crate::migrate::MigrateError> =
        run::fleet_locked(&registry, || async {
            use crate::core::Model as _;
            // A fresh registry has no orgs table until its chain runs.
            let (orgs, unreadable) =
                if crate::migrate::try_table_exists_here(&registry, Org::SCHEMA.table).await? {
                    Org::fetch_reporting(Org::objects().where_(Org::active.eq(true)), &registry)
                        .await
                        .map_err(crate::migrate::MigrateError::from)?
                } else {
                    (Vec::new(), Vec::new())
                };
            Ok(fleet_body(
                pools,
                &registry,
                dir,
                registry_url,
                fo,
                orgs,
                unreadable.len(),
            )
            .await)
        })
        .await;
    result?
}

async fn fleet_body<DB: Database>(
    pools: &TenantPools<DB>,
    registry: &crate::sql::Pool,
    dir: &Path,
    registry_url: &str,
    fo: &FleetOptions,
    orgs: Vec<Org>,
    unreadable: usize,
) -> Result<FleetReport, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let mut targets = Vec::new();
    let mut report = FleetReport::default();
    for org in &orgs {
        match target(pools, org, registry_url).await {
            Ok(t) => targets.push(t),
            Err(e) => report.tenants.push(TenantOutcome {
                slug: org.slug.clone(),
                applied: Vec::new(),
                error_class: Some("unreachable".into()),
                error: Some(e.to_string()),
            }),
        }
    }
    if unreadable > 0 {
        tracing::warn!(
            target: "rustango::tenancy",
            unreadable,
            "org rows the fleet could not read are not migrated"
        );
    }
    // `requires: tenants:<id>` holds when every tenant has applied it.
    let tenant = |t: &Target| Options {
        run_as: RunAs::Tenant,
        lock_scope: t.scope.clone(),
        ..Options::default()
    };
    let mut everywhere: Option<std::collections::BTreeSet<String>> = None;
    for t in &targets {
        let ids = run::applied_ids(&t.pool, &tenant(t))
            .await
            .map_err(migrate_err)?;
        everywhere = Some(match everywhere {
            None => ids,
            Some(all) => all.intersection(&ids).cloned().collect(),
        });
    }
    let reg = Options {
        run_as: RunAs::Registry,
        tenants_applied: Some(everywhere.unwrap_or_default()),
        ..Options::default()
    };
    report.registry = run::migrate(registry, dir, &reg)
        .await
        .map_err(migrate_err)?
        .applied;
    let registry_applied = run::applied_ids(registry, &reg)
        .await
        .map_err(migrate_err)?;
    let opts = |t: &Target, dry_run: bool| Options {
        registry_applied: Some(registry_applied.clone()),
        revoke_from: t.role.clone(),
        dry_run,
        ..tenant(t)
    };
    // Plan every tenant before any change: a refusal anywhere stops all.
    let mut blockers = Vec::new();
    for t in &targets {
        if let Err(e) = run::migrate(&t.pool, dir, &opts(t, true)).await {
            if e.code().is_some() {
                blockers.push(format!("  - {}: {e}", t.slug));
            }
        }
    }
    if !blockers.is_empty() {
        return Err(TenancyError::Validation(format!(
            "the fleet is blocked; no tenant was changed:\n{}",
            blockers.join("\n")
        )));
    }
    let run_id = run::new_run_id();
    let mut failures = report.tenants.len();
    for t in &targets {
        if fo.max_failures.is_some_and(|m| failures > m) {
            report.stopped = true;
            break;
        }
        crate::migrate::v2::ledger::cache_put(registry, &t.slug, "running", None, None, &run_id)
            .await?;
        match run::migrate(&t.pool, dir, &opts(t, false)).await {
            Ok(rep) => {
                let head = run::applied_ids(&t.pool, &tenant(t))
                    .await
                    .ok()
                    .and_then(|ids| ids.into_iter().filter(|i| !i.contains(':')).max());
                crate::migrate::v2::ledger::cache_put(
                    registry,
                    &t.slug,
                    "applied",
                    head.as_deref(),
                    None,
                    &run_id,
                )
                .await?;
                report.tenants.push(TenantOutcome {
                    slug: t.slug.clone(),
                    applied: rep.applied,
                    error_class: None,
                    error: None,
                });
            }
            Err(e) => {
                let class = run::error_class(&e);
                crate::migrate::v2::ledger::cache_put(
                    registry,
                    &t.slug,
                    "failed",
                    None,
                    Some(class),
                    &run_id,
                )
                .await?;
                tracing::warn!(
                    target: "rustango::tenancy",
                    slug = %t.slug,
                    error = %e,
                    "tenant migration failed; continuing with the rest"
                );
                failures += 1;
                report.tenants.push(TenantOutcome {
                    slug: t.slug.clone(),
                    applied: Vec::new(),
                    error_class: Some(class.to_owned()),
                    error: Some(e.to_string()),
                });
            }
        }
    }
    for t in targets.into_iter().filter(|t| t.owned) {
        t.pool.close().await;
    }
    Ok(report)
}

#[cfg(test)]
#[path = "migrate_v2_tests.rs"]
mod tests;
