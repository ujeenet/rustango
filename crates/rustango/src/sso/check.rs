//! Providers that will refuse every existing user, for `check --deploy` (#2359).
//!
//! SSO signs in by link only. A provider with no link rows signs in nobody
//! already in its user table unless email linking is on and some user may use
//! it: privileged accounts never link by email.

use crate::core::Model;
use crate::migrate::try_table_exists_here;
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

/// An enabled provider row.
#[derive(Debug, Clone)]
struct Candidate {
    slug: String,
    key: ProviderKey,
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

/// The enabled `P` rows on `pool`. A missing table counts as empty, as at sign-in.
async fn candidates<P: Model + Send>(
    pool: &Pool,
    source: LinkSource,
) -> Result<Vec<Candidate>, ExecError> {
    let rows = load_rows(QuerySet::<P>::new().filter("enabled", true), pool).await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    // The admin never links by email, whatever the row says.
    let linking = if source == LinkSource::Admin {
        Vec::new()
    } else {
        email_linking::<P>(pool).await
    };
    Ok(rows
        .into_iter()
        .map(|r| Candidate {
            key: ProviderKey::for_row(source, r.id, &r.kind, r.issuer_url.as_deref()),
            allow_email_link: linking.contains(&r.id),
            slug: r.slug,
        })
        .collect())
}

/// The `rows` with no link on `users`; none when `U` has no rows. Missing
/// tables count as empty.
async fn unlinked<U: Model + Send>(
    rows: Vec<Candidate>,
    users: &Pool,
) -> Result<Vec<Candidate>, ExecError> {
    if rows.is_empty()
        || !try_table_exists_here(users, U::SCHEMA.table).await?
        || !QuerySet::<U>::new().exists(users).await?
    {
        return Ok(Vec::new());
    }
    if !try_table_exists_here(users, super::SsoLink::SCHEMA.table).await? {
        return Ok(rows);
    }
    let mut out = Vec::new();
    for row in rows {
        if !any_link(users, &row.key).await? {
            out.push(row);
        }
    }
    Ok(out)
}

/// The `rows` that refuse every tenant user: no link, and linking off or no
/// user email linking may sign in.
#[cfg(feature = "tenancy")]
async fn refusing_tenant_users(
    rows: Vec<Candidate>,
    tenant: &Pool,
) -> Result<Vec<Stranded>, ExecError> {
    let mut linkable = None;
    let mut out = Vec::new();
    for row in unlinked::<crate::tenancy::User>(rows, tenant).await? {
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
    let rows = candidates::<super::SsoProvider>(tenant, LinkSource::Tenant).await?;
    refusing_tenant_users(rows, tenant).await
}

/// The registry's enabled shared providers, read once for every tenant.
#[cfg(all(feature = "tenancy", feature = "admin-sso"))]
#[derive(Debug, Clone)]
pub struct SharedProviders(Vec<Candidate>);

#[cfg(all(feature = "tenancy", feature = "admin-sso"))]
impl SharedProviders {
    /// # Errors
    /// Driver failures.
    pub async fn load(registry: &Pool) -> Result<Self, ExecError> {
        candidates::<crate::tenancy::sso::SharedSsoProvider>(registry, LinkSource::Shared)
            .await
            .map(Self)
    }
}

/// The `shared` providers that refuse every user of this tenant. A shared
/// slug the tenant overrides with an enabled row of its own is skipped.
///
/// # Errors
/// Driver failures.
#[cfg(all(feature = "tenancy", feature = "admin-sso"))]
pub async fn shared_providers(
    shared: &SharedProviders,
    tenant: &Pool,
) -> Result<Vec<Stranded>, ExecError> {
    let mut rows = shared.0.clone();
    if !rows.is_empty() {
        // The sign-in read: a tenant without the table overrides nothing.
        let own = load_rows(QuerySet::<super::SsoProvider>::new(), tenant).await?;
        rows.retain(|c| !own.iter().any(|r| r.enabled && r.slug == c.slug));
    }
    refusing_tenant_users(rows, tenant).await
}

/// The bare admin's providers that refuse every admin user.
///
/// # Errors
/// Driver failures.
#[cfg(feature = "admin-sso")]
pub async fn admin_providers(pool: &Pool) -> Result<Vec<String>, ExecError> {
    let rows = candidates::<super::SsoProvider>(pool, LinkSource::Admin).await?;
    let rows = unlinked::<crate::admin::AdminUser>(rows, pool).await?;
    Ok(rows.into_iter().map(|c| c.slug).collect())
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

/// `check --deploy` line for a tenant that could not be checked.
#[cfg(feature = "tenancy")]
#[must_use]
pub(crate) fn tenant_error(tenant: &str, why: impl std::fmt::Display) -> String {
    format!("[sso] tenant `{tenant}`: could not check SSO providers: {why}")
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
