//! `check --deploy` SSO findings across every active tenant (#2359).

use std::time::Duration;

use futures_util::StreamExt as _;

use crate::migrate::manage::DeployAuditFindings;
use crate::sql::{FetcherPool as _, Pool};
use crate::sso::check;
use crate::tenancy::org::Org;
use crate::tenancy::pools::TenantPools;
use crate::tenancy::StorageMode;

/// Tenants checked at once.
const CONCURRENCY: usize = 8;
/// A tenant whose database does not answer in time is reported, not waited on.
const TENANT_TIMEOUT: Duration = Duration::from_secs(10);

/// The shared providers, when they could be read.
#[cfg(feature = "admin-sso")]
type Shared = Option<check::SharedProviders>;
#[cfg(not(feature = "admin-sso"))]
type Shared = ();

/// One tenant's findings.
#[derive(Default)]
struct TenantFindings {
    warnings: Vec<String>,
    #[cfg(feature = "admin-sso")]
    shared: Vec<check::Stranded>,
}

impl TenantFindings {
    fn failed(slug: &str, why: impl std::fmt::Display) -> Self {
        Self {
            warnings: vec![format!(
                "[sso] tenant `{slug}`: could not check SSO providers: {why}"
            )],
            ..Self::default()
        }
    }
}

/// Providers in each active tenant (own and shared) that refuse every existing user.
pub(super) async fn findings<DB: sqlx::Database>(pools: &TenantPools<DB>) -> DeployAuditFindings
where
    Pool: From<sqlx::Pool<DB>>,
{
    use crate::core::Column as _;
    let registry = pools.registry_pool();
    let mut out = DeployAuditFindings::default();
    // The rest of `check` still runs and reports the registry itself.
    let orgs: Vec<Org> = match Org::objects()
        .where_(Org::active.eq(true))
        .fetch(&registry)
        .await
    {
        Ok(orgs) => orgs,
        Err(e) => {
            out.warnings
                .push(format!("[sso] could not list tenants: {e}"));
            return out;
        }
    };
    #[cfg(feature = "admin-sso")]
    let shared_rows: Shared = match check::SharedProviders::load(&registry).await {
        Ok(rows) => Some(rows),
        Err(e) => {
            out.warnings.push(format!(
                "[sso] could not check the shared SSO providers: {e}"
            ));
            None
        }
    };
    #[cfg(not(feature = "admin-sso"))]
    let shared_rows: Shared = ();
    let shared_rows = &shared_rows;
    let mut done: Vec<(&Org, TenantFindings)> = futures_util::stream::iter(&orgs)
        .map(|org| async move {
            let found = tokio::time::timeout(TENANT_TIMEOUT, one_tenant(pools, org, shared_rows))
                .await
                .unwrap_or_else(|_| TenantFindings::failed(&org.slug, "timed out"));
            (org, found)
        })
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;
    done.sort_by(|a, b| a.0.slug.cmp(&b.0.slug));
    #[cfg(feature = "admin-sso")]
    let mut shared: std::collections::BTreeMap<(String, check::Refusal), Vec<String>> =
        Default::default();
    for (_org, found) in done {
        out.warnings.extend(found.warnings);
        #[cfg(feature = "admin-sso")]
        for s in found.shared {
            shared
                .entry((s.slug, s.why))
                .or_default()
                .push(_org.slug.clone());
        }
    }
    #[cfg(feature = "admin-sso")]
    out.warnings.extend(
        shared
            .iter()
            .map(|((slug, why), tenants)| check::shared_warning(slug, *why, tenants)),
    );
    out
}

/// One tenant's own and shared providers that refuse every existing user.
async fn one_tenant<DB: sqlx::Database>(
    pools: &TenantPools<DB>,
    org: &Org,
    #[cfg_attr(not(feature = "admin-sso"), allow(unused_variables))] shared: &Shared,
) -> TenantFindings
where
    Pool: From<sqlx::Pool<DB>>,
{
    let tenant = match pools.scoped_pool_dyn(org).await {
        Ok(p) => p,
        // Not `{e}`: a secrets error can echo the database URL into CI logs.
        Err(_) => return TenantFindings::failed(&org.slug, "could not open the tenant pool"),
    };
    // A missing schema drops out of `search_path`, leaving `public`.
    if matches!(
        StorageMode::parse(&org.storage_mode),
        Ok(StorageMode::Schema)
    ) {
        let want = org.effective_schema();
        match crate::migrate::ensure::creation_schema(&tenant).await {
            Ok(Some(got)) if got == want => {}
            Ok(_) => {
                return TenantFindings::failed(&org.slug, format!("schema `{want}` is missing"))
            }
            Err(e) => return TenantFindings::failed(&org.slug, e),
        }
    }
    #[cfg_attr(not(feature = "admin-sso"), allow(unused_mut))]
    let mut out = match check::tenant_providers(&tenant).await {
        Ok(found) => TenantFindings {
            warnings: found
                .iter()
                .map(|s| check::tenant_warning(&org.slug, s))
                .collect(),
            ..TenantFindings::default()
        },
        Err(e) => TenantFindings::failed(&org.slug, e),
    };
    #[cfg(feature = "admin-sso")]
    if let Some(rows) = shared {
        match check::shared_providers(rows, &tenant).await {
            Ok(found) => out.shared = found,
            Err(e) => out.warnings.push(format!(
                "[sso] tenant `{}`: could not check shared SSO providers: {e}",
                org.slug
            )),
        }
    }
    out
}
