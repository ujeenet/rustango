//! Links an IdP identity — provider plus stable `sub` — to one local user.
//!
//! SSO signs in by this link only. A verified email creates a link just when
//! the provider opts in, and never for a privileged account.

use crate::sql::{Auto, ExecError, Pool};
use crate::Model;

use super::NormalizedUser;

/// Which provider table `provider_id` points into, and which user table `user_id` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkSource {
    /// A tenant's own `SsoProvider` row; the user is a tenant `User`.
    Tenant,
    /// A registry `SharedSsoProvider` row; the user is a tenant `User`.
    Shared,
    /// The bare admin's `SsoProvider` row; the user is an `AdminUser`.
    Admin,
    /// An identity the app verified itself (e.g. a native ID token); a tenant `User`.
    App,
}

impl LinkSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tenant => "tenant",
            Self::Shared => "shared",
            Self::Admin => "admin",
            Self::App => "app",
        }
    }
}

/// The provider half of a link key. The issuer is part of it, so repointing a
/// provider row at another IdP does not inherit the old links.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderKey {
    source: LinkSource,
    provider_id: i64,
    issuer: String,
}

impl ProviderKey {
    /// Key for a provider row: its source, primary key, kind and issuer URL.
    pub(crate) fn for_row(
        source: LinkSource,
        provider_id: i64,
        kind: &str,
        issuer_url: Option<&str>,
    ) -> Self {
        let issuer = match issuer_url.map(str::trim).filter(|u| !u.is_empty()) {
            Some(u) => format!("{kind}|{}", u.trim_end_matches('/')),
            None => kind.to_owned(),
        };
        Self {
            source,
            provider_id,
            issuer,
        }
    }

    /// Key for an identity the app verified itself, e.g. `"https://accounts.google.com"`.
    #[must_use]
    pub fn app(issuer: &str) -> Self {
        Self {
            source: LinkSource::App,
            provider_id: 0,
            issuer: issuer.to_owned(),
        }
    }
}

/// One `(provider, subject) -> user` link. `managed = false`: created by [`ensure_table`].
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "rustango_sso_links",
    managed = false,
    unique_together = "provider_source, provider_id, issuer, subject",
    admin(
        list_display = "provider_source, provider_id, subject, user_id, created_at",
        search_fields = "subject",
        ordering = "user_id",
        readonly_fields = "created_at",
    )
)]
#[allow(dead_code)]
pub struct SsoLink {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// [`LinkSource::as_str`].
    #[rustango(max_length = 16)]
    pub provider_source: String,
    /// Provider row id (`0` for [`LinkSource::App`]).
    pub provider_id: i64,
    /// `kind` or `kind|issuer_url` of the provider when linked.
    #[rustango(max_length = 300)]
    pub issuer: String,
    /// The IdP's stable subject (`sub`, or the provider's user id).
    #[rustango(max_length = 255)]
    pub subject: String,
    /// Local user id, in the user table [`LinkSource`] names.
    pub user_id: i64,
    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
}

/// Create `rustango_sso_links` if missing. Safe to call repeatedly.
///
/// # Errors
/// Driver failures other than "already exists".
pub async fn ensure_table(pool: &Pool) -> Result<(), sqlx::Error> {
    use crate::core::Model as _;
    let snapshot = crate::migrate::SchemaSnapshot::from_models_forced(&[SsoLink::SCHEMA]);
    crate::migrate::apply_idempotent(pool, &snapshot).await
}

/// The user linked to `(key, subject)`, if any.
///
/// # Errors
/// Driver failures, including a missing table.
pub async fn linked_user(
    pool: &Pool,
    key: &ProviderKey,
    subject: &str,
) -> Result<Option<i64>, ExecError> {
    use crate::sql::FetcherPool as _;
    let rows: Vec<SsoLink> = SsoLink::objects()
        .filter("provider_source", key.source.as_str())
        .filter("provider_id", key.provider_id)
        .filter("issuer", key.issuer.clone())
        .filter("subject", subject.to_owned())
        .fetch(pool)
        .await?;
    Ok(rows.into_iter().next().map(|l| l.user_id))
}

/// Link `(key, subject)` to `user_id`.
///
/// # Errors
/// Driver failures, including a duplicate link.
pub async fn create_link(
    pool: &Pool,
    key: &ProviderKey,
    subject: &str,
    user_id: i64,
) -> Result<(), ExecError> {
    let mut row = SsoLink {
        id: Auto::Unset,
        provider_source: key.source.as_str().to_owned(),
        provider_id: key.provider_id,
        issuer: key.issuer.clone(),
        subject: subject.to_owned(),
        user_id,
        created_at: Auto::Unset,
    };
    row.insert_pool(pool).await
}

/// A local account whose email matched the IdP's verified email.
#[derive(Debug, Clone, Copy)]
pub struct EmailMatch {
    pub user_id: i64,
    /// Superuser or staff: never linked by email.
    pub privileged: bool,
}

/// Why an SSO sign-in was refused.
#[derive(Debug)]
pub enum LinkRefusal {
    /// The IdP returned no subject.
    NoSubject,
    /// No link, and no verified email to match.
    Unverified,
    /// No link, and no account has this (lowercased) email.
    NoAccount(String),
    /// An account matched, but the provider does not allow email linking.
    EmailLinkDisabled,
    /// An account matched, but it is privileged.
    Privileged,
    /// A storage error.
    Storage(String),
}

impl std::fmt::Display for LinkRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSubject => write!(f, "the IdP returned no subject"),
            Self::Unverified => write!(f, "no link and no verified email"),
            Self::NoAccount(e) => write!(f, "no link and no account for {e}"),
            Self::EmailLinkDisabled => write!(f, "no link; email linking is off for this provider"),
            Self::Privileged => {
                write!(f, "no link; email linking refused for a privileged account")
            }
            Self::Storage(e) => write!(f, "storage: {e}"),
        }
    }
}

/// Sign in by link; else link by verified email when `allow_email_link` and
/// the match is not privileged. Returns the local user id.
///
/// # Errors
/// [`LinkRefusal`]; the caller refuses the login.
pub async fn sign_in<F, Fut>(
    pool: &Pool,
    key: &ProviderKey,
    allow_email_link: bool,
    profile: &NormalizedUser,
    by_email: F,
) -> Result<i64, LinkRefusal>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<Option<EmailMatch>, String>>,
{
    let subject = profile.provider_user_id.as_str();
    if subject.is_empty() {
        return Err(LinkRefusal::NoSubject);
    }
    if let Err(e) = ensure_table(pool).await {
        tracing::warn!(target: "rustango::sso", "ensure sso links table: {e}");
    }
    if let Some(uid) = linked_user(pool, key, subject)
        .await
        .map_err(|e| LinkRefusal::Storage(e.to_string()))?
    {
        return Ok(uid);
    }
    let email = super::verified_email(profile)
        .map_err(|_| LinkRefusal::Unverified)?
        .to_ascii_lowercase();
    let Some(found) = by_email(email.clone())
        .await
        .map_err(LinkRefusal::Storage)?
    else {
        return Err(LinkRefusal::NoAccount(email));
    };
    if !allow_email_link {
        return Err(LinkRefusal::EmailLinkDisabled);
    }
    if found.privileged {
        return Err(LinkRefusal::Privileged);
    }
    create_link(pool, key, subject, found.user_id)
        .await
        .map_err(|e| LinkRefusal::Storage(e.to_string()))?;
    Ok(found.user_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issuer_is_part_of_the_key() {
        let a = ProviderKey::for_row(LinkSource::Tenant, 1, "oidc", Some("https://a.example/"));
        let b = ProviderKey::for_row(LinkSource::Tenant, 1, "oidc", Some("https://b.example"));
        assert_eq!(a.issuer, "oidc|https://a.example");
        assert_ne!(a, b);
        assert_eq!(
            ProviderKey::for_row(LinkSource::Admin, 1, "github", Some(" ")).issuer,
            "github"
        );
        assert_ne!(
            ProviderKey::for_row(LinkSource::Tenant, 1, "github", None),
            ProviderKey::for_row(LinkSource::Shared, 1, "github", None)
        );
    }
}
