//! Providers that will refuse every existing user, for `check --deploy` (#2359).
//!
//! SSO signs in by link only. A provider that cannot link by email and has
//! no link rows refuses everyone already in the user table it signs into.

use crate::core::Model;
use crate::migrate::table_exists_here;
use crate::query::QuerySet;
use crate::sql::{ExecError, ExistsPool as _, Pool};

use super::link::{any_link, LinkSource, ProviderKey};
use super::provider::load_rows;

/// Slugs of the enabled `P` rows on `providers` that refuse every `U` on
/// `users`. Missing tables count as empty.
async fn stranded<P: Model + Send, U: Model + Send>(
    providers: &Pool,
    source: LinkSource,
    users: &Pool,
) -> Result<Vec<String>, ExecError> {
    if !table_exists_here(providers, P::SCHEMA.table).await
        || !table_exists_here(users, U::SCHEMA.table).await
        || !QuerySet::<U>::new().exists(users).await?
    {
        return Ok(Vec::new());
    }
    let mut qs = QuerySet::<P>::new().filter("enabled", true);
    // The admin never links by email, whatever the row says.
    if source != LinkSource::Admin {
        qs = qs.filter("allow_email_link", false);
    }
    let has_links = table_exists_here(users, super::SsoLink::SCHEMA.table).await;
    let mut out = Vec::new();
    for row in load_rows(qs, providers).await? {
        let key = ProviderKey::for_row(source, row.id, &row.kind, row.issuer_url.as_deref());
        if !has_links || !any_link(users, &key).await? {
            out.push(row.slug);
        }
    }
    Ok(out)
}

/// The tenant's own providers that refuse every tenant user.
///
/// # Errors
/// Driver failures.
#[cfg(feature = "tenancy")]
pub async fn tenant_providers(tenant: &Pool) -> Result<Vec<String>, ExecError> {
    stranded::<super::SsoProvider, crate::tenancy::User>(tenant, LinkSource::Tenant, tenant).await
}

/// Registry-wide shared providers that refuse every user of this tenant.
/// A shared slug the tenant overrides with an enabled row of its own is skipped.
///
/// # Errors
/// Driver failures.
#[cfg(all(feature = "tenancy", feature = "admin-sso"))]
pub async fn shared_providers(registry: &Pool, tenant: &Pool) -> Result<Vec<String>, ExecError> {
    use crate::tenancy::sso::SharedSsoProvider;
    let mut out =
        stranded::<SharedSsoProvider, crate::tenancy::User>(registry, LinkSource::Shared, tenant)
            .await?;
    if !out.is_empty() && table_exists_here(tenant, super::SsoProvider::SCHEMA.table).await {
        let own = load_rows(QuerySet::<super::SsoProvider>::new(), tenant).await?;
        out.retain(|slug| !own.iter().any(|r| r.enabled && &r.slug == slug));
    }
    Ok(out)
}

/// The bare admin's providers that refuse every admin user.
///
/// # Errors
/// Driver failures.
#[cfg(feature = "admin-sso")]
pub async fn admin_providers(pool: &Pool) -> Result<Vec<String>, ExecError> {
    stranded::<super::SsoProvider, crate::admin::AdminUser>(pool, LinkSource::Admin, pool).await
}

/// `check --deploy` line for a tenant provider.
#[cfg(feature = "tenancy")]
#[must_use]
pub(crate) fn tenant_warning(tenant: &str, slug: &str) -> String {
    format!(
        "[sso] tenant `{tenant}`: provider `{slug}` has allow_email_link off and no SsoLink \
         rows, so it refuses every existing user — turn on allow_email_link or add SsoLink rows"
    )
}

/// `check --deploy` line for a shared provider, naming the tenants it refuses.
#[cfg(all(feature = "tenancy", feature = "admin-sso"))]
#[must_use]
pub(crate) fn shared_warning(slug: &str, tenants: &[String]) -> String {
    format!(
        "[sso] shared provider `{slug}` has allow_email_link off and no SsoLink rows in tenant(s) \
         {}, so it refuses every existing user there — turn on allow_email_link or add SsoLink rows",
        tenants.join(", ")
    )
}

/// `check --deploy` line for a bare-admin provider.
#[cfg(feature = "admin-sso")]
#[must_use]
pub(crate) fn admin_warning(slug: &str) -> String {
    format!(
        "[sso] admin provider `{slug}` has no SsoLink rows, so it refuses every admin user — \
         add SsoLink rows (the admin never links by email)"
    )
}
