//! Providers that will refuse every existing user, for `check --deploy` (#2359).
//!
//! SSO signs in by link only. A provider with no link rows signs in nobody
//! already in its user table unless email linking is on and some user may use
//! it: privileged accounts never link by email.

use crate::core::Model;
use crate::migrate::table_exists_here;
use crate::query::QuerySet;
use crate::sql::{ExecError, ExistsPool as _, Pool};

use super::link::{any_link, LinkSource, ProviderKey};
use super::provider::load_rows;

/// Why a provider refuses every existing user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Refusal {
    /// `allow_email_link` is off, or unreadable.
    LinkingOff,
    /// Email linking is on, but every active user is privileged.
    OnlyPrivileged,
}

/// A provider that refuses every existing user.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Stranded {
    pub slug: String,
    pub why: Refusal,
}

/// An enabled provider row with no link rows.
struct Unlinked {
    slug: String,
    allow_email_link: bool,
}

/// Ids of the `P` rows with email linking on. Read apart from the row, as at
/// sign-in, so a table without the column reads as off.
async fn email_linking<P: Model + Send>(pool: &Pool) -> Vec<i64> {
    QuerySet::<P>::new()
        .filter("allow_email_link", true)
        .values_list_flat("id")
        .fetch::<i64>(pool)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(target: "rustango::sso", "allow_email_link unreadable, treated as off: {e}");
            Vec::new()
        })
}

/// The enabled `P` rows on `providers` with no link on `users`; none when
/// `U` has no rows. Missing tables count as empty.
async fn unlinked<P: Model + Send, U: Model + Send>(
    providers: &Pool,
    source: LinkSource,
    users: &Pool,
) -> Result<Vec<Unlinked>, ExecError> {
    if !table_exists_here(providers, P::SCHEMA.table).await
        || !table_exists_here(users, U::SCHEMA.table).await
        || !QuerySet::<U>::new().exists(users).await?
    {
        return Ok(Vec::new());
    }
    // The admin never links by email, whatever the row says.
    let linking = if source == LinkSource::Admin {
        Vec::new()
    } else {
        email_linking::<P>(providers).await
    };
    let has_links = table_exists_here(users, super::SsoLink::SCHEMA.table).await;
    let mut out = Vec::new();
    for row in load_rows(QuerySet::<P>::new().filter("enabled", true), providers).await? {
        let key = ProviderKey::for_row(source, row.id, &row.kind, row.issuer_url.as_deref());
        if !has_links || !any_link(users, &key).await? {
            out.push(Unlinked {
                allow_email_link: linking.contains(&row.id),
                slug: row.slug,
            });
        }
    }
    Ok(out)
}

/// The `rows` that refuse every tenant user: linking off, or no user email
/// linking may sign in.
#[cfg(feature = "tenancy")]
async fn refusing_tenant_users(
    rows: Vec<Unlinked>,
    tenant: &Pool,
) -> Result<Vec<Stranded>, ExecError> {
    let mut linkable = None;
    let mut out = Vec::new();
    for row in rows {
        let why = if row.allow_email_link {
            if linkable.is_none() {
                linkable = Some(crate::tenancy::member_auth::any_email_linkable(tenant).await?);
            }
            if linkable == Some(true) {
                continue;
            }
            Refusal::OnlyPrivileged
        } else {
            Refusal::LinkingOff
        };
        out.push(Stranded {
            slug: row.slug,
            why,
        });
    }
    Ok(out)
}

/// The tenant's own providers that refuse every tenant user.
///
/// # Errors
/// Driver failures.
#[cfg(feature = "tenancy")]
pub async fn tenant_providers(tenant: &Pool) -> Result<Vec<Stranded>, ExecError> {
    let rows =
        unlinked::<super::SsoProvider, crate::tenancy::User>(tenant, LinkSource::Tenant, tenant)
            .await?;
    refusing_tenant_users(rows, tenant).await
}

/// Registry-wide shared providers that refuse every user of this tenant.
/// A shared slug the tenant overrides with an enabled row of its own is skipped.
///
/// # Errors
/// Driver failures.
#[cfg(all(feature = "tenancy", feature = "admin-sso"))]
pub async fn shared_providers(registry: &Pool, tenant: &Pool) -> Result<Vec<Stranded>, ExecError> {
    use crate::tenancy::sso::SharedSsoProvider;
    let mut rows =
        unlinked::<SharedSsoProvider, crate::tenancy::User>(registry, LinkSource::Shared, tenant)
            .await?;
    if !rows.is_empty() && table_exists_here(tenant, super::SsoProvider::SCHEMA.table).await {
        let own = load_rows(QuerySet::<super::SsoProvider>::new(), tenant).await?;
        rows.retain(|u| !own.iter().any(|r| r.enabled && r.slug == u.slug));
    }
    refusing_tenant_users(rows, tenant).await
}

/// The bare admin's providers that refuse every admin user.
///
/// # Errors
/// Driver failures.
#[cfg(feature = "admin-sso")]
pub async fn admin_providers(pool: &Pool) -> Result<Vec<String>, ExecError> {
    Ok(
        unlinked::<super::SsoProvider, crate::admin::AdminUser>(pool, LinkSource::Admin, pool)
            .await?
            .into_iter()
            .map(|u| u.slug)
            .collect(),
    )
}

/// What to do about `why`, for the warning lines.
#[cfg(feature = "tenancy")]
fn refusal_text(why: Refusal) -> &'static str {
    match why {
        Refusal::LinkingOff => {
            "has allow_email_link off and no SsoLink rows, so it refuses every existing user — \
             turn on allow_email_link or add SsoLink rows"
        }
        Refusal::OnlyPrivileged => {
            "has no SsoLink rows and every active user is privileged, so it refuses every \
             existing user — privileged accounts never link by email, add SsoLink rows"
        }
    }
}

/// `check --deploy` line for a tenant provider.
#[cfg(feature = "tenancy")]
#[must_use]
pub(crate) fn tenant_warning(tenant: &str, s: &Stranded) -> String {
    format!(
        "[sso] tenant `{tenant}`: provider `{}` {}",
        s.slug,
        refusal_text(s.why)
    )
}

/// `check --deploy` line for a shared provider, naming the tenants it refuses.
#[cfg(all(feature = "tenancy", feature = "admin-sso"))]
#[must_use]
pub(crate) fn shared_warning(slug: &str, why: Refusal, tenants: &[String]) -> String {
    format!(
        "[sso] shared provider `{slug}` (tenant(s) {}) {}",
        tenants.join(", "),
        refusal_text(why)
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
