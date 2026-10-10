//! Links an IdP identity — provider plus stable `sub` — to one local user.
//!
//! SSO signs in by this link only. A verified email creates a link just when
//! the provider opts in, and never for a privileged account.

use crate::sql::{Auto, ExecError, Pool};
use crate::Model;

use super::NormalizedUser;

/// Which provider table `provider_id` points into, and which user table `user_id` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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

/// Longest issuer a link can store (`SsoLink::issuer`).
pub const MAX_ISSUER_LEN: usize = 300;

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
    /// `None` when the issuer is empty or longer than [`MAX_ISSUER_LEN`].
    #[must_use]
    pub fn app(issuer: &str) -> Option<Self> {
        (!issuer.is_empty() && issuer.len() <= MAX_ISSUER_LEN).then(|| Self {
            source: LinkSource::App,
            provider_id: 0,
            issuer: issuer.to_owned(),
        })
    }

    /// The stored issuer (`kind` or `kind|issuer_url`).
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }
}

/// One `(provider, subject) -> user` link, in the tenant's storage (or the
/// bare admin's database). Adding, changing or deleting a row is
/// superuser-only, and needs the admin's session auth.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "rustango_sso_links",
    unique_together = "provider_source, provider_id, key_sha256",
    admin(
        list_display = "provider_source, provider_id, subject, user_id, created_at",
        search_fields = "subject",
        ordering = "user_id",
        readonly_fields = "key_sha256, created_at",
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
    /// `kind` or `kind|issuer_url` (no trailing slash) of the provider.
    #[rustango(max_length = 300)]
    pub issuer: String,
    /// The IdP's stable subject (`sub`, or the provider's user id).
    #[rustango(max_length = 255)]
    pub subject: String,
    /// [`key_sha256`] of `issuer` and `subject`: the exact, collation-proof
    /// key. Computed on every write, the admin's included.
    #[rustango(max_length = 64)]
    pub key_sha256: String,
    /// Local user id, in the user table [`LinkSource`] names.
    pub user_id: i64,
    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
}

#[cfg(feature = "admin")]
fn superuser_only(_: &axum::http::request::Parts, _: Option<&serde_json::Value>) -> bool {
    crate::admin::session::current().is_some_and(|s| s.is_superuser)
}

// A link row decides who an IdP identity signs in as.
#[cfg(feature = "admin")]
crate::register_admin_object_permission!("rustango_sso_links", "add", superuser_only);
#[cfg(feature = "admin")]
crate::register_admin_object_permission!("rustango_sso_links", "change", superuser_only);
#[cfg(feature = "admin")]
crate::register_admin_object_permission!("rustango_sso_links", "delete", superuser_only);
// A provider row can turn on email linking or add a new IdP.
#[cfg(feature = "admin")]
crate::register_admin_object_permission!("rustango_sso_providers", "add", superuser_only);
#[cfg(feature = "admin")]
crate::register_admin_object_permission!("rustango_sso_providers", "change", superuser_only);
#[cfg(feature = "admin")]
crate::register_admin_object_permission!("rustango_sso_providers", "delete", superuser_only);

// The admin writes `key_sha256` from the submitted issuer and subject.
#[cfg(feature = "admin")]
fn derive_key<'a>(
    values: &'a mut Vec<(&'static str, crate::core::SqlValue)>,
    before: Option<&'a serde_json::Value>,
) -> crate::admin::derived_fields::DeriveFuture<'a> {
    use crate::admin::derived_fields::text;
    let issuer = text(values, before, "issuer").unwrap_or_default();
    let subject = text(values, before, "subject").unwrap_or_default();
    values.retain(|(c, _)| *c != "key_sha256");
    values.push((
        "key_sha256",
        crate::core::SqlValue::String(key_sha256(&issuer, &subject)),
    ));
    Box::pin(async { Ok::<(), String>(()) })
}

#[cfg(feature = "admin")]
inventory::submit! {
    crate::admin::derived_fields::AdminDerivedField {
        table: "rustango_sso_links",
        derive: derive_key,
    }
}

/// Lowercase hex SHA-256 of `issuer`, a newline and `subject`, as stored in
/// [`SsoLink::key_sha256`].
#[must_use]
pub fn key_sha256(issuer: &str, subject: &str) -> String {
    use sha2::{Digest as _, Sha256};
    Sha256::digest(format!("{issuer}\n{subject}").as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Create `rustango_sso_links` without the migration runner (tests, setups
/// without migrations). Safe to call repeatedly.
///
/// # Errors
/// Driver failures other than "already exists".
pub async fn ensure_table(pool: &Pool) -> Result<(), sqlx::Error> {
    use crate::core::Model as _;
    let snapshot = crate::migrate::SchemaSnapshot::from_models(&[SsoLink::SCHEMA]);
    crate::migrate::apply_idempotent(pool, &snapshot).await
}

/// The link for `(key, subject)`, if any. Issuer and subject are compared
/// exactly, whatever the column collation; a stale `key_sha256` (a raw
/// write) is repaired.
///
/// # Errors
/// Driver failures (including a missing table), or two rows for one identity.
pub async fn linked_user(
    pool: &Pool,
    key: &ProviderKey,
    subject: &str,
) -> Result<Option<SsoLink>, String> {
    use crate::sql::{FetcherPool as _, UpdaterPool as _};
    let rows: Vec<SsoLink> = SsoLink::objects()
        .filter("provider_source", key.source.as_str())
        .filter("provider_id", key.provider_id)
        .filter("subject", subject.to_owned())
        .fetch(pool)
        .await
        .map_err(|e| e.to_string())?;
    let mut exact = rows
        .into_iter()
        .filter(|l| l.issuer == key.issuer && l.subject == subject);
    let Some(mut link) = exact.next() else {
        return Ok(None);
    };
    if exact.next().is_some() {
        return Err(format!("two links for subject {subject:?}"));
    }
    let want = key_sha256(&key.issuer, subject);
    if link.key_sha256 != want {
        let id = link.id.get().copied().unwrap_or_default();
        SsoLink::objects()
            .filter("id", id)
            .update()
            .set("key_sha256", want.clone())
            .execute_pool(pool)
            .await
            .map_err(|e| format!("repair key_sha256 of link {id}: {e}"))?;
        link.key_sha256 = want;
    }
    Ok(Some(link))
}

/// Whether any identity is linked through `key`. The issuer is compared
/// exactly, as in [`linked_user`]: SQL `=` is case-blind on MySQL `_ci`.
#[cfg(any(feature = "tenancy", feature = "admin-sso"))]
pub(crate) async fn any_link(pool: &Pool, key: &ProviderKey) -> Result<bool, ExecError> {
    const PAGE: i64 = 500;
    let mut offset = 0;
    loop {
        let issuers = SsoLink::objects()
            .filter("provider_source", key.source.as_str())
            .filter("provider_id", key.provider_id)
            .filter("issuer", key.issuer.clone())
            .order_by(&[("id", false)])
            .limit(PAGE)
            .offset(offset)
            .values_list_flat("issuer")
            .fetch::<String>(pool)
            .await?;
        if issuers.iter().any(|i| *i == key.issuer) {
            return Ok(true);
        }
        if i64::try_from(issuers.len()).unwrap_or(PAGE) < PAGE {
            return Ok(false);
        }
        offset += PAGE;
    }
}

fn link_row(key: &ProviderKey, subject: &str, user_id: i64) -> SsoLink {
    SsoLink {
        id: Auto::Unset,
        provider_source: key.source.as_str().to_owned(),
        provider_id: key.provider_id,
        issuer: key.issuer.clone(),
        subject: subject.to_owned(),
        key_sha256: key_sha256(&key.issuer, subject),
        user_id,
        created_at: Auto::Unset,
    }
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
    link_row(key, subject, user_id).insert_pool(pool).await
}

/// [`create_link`] inside an open transaction.
///
/// # Errors
/// As [`create_link`].
pub async fn create_link_tx(
    tx: &mut crate::sql::PoolTx<'_>,
    key: &ProviderKey,
    subject: &str,
    user_id: i64,
) -> Result<(), ExecError> {
    link_row(key, subject, user_id).insert_tx(tx).await
}

/// A local account, as the link step needs to see it.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Account {
    pub user_id: i64,
    /// Superuser or staff: never linked by email.
    pub privileged: bool,
    pub active: bool,
}

impl Account {
    #[must_use]
    pub const fn new(user_id: i64, privileged: bool, active: bool) -> Self {
        Self {
            user_id,
            privileged,
            active,
        }
    }
}

/// The user table a sign-in resolves against.
pub trait AccountLookup: Sync {
    /// The account with this id.
    fn by_id(
        &self,
        id: i64,
    ) -> impl std::future::Future<Output = Result<Option<Account>, String>> + Send;
    /// The account for `email` (already lowercased), matched ASCII-case-insensitively.
    fn by_email(
        &self,
        email: &str,
    ) -> impl std::future::Future<Output = Result<EmailLookup, String>> + Send;
}

/// What an email lookup found.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum EmailLookup {
    Missing,
    Found(Account),
    /// The database matched rows (collation) that are not this email, or more
    /// than one: never link by it, never provision a second account.
    Collides,
}

impl EmailLookup {
    /// Pick the one account whose `email` equals `wanted` ignoring ASCII case.
    /// `rows` are the database's case-insensitive matches.
    #[allow(clippy::result_unit_err)]
    pub fn pick<'a, T>(
        rows: &'a [T],
        wanted: &str,
        email: impl Fn(&T) -> Option<&str>,
    ) -> Result<Option<&'a T>, ()> {
        let mut exact = rows
            .iter()
            .filter(|r| email(r).is_some_and(|e| e.eq_ignore_ascii_case(wanted)));
        match (exact.next(), exact.next()) {
            (Some(one), None) => Ok(Some(one)),
            (None, _) if rows.is_empty() => Ok(None),
            _ => Err(()),
        }
    }
}

/// Why an SSO sign-in was refused.
#[derive(Debug)]
#[non_exhaustive]
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
    /// The email collides with another account on this database.
    EmailCollides,
    /// The account is inactive.
    Inactive,
    /// A storage error.
    Storage(String),
}

impl std::fmt::Display for LinkRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSubject => write!(f, "the IdP returned no subject"),
            Self::Unverified => write!(f, "no link and no verified email"),
            Self::NoAccount(e) => write!(f, "no link and no account for {e}"),
            Self::EmailLinkDisabled => {
                write!(f, "no link; email linking is off for this provider")
            }
            Self::Privileged => {
                write!(f, "no link; email linking refused for a privileged account")
            }
            Self::EmailCollides => write!(f, "no link; the email collides with another account"),
            Self::Inactive => write!(f, "the account is inactive"),
            Self::Storage(e) => write!(f, "storage: {e}"),
        }
    }
}

#[cfg(feature = "signals")]
impl LinkRefusal {
    /// The `user_login_failed` reason; `None` for a storage error, which is no refusal.
    pub(crate) fn failure_reason(&self) -> Option<crate::signals::auth::AuthFailureReason> {
        use crate::signals::auth::AuthFailureReason as R;
        match self {
            Self::Storage(_) => None,
            Self::Inactive => Some(R::Inactive),
            _ => Some(R::InvalidCredentials),
        }
    }
}

/// Send `user_login_failed` for a refused SSO sign-in, naming the IdP
/// email, else its subject (#2559).
#[cfg(feature = "signals")]
pub(crate) async fn signal_refused(
    source: &'static str,
    profile: &NormalizedUser,
    reason: crate::signals::auth::AuthFailureReason,
    request: crate::signals::auth::AuthRequestMeta,
) {
    let attempted = profile
        .email
        .clone()
        .or_else(|| Some(profile.provider_user_id.clone()))
        .filter(|s| !s.is_empty());
    crate::signals::auth::send_user_login_failed(crate::signals::auth::UserLoginFailedContext {
        source,
        attempted_username: attempted,
        reason,
        request,
    })
    .await;
}

fn storage(e: impl std::fmt::Display) -> LinkRefusal {
    LinkRefusal::Storage(e.to_string())
}

/// Sign in by link; else link by verified email when `allow_email_link` and
/// the account is not privileged. Returns the active local user id.
///
/// # Errors
/// [`LinkRefusal`]; the caller refuses the login.
pub async fn sign_in(
    pool: &Pool,
    key: &ProviderKey,
    allow_email_link: bool,
    profile: &NormalizedUser,
    accounts: &impl AccountLookup,
) -> Result<i64, LinkRefusal> {
    let subject = profile.provider_user_id.as_str();
    if subject.is_empty() {
        return Err(LinkRefusal::NoSubject);
    }
    if let Some(link) = linked_user(pool, key, subject)
        .await
        .map_err(LinkRefusal::Storage)?
    {
        match accounts
            .by_id(link.user_id)
            .await
            .map_err(LinkRefusal::Storage)?
        {
            Some(a) if a.active => return Ok(a.user_id),
            Some(_) => return Err(LinkRefusal::Inactive),
            // The user is gone (a clean miss, not an error): drop the stale link.
            None => {
                link.delete_pool(pool).await.map_err(storage)?;
            }
        }
    }
    let email = super::verified_email(profile)
        .map_err(|_| LinkRefusal::Unverified)?
        .to_ascii_lowercase();
    let found = match accounts
        .by_email(&email)
        .await
        .map_err(LinkRefusal::Storage)?
    {
        EmailLookup::Found(a) => a,
        EmailLookup::Missing => return Err(LinkRefusal::NoAccount(email)),
        EmailLookup::Collides => return Err(LinkRefusal::EmailCollides),
    };
    if !allow_email_link {
        return Err(LinkRefusal::EmailLinkDisabled);
    }
    if found.privileged {
        return Err(LinkRefusal::Privileged);
    }
    if !found.active {
        return Err(LinkRefusal::Inactive);
    }
    if let Err(e) = create_link(pool, key, subject, found.user_id).await {
        // A concurrent first login may have linked it already.
        return match linked_user(pool, key, subject)
            .await
            .map_err(LinkRefusal::Storage)?
        {
            Some(link) if link.user_id == found.user_id => Ok(found.user_id),
            _ => Err(storage(e)),
        };
    }
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

    #[test]
    fn app_issuer_is_bounded() {
        assert!(ProviderKey::app("").is_none());
        assert!(ProviderKey::app(&"x".repeat(MAX_ISSUER_LEN + 1)).is_none());
        assert!(ProviderKey::app(&"x".repeat(MAX_ISSUER_LEN)).is_some());
    }

    #[test]
    fn key_hash_is_exact() {
        assert_ne!(key_sha256("i", "Sub-A"), key_sha256("i", "sub-a"));
        assert_ne!(key_sha256("I", "s"), key_sha256("i", "s"));
        assert_eq!(key_sha256("i", "x").len(), 64);
    }

    #[test]
    fn email_pick_is_ascii_case_insensitive_and_refuses_ambiguity() {
        let pick = |rows: &[&str], wanted: &str| {
            let rows: Vec<String> = rows.iter().map(|r| (*r).to_owned()).collect();
            EmailLookup::pick(&rows, wanted, |r| Some(r.as_str())).map(|r| r.cloned())
        };
        assert_eq!(
            pick(&["Ann@X.com"], "ann@x.com"),
            Ok(Some("Ann@X.com".into()))
        );
        assert_eq!(pick(&["jose@x.com"], "josé@x.com"), Err(()));
        assert_eq!(pick(&["a@x", "A@x"], "a@x"), Err(()));
        assert_eq!(pick(&[], "a@x"), Ok(None));
    }
}
