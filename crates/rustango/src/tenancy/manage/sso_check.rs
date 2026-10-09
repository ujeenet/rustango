//! `check --deploy` SSO findings across every active tenant (#2359).

use crate::migrate::manage::DeployAuditFindings;
use crate::sql::{FetcherPool as _, Pool};
use crate::sso::check;
use crate::tenancy::error::TenancyError;
use crate::tenancy::org::Org;
use crate::tenancy::pools::TenantPools;

/// Providers in each active tenant (own and shared) that refuse every existing user.
pub(super) async fn findings<DB: sqlx::Database>(
    pools: &TenantPools<DB>,
) -> Result<DeployAuditFindings, TenancyError>
where
    Pool: From<sqlx::Pool<DB>>,
{
    use crate::core::Column as _;
    let registry = pools.registry_pool();
    let orgs: Vec<Org> = Org::objects()
        .where_(Org::active.eq(true))
        .fetch(&registry)
        .await?;
    let mut out = DeployAuditFindings::default();
    #[cfg(feature = "admin-sso")]
    let mut shared: std::collections::BTreeMap<(String, check::Refusal), Vec<String>> =
        Default::default();
    for org in &orgs {
        let tenant = match pools.scoped_pool_dyn(org).await {
            Ok(p) => p,
            Err(e) => {
                out.warnings.push(format!(
                    "[sso] tenant `{}`: could not check SSO providers: {e}",
                    org.slug
                ));
                continue;
            }
        };
        match check::tenant_providers(&tenant).await {
            Ok(found) => out
                .warnings
                .extend(found.iter().map(|s| check::tenant_warning(&org.slug, s))),
            Err(e) => out.warnings.push(format!(
                "[sso] tenant `{}`: could not check SSO providers: {e}",
                org.slug
            )),
        }
        #[cfg(feature = "admin-sso")]
        match check::shared_providers(&registry, &tenant).await {
            Ok(found) => {
                for s in found {
                    shared
                        .entry((s.slug, s.why))
                        .or_default()
                        .push(org.slug.clone());
                }
            }
            Err(e) => out.warnings.push(format!(
                "[sso] tenant `{}`: could not check shared SSO providers: {e}",
                org.slug
            )),
        }
    }
    #[cfg(feature = "admin-sso")]
    out.warnings.extend(
        shared
            .iter()
            .map(|((slug, why), tenants)| check::shared_warning(slug, *why, tenants)),
    );
    Ok(out)
}
