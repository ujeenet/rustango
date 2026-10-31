//! Providers that will refuse every existing user, for `check --deploy` (#2359).
//!
//! SSO signs in by link only. A provider with no link rows signs in nobody
//! already in its user table unless email linking is on and some user may use
//! it (privileged accounts never link by email), or sign-up is not closed.

use crate::core::Model;
use crate::migrate::try_table_exists_here;
use crate::query::QuerySet;
use crate::sql::{ExecError, ExistsPool as _, Pool};

use super::link::{any_link, LinkSource, ProviderKey};
use super::provider::load_rows;

/// Why a provider refuses every existing user.
#[cfg(feature = "tenancy")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Refusal {
    /// `allow_email_link` is off, or unreadable.
    LinkingOff,
    /// Email linking is on, but every active user is privileged.
    OnlyPrivileged,
    /// Not a refusal: sign-up is open and no domain rule narrows it.
    OpenSignup,
}

/// A provider that refuses every existing user.
#[cfg(feature = "tenancy")]
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
    /// Only tenant users link by email; the admin never does.
    #[cfg(feature = "tenancy")]
    allow_email_link: bool,
    /// The row's sign-up mode; unreadable reads as closed.
    #[cfg(feature = "tenancy")]
    signup: super::policy::SignupMode,
    /// Open sign-up with no email domain, hosted domain or IdP tenant rule.
    #[cfg(feature = "tenancy")]
    open_to_anyone: bool,
}

/// Each `P` row's sign-up mode and whether a domain rule narrows it. A table
/// without the columns, or a value that does not parse, reads as closed.
#[cfg(feature = "tenancy")]
async fn signup_modes<P: Model + Send>(
    pool: &Pool,
) -> std::collections::HashMap<i64, (super::policy::SignupMode, bool)> {
    use crate::core::SqlValue;
    let cols = [
        "id",
        "signup_mode",
        "allowed_email_domains",
        "allowed_hosted_domains",
        "allowed_idp_tenants",
    ];
    let rows = match QuerySet::<P>::new().only(&cols).fetch(pool).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(target: "rustango::sso", "signup_mode unreadable, treated as closed: {e}");
            return std::collections::HashMap::new();
        }
    };
    let set = |r: &std::collections::HashMap<String, SqlValue>, c: &str| matches!(r.get(c), Some(SqlValue::String(s)) if !s.trim().is_empty());
    rows.iter()
        .filter_map(|r| {
            let id = match r.get("id") {
                Some(SqlValue::I64(n)) => *n,
                Some(SqlValue::I32(n)) => i64::from(*n),
                _ => return None,
            };
            let mode = match r.get("signup_mode") {
                Some(SqlValue::String(m)) => m.parse().unwrap_or(super::policy::SignupMode::Closed),
                _ => super::policy::SignupMode::Closed,
            };
            let narrowed = set(r, "allowed_email_domains")
                || set(r, "allowed_hosted_domains")
                || set(r, "allowed_idp_tenants");
            Some((id, (mode, narrowed)))
        })
        .collect()
}

/// Ids of the `P` rows with email linking on. Read apart from the row, as at
/// sign-in, so a table without the column reads as off.
#[cfg(feature = "tenancy")]
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
    #[cfg(feature = "tenancy")]
    let (linking, modes) = if source == LinkSource::Admin {
        (Vec::new(), std::collections::HashMap::new())
    } else {
        (
            email_linking::<P>(pool).await,
            signup_modes::<P>(pool).await,
        )
    };
    Ok(rows
        .into_iter()
        .map(|r| {
            #[cfg(feature = "tenancy")]
            let (signup, narrowed) = modes
                .get(&r.id)
                .copied()
                .unwrap_or((super::policy::SignupMode::Closed, false));
            Candidate {
                key: ProviderKey::for_row(source, r.id, &r.kind, r.issuer_url.as_deref()),
                #[cfg(feature = "tenancy")]
                allow_email_link: linking.contains(&r.id),
                #[cfg(feature = "tenancy")]
                signup,
                #[cfg(feature = "tenancy")]
                open_to_anyone: signup == super::policy::SignupMode::Open && !narrowed,
                slug: r.slug,
            }
        })
        .collect())
}

/// A provider setting that weakens sign-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Risk {
    /// Email linking on `kind = oidc`: whoever runs that IdP can assert any email.
    EmailLinkOnCustomIdp,
    /// A plain `http://` issuer that is not loopback.
    InsecureIssuer,
    /// A shared row that signs users up, in every tenant that uses it.
    SharedSignup,
    /// A multi-tenant Microsoft row with a role map or required claims and
    /// no `allowed_idp_tenants`: those claims are ignored until it pins one.
    UnpinnedDirectoryClaims,
}

/// An enabled provider with a [`Risk`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Risky {
    pub slug: String,
    pub why: Risk,
}

/// The enabled `P` rows' risky settings. A missing table holds none.
async fn risks<P: Model + Send>(pool: &Pool, source: LinkSource) -> Result<Vec<Risky>, ExecError> {
    use super::provider::{int, text};
    if !try_table_exists_here(pool, P::SCHEMA.table).await? {
        return Ok(Vec::new());
    }
    let cols = [
        "slug",
        "kind",
        "issuer_url",
        "allow_email_link",
        "allow_email_link_staff",
        "signup_mode",
        "allowed_idp_tenants",
        "role_map",
        "required_claims",
    ];
    let rows = QuerySet::<P>::new()
        .filter("enabled", true)
        .only(&cols)
        .fetch(pool)
        .await?;
    let mut out = Vec::new();
    for r in &rows {
        let slug = text(r.get("slug")).unwrap_or_default();
        let set = |c: &str| text(r.get(c)).is_some_and(|s| !s.trim().is_empty());
        let mut push = |why| {
            out.push(Risky {
                slug: slug.clone(),
                why,
            });
        };
        // The admin never links by email.
        if source != LinkSource::Admin
            && text(r.get("kind")).as_deref() == Some("oidc")
            && (int(r.get("allow_email_link")) != 0 || int(r.get("allow_email_link_staff")) != 0)
        {
            push(Risk::EmailLinkOnCustomIdp);
        }
        if text(r.get("issuer_url")).is_some_and(|u| super::insecure_url(&u)) {
            push(Risk::InsecureIssuer);
        }
        let kind = text(r.get("kind")).unwrap_or_default();
        let multi_tenant = kind == "microsoft"
            || (kind == "oidc"
                && text(r.get("issuer_url"))
                    .is_some_and(|u| crate::oauth2::is_microsoft_multi_tenant(&u)));
        if multi_tenant
            && !set("allowed_idp_tenants")
            && (claims_set(r.get("role_map")) || claims_set(r.get("required_claims")))
        {
            push(Risk::UnpinnedDirectoryClaims);
        }
        let signs_up = text(r.get("signup_mode"))
            .and_then(|m| m.parse::<super::policy::SignupMode>().ok())
            .is_some_and(|m| m >= super::policy::SignupMode::Allowlist);
        if source == LinkSource::Shared && signs_up {
            push(Risk::SharedSignup);
        }
    }
    Ok(out)
}

/// Whether a stored JSON map holds an entry.
fn claims_set(v: Option<&crate::core::SqlValue>) -> bool {
    use crate::core::SqlValue;
    let parsed = match v {
        Some(SqlValue::Json(j)) => j.clone(),
        Some(SqlValue::String(s)) => serde_json::from_str(s).unwrap_or_default(),
        _ => serde_json::Value::Null,
    };
    parsed.as_object().is_some_and(|o| !o.is_empty())
}

/// The tenant's own providers with a [`Risk`].
///
/// # Errors
/// Driver failures, or a table missing the policy columns.
#[cfg(feature = "tenancy")]
pub async fn tenant_risks(tenant: &Pool) -> Result<Vec<Risky>, ExecError> {
    risks::<super::SsoProvider>(tenant, LinkSource::Tenant).await
}

/// The registry's shared providers with a [`Risk`].
///
/// # Errors
/// Driver failures, or a table missing the policy columns.
#[cfg(all(feature = "tenancy", feature = "admin-sso"))]
pub async fn shared_risks(registry: &Pool) -> Result<Vec<Risky>, ExecError> {
    risks::<crate::tenancy::sso::SharedSsoProvider>(registry, LinkSource::Shared).await
}

/// The bare admin's providers with a [`Risk`].
///
/// # Errors
/// Driver failures, or a table missing the policy columns.
#[cfg(feature = "admin-sso")]
pub async fn admin_risks(pool: &Pool) -> Result<Vec<Risky>, ExecError> {
    risks::<super::SsoProvider>(pool, LinkSource::Admin).await
}

/// What to do about `why`, for the warning lines.
fn risk_text(why: Risk) -> &'static str {
    match why {
        Risk::EmailLinkOnCustomIdp => {
            "is a custom OIDC IdP with email linking on, so whoever runs that IdP can sign in \
             as any user by asserting their email — turn off allow_email_link and \
             allow_email_link_staff, or add SsoLink rows"
        }
        Risk::InsecureIssuer => {
            "has an http:// issuer_url, so discovery and tokens travel in clear — use https"
        }
        Risk::SharedSignup => {
            "is shared and signs users up, in every tenant that uses it — set signup_mode \
             closed or invite_only, or give tenants their own provider"
        }
        Risk::UnpinnedDirectoryClaims => {
            "is multi-tenant Microsoft with role_map or required_claims but no \
             allowed_idp_tenants; any directory can assert those claims, so they are ignored \
             (no roles synced, required claims refuse) — set allowed_idp_tenants"
        }
    }
}

/// `check --deploy` line for a provider with a [`Risk`]; `whose` names the
/// table, e.g. ``tenant `acme`: provider`` or `shared provider`.
#[must_use]
pub(crate) fn risk_warning(whose: &str, r: &Risky) -> String {
    format!("[sso] {whose} `{}` {}", r.slug, risk_text(r.why))
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
    let mut out: Vec<Stranded> = rows
        .iter()
        .filter(|r| r.open_to_anyone)
        .map(|r| Stranded {
            slug: r.slug.clone(),
            why: Refusal::OpenSignup,
        })
        .collect();
    // A provider that signs users up is not stranded.
    let rows = rows
        .into_iter()
        .filter(|r| r.signup == super::policy::SignupMode::Closed)
        .collect();
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
        Refusal::OpenSignup => {
            "has signup_mode open and no allowed_email_domains, allowed_hosted_domains or \
             allowed_idp_tenants, so anyone the IdP signs in gets an account — add a domain rule"
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
