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

    /// The key a stored link row was made under.
    #[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
    pub(crate) fn of_link(link: &SsoLink) -> Option<Self> {
        let source = [
            LinkSource::Tenant,
            LinkSource::Shared,
            LinkSource::Admin,
            LinkSource::App,
        ]
        .into_iter()
        .find(|s| s.as_str() == link.provider_source)?;
        Some(Self {
            source,
            provider_id: link.provider_id,
            issuer: link.issuer.clone(),
        })
    }

    /// Lowercase hex SHA-256 of source, provider id and issuer: the whole key.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        key_sha256(
            &format!("{}\n{}", self.source.as_str(), self.provider_id),
            &self.issuer,
        )
    }

    /// Which provider table the row came from.
    #[cfg_attr(not(all(feature = "tenancy", feature = "admin-sso")), allow(dead_code))]
    pub(crate) fn source(&self) -> LinkSource {
        self.source
    }

    /// The provider row's primary key.
    #[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
    pub(crate) fn provider_id(&self) -> i64 {
        self.provider_id
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
    // Sign-in looks a link up by subject.
    index("provider_source, provider_id, subject"),
    // The hourly sign-up count.
    index("provisioned, created_at"),
    admin(
        list_display = "provider_source, provider_id, subject, user_id, created_at",
        search_fields = "subject",
        ordering = "user_id",
        readonly_fields = "key_sha256, created_at",
        actions = "unlink, relink",
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
    #[rustango(index)]
    pub user_id: i64,
    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
    /// Sign-up made this link along with its user.
    #[rustango(default = "false")]
    pub provisioned: bool,
    /// The last sign-in through this link.
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
}

// A link row decides who an IdP identity signs in as.
#[cfg(feature = "admin")]
crate::object_permissions::register_framework_object_permission!(
    "rustango_sso_links",
    "add",
    crate::object_permissions::superuser_only
);
#[cfg(feature = "admin")]
crate::object_permissions::register_framework_object_permission!(
    "rustango_sso_links",
    "change",
    crate::object_permissions::superuser_only
);
#[cfg(feature = "admin")]
crate::object_permissions::register_framework_object_permission!(
    "rustango_sso_links",
    "delete",
    crate::object_permissions::superuser_only
);
// A provider row can turn on email linking or add a new IdP.
#[cfg(feature = "admin")]
crate::object_permissions::register_framework_object_permission!(
    "rustango_sso_providers",
    "add",
    crate::object_permissions::superuser_only
);
#[cfg(feature = "admin")]
crate::object_permissions::register_framework_object_permission!(
    "rustango_sso_providers",
    "change",
    crate::object_permissions::superuser_only
);
#[cfg(feature = "admin")]
crate::object_permissions::register_framework_object_permission!(
    "rustango_sso_providers",
    "delete",
    crate::object_permissions::superuser_only
);

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

/// Why an admin write to `table` may not store its `provider_source` in the
/// admin's database: an `admin` link outside the bare admin's own database
/// would sign someone into the bare admin; a `shared` one lives only in tenant storage.
#[cfg(feature = "admin")]
pub(crate) fn admin_source_refusal(
    table: &str,
    db: crate::admin::AdminDatabase,
    values: &[(&'static str, crate::core::SqlValue)],
    before: Option<&serde_json::Value>,
) -> Option<String> {
    use crate::admin::AdminDatabase;
    if table != "rustango_sso_links" {
        return None;
    }
    let raw =
        crate::admin::derived_fields::text(values, before, "provider_source").unwrap_or_default();
    let allowed = match db {
        AdminDatabase::Tenant => {
            [LinkSource::Tenant, LinkSource::Shared, LinkSource::App].as_slice()
        }
        AdminDatabase::Single | AdminDatabase::Registry => {
            [LinkSource::Admin, LinkSource::Tenant, LinkSource::App].as_slice()
        }
    };
    if allowed.iter().any(|s| s.as_str() == raw) {
        return None;
    }
    let names = allowed
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let stored = before
        .and_then(|b| b.get("provider_source")?.as_str())
        .is_some_and(|s| s == raw);
    Some(if stored {
        format!(
            "this link's stored provider_source `{raw}` does not belong in this admin's \
             database (one of {names}); delete the link instead"
        )
    } else {
        format!("provider_source must be one of {names} in this admin's database")
    })
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
    use crate::sql::UpdaterPool as _;
    let rows = links_for(pool, key, subject).await?;
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

/// Columns added after the table first shipped.
const LATER_COLUMNS: &[&str] = &["provisioned", "last_used_at"];

/// The links of `(key, subject)`. A table without [`LATER_COLUMNS`] still
/// reads, with their defaults; any other error is an error.
async fn links_for(pool: &Pool, key: &ProviderKey, subject: &str) -> Result<Vec<SsoLink>, String> {
    use crate::core::SqlValue;
    use crate::sql::FetcherPool as _;
    let qs = || {
        SsoLink::objects()
            .filter("provider_source", key.source.as_str())
            .filter("provider_id", key.provider_id)
            .filter("subject", subject.to_owned())
    };
    let err = match qs().fetch(pool).await {
        Ok(rows) => return Ok(rows),
        Err(e) => e,
    };
    use super::provider::{columns_present, Columns};
    let missing: Vec<&'static str> =
        match columns_present(pool, "rustango_sso_links", LATER_COLUMNS).await {
            Ok(Columns::None) => LATER_COLUMNS.to_vec(),
            Ok(Columns::Some(have)) => LATER_COLUMNS
                .iter()
                .copied()
                .filter(|c| !have.contains(c))
                .collect(),
            Ok(Columns::All) => return Err(err.to_string()),
            Err(e) => return Err(format!("{err}; {e}")),
        };
    tracing::warn!(target: "rustango::sso", "rustango_sso_links is not migrated; run `migrate`: {err}");
    let rows = qs()
        .defer(&missing)
        .fetch(pool)
        .await
        .map_err(|e| e.to_string())?;
    let text = |r: &std::collections::HashMap<String, SqlValue>, c: &str| match r.get(c) {
        Some(SqlValue::String(s)) => s.clone(),
        _ => String::new(),
    };
    let int = |r: &std::collections::HashMap<String, SqlValue>, c: &str| match r.get(c) {
        Some(SqlValue::I64(n)) => *n,
        Some(SqlValue::I32(n)) => i64::from(*n),
        _ => 0,
    };
    Ok(rows
        .iter()
        .map(|r| SsoLink {
            id: Auto::Set(int(r, "id")),
            provider_source: text(r, "provider_source"),
            provider_id: int(r, "provider_id"),
            issuer: text(r, "issuer"),
            subject: text(r, "subject"),
            key_sha256: text(r, "key_sha256"),
            user_id: int(r, "user_id"),
            created_at: match r.get("created_at") {
                Some(SqlValue::DateTime(at)) => Auto::Set(*at),
                _ => Auto::Unset,
            },
            provisioned: matches!(r.get("provisioned"), Some(SqlValue::Bool(true)))
                || matches!(
                    r.get("provisioned"),
                    Some(SqlValue::I64(1) | SqlValue::I32(1))
                ),
            last_used_at: match r.get("last_used_at") {
                Some(SqlValue::DateTime(at)) => Some(*at),
                _ => None,
            },
        })
        .collect())
}

/// Stamp `last_used_at`. Best effort: a table not yet migrated only warns.
pub(crate) async fn touch(pool: &Pool, link: &SsoLink) {
    use crate::sql::UpdaterPool as _;
    let id = link.id.get().copied().unwrap_or_default();
    if let Err(e) = SsoLink::objects()
        .filter("id", id)
        .update()
        .set("last_used_at", chrono::Utc::now())
        .execute_pool(pool)
        .await
    {
        tracing::warn!(target: "rustango::sso", link_id = id, "last_used_at not stamped: {e}");
    }
}

/// Delete one link by id (works on a table not yet migrated).
pub(crate) async fn drop_link(pool: &Pool, link: &SsoLink) -> Result<(), String> {
    let id = link.id.get().copied().unwrap_or_default();
    let query = SsoLink::objects()
        .filter("id", id)
        .compile_delete()
        .map_err(|e| e.to_string())?;
    crate::sql::delete_pool(pool, &query)
        .await
        .map(|_| ())
        .map_err(|e| format!("delete link {id}: {e}"))
}

/// The links an admin hard delete of users removes with them, so a later
/// user given the same id does not inherit them.
#[cfg(feature = "admin")]
pub(crate) struct UserLinks(&'static [LinkSource]);

#[cfg(feature = "admin")]
impl UserLinks {
    /// `None` unless `user_table` is a user table links point into and the
    /// links table exists on `pool`.
    pub(crate) async fn of(pool: &Pool, user_table: &str) -> Result<Option<Self>, ExecError> {
        let sources: &'static [LinkSource] = match user_table {
            "rustango_users" => &[LinkSource::Tenant, LinkSource::Shared, LinkSource::App],
            "rustango_admin_users" => &[LinkSource::Admin],
            _ => return Ok(None),
        };
        use crate::core::Model as _;
        let here = crate::migrate::try_table_exists_here(pool, SsoLink::SCHEMA.table).await?;
        Ok(here.then_some(Self(sources)))
    }

    /// Delete the links to users `ids`, in the delete's transaction.
    pub(crate) async fn drop_tx(
        &self,
        tx: &mut crate::sql::PoolTx<'_>,
        ids: &[crate::core::SqlValue],
    ) -> Result<u64, ExecError> {
        use crate::core::{Column as _, SqlValue};
        let ids: Vec<i64> = ids
            .iter()
            .filter_map(|v| match v {
                SqlValue::I64(n) => Some(*n),
                SqlValue::I32(n) => Some(i64::from(*n)),
                _ => None,
            })
            .collect();
        if ids.is_empty() {
            return Ok(0);
        }
        let query = SsoLink::objects()
            .where_(SsoLink::user_id.is_in(ids))
            .where_(SsoLink::provider_source.is_in(self.0.iter().map(|s| s.as_str().to_owned())))
            .compile_delete()?;
        crate::sql::delete_tx(tx, &query).await
    }
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

fn link_row(key: &ProviderKey, subject: &str, user_id: i64, provisioned: bool) -> SsoLink {
    SsoLink {
        id: Auto::Unset,
        provider_source: key.source.as_str().to_owned(),
        provider_id: key.provider_id,
        issuer: key.issuer.clone(),
        subject: subject.to_owned(),
        key_sha256: key_sha256(&key.issuer, subject),
        user_id,
        created_at: Auto::Unset,
        provisioned,
        last_used_at: None,
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
    link_row(key, subject, user_id, false)
        .insert_pool(pool)
        .await
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
    insert_link_tx(tx, key, subject, user_id, false).await
}

/// [`create_link_tx`]; `provisioned` marks a link sign-up made with its user.
pub(crate) async fn insert_link_tx(
    tx: &mut crate::sql::PoolTx<'_>,
    key: &ProviderKey,
    subject: &str,
    user_id: i64,
    provisioned: bool,
) -> Result<(), ExecError> {
    link_row(key, subject, user_id, provisioned)
        .insert_tx(tx)
        .await
}

/// A local account, as the link step needs to see it.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Account {
    pub user_id: i64,
    /// Superuser or staff: never linked by email.
    pub privileged: bool,
    /// A superuser: never linked by email, whatever else allows it.
    pub superuser: bool,
    pub active: bool,
    /// When the account was made. A link older than its account may be an
    /// earlier user's with the same id (a reset sequence, a restore); see [`StaleLinks`].
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl Account {
    #[must_use]
    pub const fn new(user_id: i64, privileged: bool, active: bool) -> Self {
        Self {
            user_id,
            privileged,
            superuser: false,
            active,
            created_at: None,
        }
    }

    /// Mark a superuser.
    #[must_use]
    pub const fn with_superuser(mut self, superuser: bool) -> Self {
        self.superuser = superuser;
        self
    }

    /// Set [`Self::created_at`], so a link older than the account is noticed.
    #[must_use]
    pub const fn with_created_at(mut self, at: chrono::DateTime<chrono::Utc>) -> Self {
        self.created_at = Some(at);
        self
    }

    /// Whether `link` predates this account by more than clock skew. The
    /// account time is the app's clock, the link's the database's: allow an hour.
    fn outlived_by(&self, link: &SsoLink) -> bool {
        const SKEW: chrono::TimeDelta = chrono::TimeDelta::hours(1);
        match (self.created_at, link.created_at.get()) {
            (Some(account), Some(linked)) => account > *linked + SKEW,
            _ => false,
        }
    }
}

/// What sign-in does with a link made before the account it points at.
/// Dropping fails closed: a wrong drop only asks for a relink, while signing
/// in through an earlier user's link hands over the account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum StaleLinks {
    /// Log a warning and sign in through the link. For imported or
    /// backfilled `created_at` values that postdate their links.
    Warn,
    /// Delete the link and sign in as if unlinked.
    #[default]
    Drop,
}

/// `true` once [`StaleLinks::Warn`] is set; the default is `Drop`.
static WARN_STALE_LINKS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set the process-wide [`StaleLinks`] policy, normally once at startup.
pub fn set_stale_links(policy: StaleLinks) {
    WARN_STALE_LINKS.store(
        policy == StaleLinks::Warn,
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// The current [`StaleLinks`] policy.
#[must_use]
pub fn stale_links() -> StaleLinks {
    if WARN_STALE_LINKS.load(std::sync::atomic::Ordering::Relaxed) {
        StaleLinks::Warn
    } else {
        StaleLinks::Drop
    }
}

/// Apply [`stale_links`] to a link older than `account`: `true` when it was deleted.
pub(crate) async fn stale(pool: &Pool, link: &SsoLink, account: &Account) -> Result<bool, String> {
    if !account.outlived_by(link) {
        return Ok(false);
    }
    let policy = stale_links();
    tracing::warn!(
        target: "rustango::sso",
        user_id = account.user_id,
        link_id = link.id.get().copied().unwrap_or_default(),
        link_created = ?link.created_at.get(),
        account_created = ?account.created_at,
        ?policy,
        "SSO link is older than its account: an earlier user with this id, or an imported created_at"
    );
    if policy != StaleLinks::Drop {
        return Ok(false);
    }
    drop_link(pool, link).await?;
    tracing::error!(
        target: "rustango::sso",
        user_id = account.user_id,
        provider_source = %link.provider_source,
        "dropped an SSO link older than its account; the user must link again"
    );
    Ok(true)
}

/// The user table a sign-in resolves against.
pub trait AccountLookup: Sync {
    /// The tenant `User` table: SSO sign-up, role and profile sync write to it.
    const TENANT_USERS: bool = false;
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
    /// A sign-in policy rule, named by its audit token.
    Policy(&'static str),
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
            Self::Policy(rule) => write!(f, "refused by the sign-in policy: {rule}"),
        }
    }
}

/// Send `user_login_failed` for a refused SSO sign-in, naming the IdP's
/// verified email, else its subject (#2559).
#[cfg(feature = "signals")]
pub(crate) async fn signal_refused(
    source: &'static str,
    profile: &NormalizedUser,
    reason: crate::signals::auth::AuthFailureReason,
    request: crate::signals::auth::AuthRequestMeta,
    tenant: Option<String>,
) {
    let attempted = super::verified_email(profile)
        .ok()
        .map(str::to_owned)
        .or_else(|| Some(profile.provider_user_id.clone()))
        .filter(|s| !s.is_empty());
    crate::signals::auth::send_user_login_failed(crate::signals::auth::UserLoginFailedContext {
        source,
        attempted_username: attempted,
        reason,
        request,
        tenant,
    })
    .await;
}

/// Sign in by link; else link by verified email when `allow_email_link` and
/// the account is not privileged. Returns the active local user id. Runs
/// [`super::policy::admit`] with no sign-in rules.
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
    super::policy::legacy_sign_in(pool, key, allow_email_link, profile, accounts).await
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
