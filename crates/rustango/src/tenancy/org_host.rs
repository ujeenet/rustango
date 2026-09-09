//! Additional hostnames a tenant answers on.
//!
//! [`Org::host_pattern`](super::Org) is the tenant's **base** host — the one
//! `create-tenant` writes and every existing deployment already resolves
//! through. This table holds the *extra* hosts an operator or a CMS adds
//! later, so one tenant can serve several domains.
//!
//! ## Why the base host is not a row here
//!
//! It would be tidier to migrate `host_pattern` into this table and mark it
//! `is_primary`, and that is exactly what makes it fragile: the guard
//! against deleting the base becomes a runtime `if` that one careless
//! `DELETE … WHERE org_id = ?` walks straight past, leaving a tenant no
//! host at all and no obvious way back.
//!
//! Keeping the base in `Org.host_pattern` makes it *structurally*
//! undeletable through this table — there is no row to remove. Listing
//! unions the two and flags which is which, so callers still see one list.

use serde::Serialize;

/// One extra hostname bound to a tenant.
#[derive(crate::Model, Debug, Clone, Serialize)]
#[rustango(
    table = "rustango_org_hosts",
    scope = "registry",
    display = "hostname",
    admin(
        list_display = "hostname, org_id, enabled, created_at",
        ordering = "hostname",
    )
)]
pub struct OrgHost {
    #[rustango(primary_key)]
    pub id: crate::sql::Auto<i64>,

    #[rustango(fk = "rustango_orgs", on = "id", index)]
    pub org_id: i64,

    /// The `Host` header to match, lowercased, no port and no scheme.
    ///
    /// Unique across the whole registry, not per-org: a hostname resolves
    /// to exactly one tenant, and letting two orgs claim one host would
    /// make routing depend on row order.
    #[rustango(max_length = 255, unique, index)]
    pub hostname: String,

    /// Lets an operator park a host without serving it — useful while DNS
    /// propagates, or to retire a domain without losing the record.
    pub enabled: bool,

    #[rustango(auto_now_add)]
    pub created_at: crate::sql::Auto<chrono::DateTime<chrono::Utc>>,
}

/// A hostname a tenant answers on, from either source.
#[derive(Debug, Clone, Serialize)]
pub struct TenantHost {
    pub hostname: String,
    /// True for `Org.host_pattern`. The caller uses this to refuse a
    /// remove, and the UI to explain why the row has no delete button.
    pub is_base: bool,
    pub enabled: bool,
    /// `None` for the base host — it has no row of its own.
    pub id: Option<i64>,
}

/// Why a host could not be registered.
#[derive(Debug)]
pub enum HostError {
    /// Empty, or carrying a scheme, port, path or uppercase.
    Invalid(String),
    /// Already claimed — by this tenant or another one.
    Taken(String),
    /// Removing the base host, which this table cannot represent.
    IsBaseHost(String),
    NotFound,
    Driver(crate::sql::ExecError),
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(h) => write!(f, "`{h}` is not a bare hostname"),
            Self::Taken(h) => write!(f, "`{h}` is already registered"),
            Self::IsBaseHost(h) => {
                write!(f, "`{h}` is this tenant's base host and cannot be removed")
            }
            Self::NotFound => write!(f, "no such host"),
            Self::Driver(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for HostError {}

impl From<crate::sql::ExecError> for HostError {
    fn from(e: crate::sql::ExecError) -> Self {
        Self::Driver(e)
    }
}

/// Normalize and validate a hostname.
///
/// Rejects rather than repairs anything that is not a bare host, because
/// the value is compared byte-for-byte against the `Host` header: a stored
/// `https://x.com/` or `x.com:443` would simply never match, and would look
/// like a routing bug rather than a bad input.
///
/// # Errors
/// [`HostError::Invalid`] when the value is empty, over-long, or carries a
/// scheme / port / path / whitespace.
pub fn normalize_hostname(raw: &str) -> Result<String, HostError> {
    let h = raw.trim().to_ascii_lowercase();
    let bad = h.is_empty()
        || h.len() > 255
        || h.contains("://")
        || h.contains('/')
        || h.contains(':')
        || h.contains(char::is_whitespace)
        || h.starts_with('.')
        || h.ends_with('.')
        || !h
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    if bad {
        return Err(HostError::Invalid(raw.to_owned()));
    }
    Ok(h)
}

use crate::core::Column as _;
use crate::sql::{FetcherPool as _, Pool};

/// Every hostname `org_slug` answers on — base first, then extras.
///
/// # Errors
/// Driver / query failures.
pub async fn list_for_org(registry: &Pool, org_slug: &str) -> Result<Vec<TenantHost>, HostError> {
    let Some(org) = super::Org::objects()
        .where_(super::Org::slug.eq(org_slug.to_owned()))
        .first(registry)
        .await?
    else {
        return Err(HostError::NotFound);
    };
    let org_id = org.id.get().copied().unwrap_or_default();
    let mut out = Vec::new();
    if let Some(base) = org.host_pattern.as_deref().filter(|h| !h.trim().is_empty()) {
        out.push(TenantHost {
            hostname: base.to_owned(),
            is_base: true,
            enabled: true,
            id: None,
        });
    }
    let extras: Vec<OrgHost> = OrgHost::objects()
        .where_(OrgHost::org_id.eq(org_id))
        .order_by(&[("hostname", false)])
        .fetch(registry)
        .await?;
    out.extend(extras.into_iter().map(|h| TenantHost {
        hostname: h.hostname,
        is_base: false,
        enabled: h.enabled,
        id: h.id.get().copied(),
    }));
    Ok(out)
}

/// Bind an extra `hostname` to `org_slug`.
///
/// # Errors
/// [`HostError::Invalid`] for a malformed host, [`HostError::Taken`] when
/// any tenant already claims it (including as its base host), or
/// [`HostError::NotFound`] for an unknown org.
pub async fn add_host(
    registry: &Pool,
    org_slug: &str,
    hostname: &str,
) -> Result<OrgHost, HostError> {
    let host = normalize_hostname(hostname)?;
    let Some(org) = super::Org::objects()
        .where_(super::Org::slug.eq(org_slug.to_owned()))
        .first(registry)
        .await?
    else {
        return Err(HostError::NotFound);
    };
    // Claimed as some tenant's BASE host? The unique index below cannot see
    // `rustango_orgs.host_pattern`, so without this a host could be added
    // here that silently never wins — `SubdomainResolver` runs first.
    let base_clash: Vec<super::Org> = super::Org::objects()
        .where_(super::Org::host_pattern.eq(Some(host.clone())))
        .fetch(registry)
        .await?;
    if !base_clash.is_empty() {
        return Err(HostError::Taken(host));
    }
    if !OrgHost::objects()
        .where_(OrgHost::hostname.eq(host.clone()))
        .fetch(registry)
        .await?
        .is_empty()
    {
        return Err(HostError::Taken(host));
    }
    let mut row = OrgHost {
        id: crate::sql::Auto::Unset,
        org_id: org.id.get().copied().unwrap_or_default(),
        hostname: host,
        enabled: true,
        created_at: crate::sql::Auto::Unset,
    };
    row.insert_pool(registry).await?;
    super::invalidate_host_cache();
    Ok(row)
}

/// Unbind an extra hostname.
///
/// Refuses the base host: it lives on the org, not here, so there is
/// nothing to delete — and a tenant with no host is unreachable.
///
/// # Errors
/// [`HostError::IsBaseHost`] for the base host, [`HostError::NotFound`]
/// when no extra row matches.
pub async fn remove_host(registry: &Pool, org_slug: &str, hostname: &str) -> Result<(), HostError> {
    let host = normalize_hostname(hostname)?;
    let Some(org) = super::Org::objects()
        .where_(super::Org::slug.eq(org_slug.to_owned()))
        .first(registry)
        .await?
    else {
        return Err(HostError::NotFound);
    };
    if org.host_pattern.as_deref() == Some(host.as_str()) {
        return Err(HostError::IsBaseHost(host));
    }
    let org_id = org.id.get().copied().unwrap_or_default();
    // Scoped to the org on purpose: one tenant must not be able to unbind
    // another's host by guessing the name.
    let Some(row) = OrgHost::objects()
        .where_(OrgHost::hostname.eq(host))
        .where_(OrgHost::org_id.eq(org_id))
        .first(registry)
        .await?
    else {
        return Err(HostError::NotFound);
    };
    row.delete_pool(registry).await?;
    super::invalidate_host_cache();
    Ok(())
}

/// Enable or disable an extra host without unbinding it.
///
/// # Errors
/// [`HostError::IsBaseHost`], [`HostError::NotFound`], or a driver failure.
pub async fn set_host_enabled(
    registry: &Pool,
    org_slug: &str,
    hostname: &str,
    enabled: bool,
) -> Result<(), HostError> {
    let host = normalize_hostname(hostname)?;
    let Some(org) = super::Org::objects()
        .where_(super::Org::slug.eq(org_slug.to_owned()))
        .first(registry)
        .await?
    else {
        return Err(HostError::NotFound);
    };
    if org.host_pattern.as_deref() == Some(host.as_str()) {
        return Err(HostError::IsBaseHost(host));
    }
    let org_id = org.id.get().copied().unwrap_or_default();
    let Some(mut row) = OrgHost::objects()
        .where_(OrgHost::hostname.eq(host))
        .where_(OrgHost::org_id.eq(org_id))
        .first(registry)
        .await?
    else {
        return Err(HostError::NotFound);
    };
    row.enabled = enabled;
    row.save_pool(registry).await?;
    super::invalidate_host_cache();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_bare_hostname_and_lowercases_it() {
        assert_eq!(normalize_hostname("Example.COM").unwrap(), "example.com");
        assert_eq!(
            normalize_hostname("  a.b.example.com  ").unwrap(),
            "a.b.example.com"
        );
        assert_eq!(
            normalize_hostname("my-site.co.uk").unwrap(),
            "my-site.co.uk"
        );
        // A bare label is legal — `localhost`, or an internal name.
        assert_eq!(normalize_hostname("localhost").unwrap(), "localhost");
    }

    /// Each of these would insert happily and then never match the `Host`
    /// header, which reads as "the feature is broken" rather than "the
    /// input was wrong". Reject at the door instead.
    #[test]
    fn rejects_anything_that_is_not_a_bare_host() {
        for bad in [
            "",
            "   ",
            "https://example.com", // scheme
            "example.com/",        // path
            "example.com/blog",    // path
            "example.com:8080",    // port
            "exa mple.com",        // whitespace
            ".example.com",        // leading dot
            "example.com.",        // trailing dot
            "exam_ple.com",        // underscore is not a hostname char
            "exam?ple.com",
        ] {
            assert!(
                normalize_hostname(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_an_over_long_hostname() {
        assert!(normalize_hostname(&"a".repeat(256)).is_err());
        assert!(normalize_hostname(&"a".repeat(255)).is_ok());
    }

    #[test]
    fn base_host_is_flagged_so_callers_can_refuse_a_remove() {
        let base = TenantHost {
            hostname: "acme.example.com".into(),
            is_base: true,
            enabled: true,
            id: None,
        };
        // The base has no row id precisely because it has no row — that is
        // what makes it undeletable through this table.
        assert!(base.is_base);
        assert!(base.id.is_none());
    }
}
