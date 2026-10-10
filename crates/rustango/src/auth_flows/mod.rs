//! Pre-built auth flows — password reset + email verification.
//!
//! Lightweight helpers that compose the existing `signed_url` + `email`
//! layers into the canonical signed-token-via-email pattern. You wire the
//! routes / templates yourself; these handle the token generation +
//! verification cycle.
//!
//! ## Password reset flow
//!
//! 1. User requests a reset → call [`PasswordReset::issue`] with their
//!    email + a callback URL → emit the email yourself with the returned URL.
//! 2. User clicks link → axum handler parses the URL → call
//!    [`PasswordReset::verify`] → if Ok, render the "set new password" form.
//! 3. User submits form → validate + write new hashed password to DB.
//!
//! ```ignore
//! use rustango::auth_flows::{AuthFlowError, LinkScope, PasswordReset};
//! use std::time::Duration;
//!
//! let secret: &[u8] = b"32-byte-app-secret-...";
//! // The request's tenant; confirm checks the same scope (#2472).
//! let scope = LinkScope::from(&tenant);
//!
//! // Step 1: issue
//! let url = PasswordReset::issue(
//!     &scope,
//!     "https://app.example.com/auth/reset",
//!     user_id,
//!     secret,
//!     Duration::from_secs(3600),
//! );
//! mailer.send(&Email::new()
//!     .to(&user.email)
//!     .subject("Reset your password")
//!     .body(&format!("Click here: {url}"))
//! ).await?;
//!
//! // Step 2: verify (in the callback handler)
//! match PasswordReset::verify(&scope, &incoming_url, secret) {
//!     Ok(user_id) => { /* render form to capture new password */ }
//!     Err(AuthFlowError::Expired) => { /* "link expired, request a new one" */ }
//!     Err(_) => { /* tampered or malformed */ }
//! }
//! ```
//!
//! ## Email verification flow
//!
//! Same pattern with [`EmailVerification`] — issue the URL after signup,
//! verify on the callback, mark the user's `email_verified_at` column.
//!
//! [`EmailVerification`]: crate::auth_flows::EmailVerification
//! [`PasswordReset::issue`]: crate::auth_flows::PasswordReset::issue
//! [`PasswordReset::verify`]: crate::auth_flows::PasswordReset::verify

use std::time::Duration;

use crate::signed_url::{sign, verify, SignedUrlError};

/// Errors from auth-flow helpers.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthFlowError {
    #[error("token is missing or malformed")]
    Malformed,
    #[error("token signature does not match")]
    InvalidSignature,
    #[error("token has expired")]
    Expired,
    #[error("token is for the wrong purpose ({0})")]
    WrongPurpose(String),
    /// `confirm_password_reset_pool` rejected the new password — too
    /// short, too common, or otherwise out-of-spec. Message is the
    /// validator's complaint, suitable for surfacing to the user.
    /// Issue #391.
    #[error("password rejected: {0}")]
    WeakPassword(String),
    /// Driver / SQL failure during the password UPDATE. The message
    /// carries the underlying driver error; callers typically log it
    /// and surface a generic "try again" message to the operator.
    /// Issue #391.
    #[error("database error: {0}")]
    Database(String),
    /// The token was already redeemed (audit N1). Returned by the
    /// `verify_single_use` variants when the signature has been seen
    /// before within its lifetime — defeats replay of a leaked
    /// magic-link / reset link.
    #[error("token already used")]
    AlreadyUsed,
    /// The link was issued for another tenant or audience (#2472).
    #[error("token is for another scope")]
    WrongScope,
}

/// Who a link is for: one tenant, or an app-chosen audience. Signed into
/// the link and required again on redeem, so a link from one tenant cannot
/// act on the same user id in another (#2472).
///
/// A multi-tenant app takes it from the request's tenant
/// (`LinkScope::from(&tenant)`) and confirms with [`LinkTarget::from`] the
/// same `&tenant`. [`Self::audience`] is for single-database apps only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkScope(String);

impl LinkScope {
    /// Query param holding the scope.
    const PARAM: &'static str = "scope";

    /// Scope a link to an opaque audience, e.g. `"app"`, in an app with one
    /// user database. Never equal to a tenant scope.
    #[must_use]
    pub fn audience(audience: &str) -> Self {
        Self(format!("aud:{audience}"))
    }

    /// Append the scope to an unsigned link.
    fn append(&self, url: &str) -> String {
        format!("{url}&{}={}", Self::PARAM, url_encode(&self.0))
    }

    /// The link's scope must be this one; a link without one is refused.
    fn check(&self, url: &str) -> Result<(), AuthFlowError> {
        match extract_query(url, Self::PARAM) {
            Some(s) if s == self.0 => Ok(()),
            _ => Err(AuthFlowError::WrongScope),
        }
    }
}

/// A tenant's scope. The org id is in it, so a slug freed by a purge and
/// taken again does not accept the old tenant's links.
#[cfg(feature = "tenancy")]
impl From<&crate::tenancy::Org> for LinkScope {
    fn from(org: &crate::tenancy::Org) -> Self {
        let id = org.id.get().copied().unwrap_or_default();
        Self(format!("tenant:{}:{id}", org.slug))
    }
}

#[cfg(feature = "tenancy")]
impl<DB: crate::sql::sqlx::Database> From<&crate::extractors::Tenant<DB>> for LinkScope {
    fn from(tenant: &crate::extractors::Tenant<DB>) -> Self {
        Self::from(&tenant.org)
    }
}

/// The pool a confirm writes to, with the scope its links must carry.
/// From `&Tenant` both come from one value, so they cannot disagree.
#[cfg(feature = "passwords")]
pub struct LinkTarget<'a> {
    pool: &'a crate::sql::Pool,
    scope: LinkScope,
}

#[cfg(feature = "passwords")]
impl<'a> LinkTarget<'a> {
    /// A single-database app's user pool and its audience.
    #[must_use]
    pub fn audience(pool: &'a crate::sql::Pool, audience: &str) -> Self {
        Self {
            pool,
            scope: LinkScope::audience(audience),
        }
    }
}

#[cfg(all(feature = "passwords", feature = "tenancy"))]
impl<'a, DB: crate::sql::sqlx::Database> From<&'a crate::extractors::Tenant<DB>>
    for LinkTarget<'a>
{
    fn from(tenant: &'a crate::extractors::Tenant<DB>) -> Self {
        Self {
            pool: tenant.pool(),
            scope: LinkScope::from(tenant),
        }
    }
}

/// Verify the signature, purpose and scope of a link.
fn check_link(
    url: &str,
    purpose: &str,
    scope: &LinkScope,
    secret: &[u8],
) -> Result<(), AuthFlowError> {
    verify(url, secret)?;
    let found = extract_query(url, "purpose").ok_or(AuthFlowError::Malformed)?;
    if found != purpose {
        return Err(AuthFlowError::WrongPurpose(found));
    }
    scope.check(url)
}

impl From<SignedUrlError> for AuthFlowError {
    fn from(e: SignedUrlError) -> Self {
        match e {
            SignedUrlError::MissingSignature | SignedUrlError::MalformedSignature => {
                Self::Malformed
            }
            SignedUrlError::InvalidSignature => Self::InvalidSignature,
            SignedUrlError::Expired => Self::Expired,
        }
    }
}

// ------------------------------------------------------------------ Password reset

/// Password reset flow helpers.
pub struct PasswordReset;

impl PasswordReset {
    const PURPOSE: &'static str = "pwreset";

    /// Build a signed reset URL valid for `ttl`. `base_url` should be your
    /// public callback (e.g. `"https://app.example.com/auth/reset"`).
    /// `user_id` is encoded as a query param so the verifier can identify
    /// the account. The signed issue time lets a password change since end
    /// the link (#2248). `scope` names the tenant whose pool will redeem it.
    #[must_use]
    pub fn issue(
        scope: &LinkScope,
        base_url: &str,
        user_id: i64,
        secret: &[u8],
        ttl: Duration,
    ) -> String {
        let url = format!(
            "{}?user_id={}&purpose={}&{}={}",
            base_url.trim_end_matches('?'),
            user_id,
            Self::PURPOSE,
            Self::ISSUED_AT,
            chrono::Utc::now().timestamp_micros(),
        );
        sign(&scope.append(&url), secret, Some(ttl))
    }

    /// Query param holding the issue time, unix microseconds.
    const ISSUED_AT: &'static str = "iat";

    /// [`Self::verify`] plus the link's issue time; `Expired` for a link
    /// minted before it carried one.
    #[cfg(feature = "passwords")]
    fn verify_issued(
        scope: &LinkScope,
        url: &str,
        secret: &[u8],
    ) -> Result<(i64, i64), AuthFlowError> {
        let user_id = Self::verify(scope, url, secret)?;
        let iat = extract_query(url, Self::ISSUED_AT)
            .and_then(|s| s.parse::<i64>().ok())
            .ok_or(AuthFlowError::Expired)?;
        Ok((user_id, iat))
    }

    /// Verify a reset URL issued for `scope`. On success returns the
    /// `user_id` extracted from the URL — caller writes the new password
    /// against this id.
    ///
    /// # Errors
    /// [`AuthFlowError`] variants describe the failure mode.
    pub fn verify(scope: &LinkScope, url: &str, secret: &[u8]) -> Result<i64, AuthFlowError> {
        check_link(url, Self::PURPOSE, scope, secret)?;
        let user_id_str = extract_query(url, "user_id").ok_or(AuthFlowError::Malformed)?;
        user_id_str
            .parse::<i64>()
            .map_err(|_| AuthFlowError::Malformed)
    }

    /// Like [`Self::verify`] but enforces **single use** (audit N1):
    /// records the token as consumed in `cache` so a replay returns
    /// [`AuthFlowError::AlreadyUsed`]. Prefer this for reset links, or
    /// enforce single-use yourself by deleting the reset record after a
    /// successful password change.
    ///
    /// # Errors
    /// As [`Self::verify`], plus [`AuthFlowError::AlreadyUsed`].
    #[cfg(feature = "cache")]
    pub async fn verify_single_use(
        scope: &LinkScope,
        url: &str,
        secret: &[u8],
        cache: &std::sync::Arc<dyn crate::cache::Cache>,
    ) -> Result<i64, AuthFlowError> {
        let user_id = Self::verify(scope, url, secret)?;
        consume_single_use(url, cache).await?;
        Ok(user_id)
    }
}

/// Finish a password reset: verify the reset URL, validate the new
/// password, hash it, and update the named user row. Returns the
/// `user_id` on success.
///
/// Sensible defaults:
/// - `user_table` = `"rustango_users"`
/// - `pk_column` = `"id"`
/// - `password_column` = `"password_hash"`
///
/// The new password must pass [`crate::passwords::strength_score`] —
/// the same policy `docs/auth-passwords.md` documents, so a password
/// refused at registration can no longer be set by resetting (#1399).
///
/// The link works only while `password_changed_at` is older than it, so
/// any password change, this reset included, ends every older link (#2248).
///
/// Sessions carry a fingerprint of the password hash (#1338), so the new
/// hash ends every session issued before the reset, an attacker's
/// included. It also stamps `password_changed_at` as a record (#1449).
///
/// Pairs with [`PasswordReset::issue`] — issue the URL, email it,
/// and call this helper from your POST `/password-reset/confirm`
/// endpoint to land the new password. Pass `&tenant` as `target` in a
/// multi-tenant app; a link issued for another scope is refused (#2472).
///
/// # Errors
/// - [`AuthFlowError`] from `PasswordReset::verify` (Malformed,
///   InvalidSignature, Expired, WrongPurpose, WrongScope).
/// - [`AuthFlowError::Expired`] too when the password changed since the link.
/// - [`AuthFlowError::WeakPassword`] when the password fails the policy.
/// - [`AuthFlowError::Database`] for SQL / driver failures.
#[cfg(feature = "passwords")]
pub async fn confirm_password_reset_pool(
    target: impl Into<LinkTarget<'_>>,
    url: &str,
    new_password: &str,
    secret: &[u8],
) -> Result<i64, AuthFlowError> {
    let LinkTarget { pool, scope } = target.into();
    let (user_id, iat) = PasswordReset::verify_issued(&scope, url, secret)?;
    check_password_strength(new_password)?;
    // Not a delegation to `_into`: this form owns `rustango_users`, so it
    // also stamps `password_changed_at` and ends existing sessions (#1449).
    write_password_hash(
        pool,
        user_id,
        new_password,
        "rustango_users",
        "id",
        "password_hash",
        Some(Rotation::since(iat)),
    )
    .await
}

/// As [`confirm_password_reset_pool`] but writes the new hash into a
/// caller-named table / columns. Use when the user model lives in a
/// custom table (e.g. tenant `app_users`) rather than the framework's
/// default `rustango_users`. Issue #391.
///
/// **Writes only the password column.** Framework sessions end on the new
/// hash; a session check your app writes itself must compare the hash too,
/// or stamp its own column in the same transaction (#1736).
///
/// # Errors
/// Same shape as [`confirm_password_reset_pool`].
#[cfg(feature = "passwords")]
#[allow(clippy::too_many_arguments)]
pub async fn confirm_password_reset_pool_into(
    target: impl Into<LinkTarget<'_>>,
    url: &str,
    new_password: &str,
    secret: &[u8],
    user_table: &str,
    pk_column: &str,
    password_column: &str,
) -> Result<i64, AuthFlowError> {
    let LinkTarget { pool, scope } = target.into();
    let user_id = PasswordReset::verify(&scope, url, secret)?;
    check_password_strength(new_password)?;
    write_password_hash(
        pool,
        user_id,
        new_password,
        user_table,
        pk_column,
        password_column,
        None,
    )
    .await
}

/// As [`confirm_password_reset_pool`] but the link is **single-use**: the
/// token is recorded in `cache` and a replay returns
/// [`AuthFlowError::AlreadyUsed`] (#1399).
///
/// `docs/auth-flows.md` recommends single-use for reset links; this is
/// the helper that can honour it. Without it a leaked copy of the reset
/// email is a working account takeover for the rest of the TTL, *after*
/// the legitimate user has completed their reset.
///
/// The password policy is checked before the token is consumed, so a
/// rejected password does not burn the user's link.
///
/// # Errors
/// As [`confirm_password_reset_pool`], plus [`AuthFlowError::AlreadyUsed`].
#[cfg(all(feature = "passwords", feature = "cache"))]
pub async fn confirm_password_reset_single_use(
    target: impl Into<LinkTarget<'_>>,
    url: &str,
    new_password: &str,
    secret: &[u8],
    cache: &std::sync::Arc<dyn crate::cache::Cache>,
) -> Result<i64, AuthFlowError> {
    let LinkTarget { pool, scope } = target.into();
    let (user_id, iat) = PasswordReset::verify_issued(&scope, url, secret)?;
    check_password_strength(new_password)?;
    consume_single_use(url, cache).await?;
    // As with the non-single-use form: this one owns `rustango_users`,
    // so it stamps `password_changed_at` too (#1449).
    write_password_hash(
        pool,
        user_id,
        new_password,
        "rustango_users",
        "id",
        "password_hash",
        Some(Rotation::since(iat)),
    )
    .await
}

/// As [`confirm_password_reset_single_use`] but writes into a
/// caller-named table / columns.
///
/// # Errors
/// Same shape as [`confirm_password_reset_single_use`].
#[cfg(all(feature = "passwords", feature = "cache"))]
#[allow(clippy::too_many_arguments)]
pub async fn confirm_password_reset_single_use_into(
    target: impl Into<LinkTarget<'_>>,
    url: &str,
    new_password: &str,
    secret: &[u8],
    cache: &std::sync::Arc<dyn crate::cache::Cache>,
    user_table: &str,
    pk_column: &str,
    password_column: &str,
) -> Result<i64, AuthFlowError> {
    let LinkTarget { pool, scope } = target.into();
    let user_id = PasswordReset::verify(&scope, url, secret)?;
    // Policy before consumption: a weak password must not cost the user
    // their link, but a replay must not reach the write.
    check_password_strength(new_password)?;
    consume_single_use(url, cache).await?;
    write_password_hash(
        pool,
        user_id,
        new_password,
        user_table,
        pk_column,
        password_column,
        None,
    )
    .await
}

/// The framework's documented password policy, applied where the reset
/// path used to check only `len() < 8` (#1399).
#[cfg(feature = "passwords")]
fn check_password_strength(new_password: &str) -> Result<(), AuthFlowError> {
    use crate::passwords::StrengthIssue;

    let issues = crate::passwords::strength_score(new_password);
    if issues.is_empty() {
        return Ok(());
    }
    let reasons: Vec<&str> = issues
        .iter()
        .map(|i| match i {
            StrengthIssue::TooShort => "it must be at least 12 characters",
            StrengthIssue::NoDigitsOrSymbols => "it needs a digit or a symbol",
            StrengthIssue::NoVariety => "it needs more than lowercase letters",
            StrengthIssue::KnownWeak => "it is a well-known weak password",
        })
        .collect();
    Err(AuthFlowError::WeakPassword(format!(
        "Password rejected: {}.",
        reasons.join("; ")
    )))
}

/// `password_changed_at`, stamped by a reset and compared with the link's
/// issue time. Only `rustango_users` is known to have it.
#[cfg(feature = "passwords")]
struct Rotation {
    /// Unix microseconds, so a link asked for just after a change still works.
    link_issued_at: i64,
}

#[cfg(feature = "passwords")]
impl Rotation {
    const COLUMN: &'static str = "password_changed_at";

    fn since(link_issued_at: i64) -> Self {
        Self { link_issued_at }
    }

    fn issued(&self) -> Result<chrono::DateTime<chrono::Utc>, AuthFlowError> {
        chrono::DateTime::from_timestamp_micros(self.link_issued_at).ok_or(AuthFlowError::Malformed)
    }
}

/// Hash and store the new password. Shared by the replayable and
/// single-use confirm helpers.
///
/// `rotation`, when given, stamps `password_changed_at` in the same UPDATE
/// so older sessions end (#1449), and the UPDATE lands only if the password
/// has not changed since the link was issued (#2248). `None` for a
/// caller-named table, which may have no such column.
#[cfg(feature = "passwords")]
async fn write_password_hash(
    pool: &crate::sql::Pool,
    user_id: i64,
    new_password: &str,
    user_table: &str,
    pk_column: &str,
    password_column: &str,
    rotation: Option<Rotation>,
) -> Result<i64, AuthFlowError> {
    let hash = crate::passwords::hash_async(new_password)
        .await
        .map_err(|e| AuthFlowError::Database(e.to_string()))?;
    if let Some(rotation) = rotation {
        let written = rotate_user_password(pool, user_id, hash, &rotation).await?;
        // A changed password (or a missing row) ends the link (#2248).
        return if written == 0 {
            Err(AuthFlowError::Expired)
        } else {
            Ok(user_id)
        };
    }
    // Raw: the table and columns are caller-named, so there is no model.
    let dialect = pool.dialect();
    let sql = format!(
        "UPDATE {} SET {} = {} WHERE {} = {}",
        dialect.quote_ident(user_table),
        dialect.quote_ident(password_column),
        dialect.placeholder(1),
        dialect.quote_ident(pk_column),
        dialect.placeholder(2),
    );
    let args = vec![
        crate::core::SqlValue::String(hash),
        crate::core::SqlValue::I64(user_id),
    ];
    crate::sql::raw_execute_pool(pool, &sql, args)
        .await
        .map_err(|e| AuthFlowError::Database(e.to_string()))?;
    Ok(user_id)
}

/// The `rustango_users` columns a reset writes. Built by hand so the
/// UPDATE goes through the ORM in builds without `tenancy`'s `User` (#2273).
#[cfg(feature = "passwords")]
static RESET_USERS: crate::core::ModelSchema = {
    use crate::core::{FieldSchema, FieldType};
    const FIELDS: &[FieldSchema] = &[
        {
            let mut f = FieldSchema::new("id", "id", FieldType::I64);
            f.primary_key = true;
            f
        },
        FieldSchema::new("password_hash", "password_hash", FieldType::String),
        {
            let mut f = FieldSchema::new(Rotation::COLUMN, Rotation::COLUMN, FieldType::DateTime);
            f.nullable = true;
            f
        },
    ];
    let mut s = crate::core::ModelSchema::new("User", "rustango_users");
    s.fields = FIELDS;
    s
};

/// Store `hash` and stamp the rotation, only while the password is
/// unchanged since the link. Rows written.
#[cfg(feature = "passwords")]
async fn rotate_user_password(
    pool: &crate::sql::Pool,
    user_id: i64,
    hash: String,
    rotation: &Rotation,
) -> Result<u64, AuthFlowError> {
    use crate::core::{Assignment, SqlValue, UpdateQuery};
    use crate::query::Q;
    let since = rotation.issued()?;
    let query = UpdateQuery {
        model: &RESET_USERS,
        set: vec![
            Assignment::new("password_hash", SqlValue::String(hash)),
            Assignment::new(Rotation::COLUMN, SqlValue::DateTime(chrono::Utc::now())),
        ],
        where_clause: (Q::eq("id", user_id)
            & (Q::is_null(Rotation::COLUMN) | Q::lt(Rotation::COLUMN, since)))
        .into(),
    };
    crate::sql::update_pool(pool, &query)
        .await
        .map_err(|e| AuthFlowError::Database(e.to_string()))
}

// ------------------------------------------------------------------ Email verification

/// Email verification flow helpers.
pub struct EmailVerification;

impl EmailVerification {
    const PURPOSE: &'static str = "verify_email";

    /// Build a signed verification URL valid for `ttl` (typically 24h+).
    /// Encodes both `user_id` and `email` so the verifier can confirm the
    /// user hasn't changed their email between issuance and click.
    #[must_use]
    pub fn issue(
        scope: &LinkScope,
        base_url: &str,
        user_id: i64,
        email: &str,
        secret: &[u8],
        ttl: Duration,
    ) -> String {
        let url = format!(
            "{}?user_id={}&email={}&purpose={}",
            base_url.trim_end_matches('?'),
            user_id,
            url_encode(email),
            Self::PURPOSE,
        );
        sign(&scope.append(&url), secret, Some(ttl))
    }

    /// Verify the URL and return the `(user_id, email)` it was issued for.
    /// Caller compares email against the user's current email to detect
    /// stale verification links.
    ///
    /// # Errors
    /// As [`PasswordReset::verify`].
    pub fn verify(
        scope: &LinkScope,
        url: &str,
        secret: &[u8],
    ) -> Result<(i64, String), AuthFlowError> {
        check_link(url, Self::PURPOSE, scope, secret)?;
        let user_id_str = extract_query(url, "user_id").ok_or(AuthFlowError::Malformed)?;
        let user_id = user_id_str
            .parse::<i64>()
            .map_err(|_| AuthFlowError::Malformed)?;
        let email = extract_query(url, "email").ok_or(AuthFlowError::Malformed)?;
        Ok((user_id, email))
    }

    /// Like [`Self::verify`] but enforces **single use** (audit N1) via
    /// `cache` — a replay returns [`AuthFlowError::AlreadyUsed`].
    ///
    /// # Errors
    /// As [`Self::verify`], plus [`AuthFlowError::AlreadyUsed`].
    #[cfg(feature = "cache")]
    pub async fn verify_single_use(
        scope: &LinkScope,
        url: &str,
        secret: &[u8],
        cache: &std::sync::Arc<dyn crate::cache::Cache>,
    ) -> Result<(i64, String), AuthFlowError> {
        let out = Self::verify(scope, url, secret)?;
        consume_single_use(url, cache).await?;
        Ok(out)
    }
}

// ------------------------------------------------------------------ Magic-link login

/// Magic-link login flow — passwordless authentication via emailed URL.
pub struct MagicLink;

impl MagicLink {
    const PURPOSE: &'static str = "magic_link";

    /// Build a signed login URL valid for `ttl` (typically 10–30 minutes).
    /// `email` identifies the user — the verifier uses it to look up the
    /// account and issue a session.
    #[must_use]
    pub fn issue(
        scope: &LinkScope,
        base_url: &str,
        email: &str,
        secret: &[u8],
        ttl: Duration,
    ) -> String {
        let url = format!(
            "{}?email={}&purpose={}",
            base_url.trim_end_matches('?'),
            url_encode(email),
            Self::PURPOSE,
        );
        sign(&scope.append(&url), secret, Some(ttl))
    }

    /// Verify the URL and return the email it was issued for.
    ///
    /// **This is NOT single-use** — the signature is valid until its TTL
    /// expires, so a leaked link (Referer header, browser history, proxy
    /// logs) can be replayed to log in repeatedly. For passwordless login
    /// use [`Self::verify_single_use`] (or otherwise burn the token after
    /// first use). Audit N1.
    ///
    /// # Errors
    /// As [`PasswordReset::verify`].
    pub fn verify(scope: &LinkScope, url: &str, secret: &[u8]) -> Result<String, AuthFlowError> {
        check_link(url, Self::PURPOSE, scope, secret)?;
        extract_query(url, "email").ok_or(AuthFlowError::Malformed)
    }

    /// Like [`Self::verify`] but enforces **single use** (audit N1):
    /// the magic link can be redeemed only once — a replay returns
    /// [`AuthFlowError::AlreadyUsed`]. Strongly recommended for
    /// passwordless login, where `verify` alone leaves a replay window.
    ///
    /// # Errors
    /// As [`Self::verify`], plus [`AuthFlowError::AlreadyUsed`].
    #[cfg(feature = "cache")]
    pub async fn verify_single_use(
        scope: &LinkScope,
        url: &str,
        secret: &[u8],
        cache: &std::sync::Arc<dyn crate::cache::Cache>,
    ) -> Result<String, AuthFlowError> {
        let email = Self::verify(scope, url, secret)?;
        consume_single_use(url, cache).await?;
        Ok(email)
    }
}

// ------------------------------------------------------------------ Helpers

fn extract_query(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=')?;
        let k = url_decode(k);
        if k == key {
            return Some(url_decode(v));
        }
    }
    None
}

/// Mark a (already-verified) signed-URL token as consumed so it can't be
/// replayed (audit N1). Keys the token's `signature` in the cache for its
/// remaining lifetime; a repeat within that window returns
/// [`AuthFlowError::AlreadyUsed`].
///
/// One atomic `add`, so of two concurrent redemptions one wins (#1853).
/// Fails closed: a cache error, or a cache that stores nothing, refuses.
#[cfg(feature = "cache")]
async fn consume_single_use(
    url: &str,
    cache: &std::sync::Arc<dyn crate::cache::Cache>,
) -> Result<(), AuthFlowError> {
    let sig = extract_query(url, "signature").ok_or(AuthFlowError::Malformed)?;
    if cache.stores_nothing() {
        tracing::error!(
            target: "rustango::auth_flows",
            "single-use token refused: the cache keeps nothing (`NullCache`); use a shared cache"
        );
        return Err(AuthFlowError::AlreadyUsed);
    }
    let key = format!("authflow_used:{sig}");
    match cache.add(&key, "1", Some(single_use_ttl(url))).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(AuthFlowError::AlreadyUsed),
        Err(e) => {
            tracing::error!(target: "rustango::auth_flows", error = %e, "single-use token refused: cache failed");
            Err(AuthFlowError::AlreadyUsed)
        }
    }
}

/// Remaining lifetime of the token from its `expires` param (unix secs),
/// so the used-marker lives exactly as long as the token could be valid.
/// Falls back to 1h when `expires` is absent/unparseable/in the past.
#[cfg(feature = "cache")]
fn single_use_ttl(url: &str) -> Duration {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    extract_query(url, "expires")
        .and_then(|s| s.parse::<u64>().ok())
        .map(|exp| Duration::from_secs(exp.saturating_sub(now)))
        .filter(|d| !d.is_zero())
        .unwrap_or(Duration::from_secs(3600))
}

// #806 — was byte-identical to `crate::url_codec::url_encode`
// (RFC 3986 unreserved set — the right alphabet for OAuth-style
// `?next=...` redirect targets). Route through the canonical codec.
// Percent-decoder consolidated into [`crate::url_codec`] — see the
// note in [`crate::signed_url`]. Re-imported under the `url_decode`
// name to keep the local call sites unchanged.
use crate::url_codec::{url_decode, url_encode};

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"my-test-secret";

    fn scope() -> LinkScope {
        LinkScope::audience("acme")
    }

    /// The hand-built schema must match the `User` model it stands in for.
    #[cfg(all(feature = "passwords", feature = "tenancy"))]
    #[test]
    fn reset_users_schema_matches_the_user_model() {
        use crate::core::Model as _;
        let user = crate::tenancy::User::SCHEMA;
        assert_eq!(RESET_USERS.table, user.table);
        for f in RESET_USERS.fields {
            let u = user.field_by_column(f.column).expect(f.column);
            assert_eq!((f.ty, f.nullable), (u.ty, u.nullable), "{}", f.column);
        }
    }

    // -------------------------------- Password reset

    #[test]
    fn password_reset_issue_and_verify_roundtrip() {
        let url = PasswordReset::issue(
            &scope(),
            "https://app.example.com/reset",
            42,
            SECRET,
            Duration::from_secs(3600),
        );
        let user_id = PasswordReset::verify(&scope(), &url, SECRET).unwrap();
        assert_eq!(user_id, 42);
    }

    /// #2472 — every link kind is refused outside the scope it was issued for.
    #[test]
    fn links_are_refused_in_another_scope() {
        let other = LinkScope::audience("globex");
        let ttl = Duration::from_secs(600);
        let reset = PasswordReset::issue(&scope(), "https://x/r", 1, SECRET, ttl);
        let verify = EmailVerification::issue(&scope(), "https://x/v", 1, "a@x.com", SECRET, ttl);
        let magic = MagicLink::issue(&scope(), "https://x/l", "a@x.com", SECRET, ttl);
        assert_eq!(
            PasswordReset::verify(&other, &reset, SECRET),
            Err(AuthFlowError::WrongScope)
        );
        assert_eq!(
            EmailVerification::verify(&other, &verify, SECRET),
            Err(AuthFlowError::WrongScope)
        );
        assert_eq!(
            MagicLink::verify(&other, &magic, SECRET),
            Err(AuthFlowError::WrongScope)
        );
    }

    /// #2472 — the scope is signed: rewriting it breaks the signature.
    #[test]
    fn a_rewritten_scope_fails_the_signature() {
        let url =
            PasswordReset::issue(&scope(), "https://x/r", 1, SECRET, Duration::from_secs(600));
        let forged = url.replace("aud%3Aacme", "aud%3Aglobex");
        assert_ne!(forged, url);
        let r = PasswordReset::verify(&LinkScope::audience("globex"), &forged, SECRET);
        assert_eq!(r, Err(AuthFlowError::InvalidSignature));
    }

    #[cfg(feature = "tenancy")]
    fn org(slug: &str, id: i64) -> crate::tenancy::Org {
        crate::tenancy::Org {
            id: crate::sql::Auto::Set(id),
            slug: slug.into(),
            ..crate::testkit::org()
        }
    }

    /// #2472 — a tenant scope names the org id, so a slug reused after a
    /// purge, or an audience of the same name, does not match.
    #[cfg(all(feature = "tenancy", feature = "testkit"))]
    #[test]
    fn a_tenant_scope_is_its_org_not_its_slug() {
        let old = LinkScope::from(&org("acme", 1));
        let url = PasswordReset::issue(&old, "https://x/r", 1, SECRET, Duration::from_secs(600));
        for other in [
            LinkScope::from(&org("acme", 2)),
            LinkScope::audience("acme"),
        ] {
            assert_eq!(
                PasswordReset::verify(&other, &url, SECRET),
                Err(AuthFlowError::WrongScope)
            );
        }
        assert_eq!(PasswordReset::verify(&old, &url, SECRET), Ok(1));
    }

    /// #2472 — confirm through `&Tenant` takes pool and scope from one
    /// value: its own links reset, another tenant's are refused.
    #[cfg(all(feature = "tenancy", feature = "sqlite", feature = "testkit"))]
    #[tokio::test]
    async fn confirm_through_a_tenant_checks_its_scope() {
        use crate::sql::sqlx;
        let sq = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let pool = crate::sql::Pool::from(sq.clone());
        crate::testkit::create_tables_for::<crate::tenancy::User>(&pool)
            .await
            .unwrap();
        let mut u = crate::tenancy::User {
            username: "ada".into(),
            password_hash: "OLD".into(),
            ..crate::testkit::user()
        };
        u.insert_pool(&pool).await.unwrap();
        let id = *u.id.get().unwrap();
        let conn = crate::tenancy::TenantConn::database(sq.acquire().await.unwrap());
        let tenant = crate::extractors::Tenant::for_test(org("b", 2), conn, pool);
        let ttl = Duration::from_secs(600);
        let strong = "brand-new-strong-password-7!";

        let foreign = PasswordReset::issue(
            &LinkScope::from(&org("x", 1)),
            "https://x/r",
            id,
            SECRET,
            ttl,
        );
        let r = confirm_password_reset_pool(&tenant, &foreign, strong, SECRET).await;
        assert_eq!(r, Err(AuthFlowError::WrongScope));

        let own = PasswordReset::issue(&LinkScope::from(&tenant), "https://b/r", id, SECRET, ttl);
        assert_eq!(
            confirm_password_reset_pool(&tenant, &own, strong, SECRET).await,
            Ok(id)
        );
    }

    /// A signed link minted before scopes existed is refused.
    #[test]
    fn an_unscoped_link_is_refused() {
        let url = sign(
            "https://x/r?user_id=1&purpose=pwreset&iat=1",
            SECRET,
            Some(Duration::from_secs(60)),
        );
        let r = PasswordReset::verify(&scope(), &url, SECRET);
        assert_eq!(r, Err(AuthFlowError::WrongScope));
    }

    #[test]
    fn password_reset_wrong_secret_fails() {
        let url = PasswordReset::issue(
            &scope(),
            "https://x/r",
            42,
            SECRET,
            Duration::from_secs(3600),
        );
        let r = PasswordReset::verify(&scope(), &url, b"different");
        assert_eq!(r.unwrap_err(), AuthFlowError::InvalidSignature);
    }

    #[test]
    fn password_reset_tampered_user_id_fails() {
        let url = PasswordReset::issue(
            &scope(),
            "https://x/r",
            42,
            SECRET,
            Duration::from_secs(3600),
        );
        let tampered = url.replace("user_id=42", "user_id=99");
        let r = PasswordReset::verify(&scope(), &tampered, SECRET);
        assert_eq!(r.unwrap_err(), AuthFlowError::InvalidSignature);
    }

    #[test]
    fn password_reset_rejects_email_verification_token() {
        // Same URL shape but issued for a different purpose
        let url = EmailVerification::issue(
            &scope(),
            "https://x/r",
            42,
            "alice@x.com",
            SECRET,
            Duration::from_secs(3600),
        );
        let r = PasswordReset::verify(&scope(), &url, SECRET);
        assert!(matches!(r, Err(AuthFlowError::WrongPurpose(_))));
    }

    // -------------------------------- Email verification

    #[test]
    fn email_verification_roundtrip() {
        let url = EmailVerification::issue(
            &scope(),
            "https://x/v",
            42,
            "alice@example.com",
            SECRET,
            Duration::from_secs(86_400),
        );
        let (uid, email) = EmailVerification::verify(&scope(), &url, SECRET).unwrap();
        assert_eq!(uid, 42);
        assert_eq!(email, "alice@example.com");
    }

    #[test]
    fn email_verification_handles_special_chars() {
        let url = EmailVerification::issue(
            &scope(),
            "https://x/v",
            42,
            "a+b@example.com",
            SECRET,
            Duration::from_secs(86_400),
        );
        let (_, email) = EmailVerification::verify(&scope(), &url, SECRET).unwrap();
        assert_eq!(email, "a+b@example.com");
    }

    #[test]
    fn email_verification_rejects_password_reset_token() {
        let url = PasswordReset::issue(
            &scope(),
            "https://x/v",
            42,
            SECRET,
            Duration::from_secs(3600),
        );
        let r = EmailVerification::verify(&scope(), &url, SECRET);
        assert!(matches!(r, Err(AuthFlowError::WrongPurpose(_))));
    }

    // -------------------------------- Magic link

    #[test]
    fn magic_link_roundtrip() {
        let url = MagicLink::issue(
            &scope(),
            "https://x/login",
            "alice@example.com",
            SECRET,
            Duration::from_secs(900),
        );
        let email = MagicLink::verify(&scope(), &url, SECRET).unwrap();
        assert_eq!(email, "alice@example.com");
    }

    #[test]
    fn magic_link_rejects_password_reset_token() {
        let url = PasswordReset::issue(
            &scope(),
            "https://x/r",
            42,
            SECRET,
            Duration::from_secs(3600),
        );
        let r = MagicLink::verify(&scope(), &url, SECRET);
        assert!(matches!(r, Err(AuthFlowError::WrongPurpose(_))));
    }

    #[cfg(feature = "cache")]
    #[tokio::test]
    async fn magic_link_single_use_rejects_replay() {
        use crate::cache::InMemoryCache;
        let cache: std::sync::Arc<dyn crate::cache::Cache> =
            std::sync::Arc::new(InMemoryCache::new());
        let url = MagicLink::issue(
            &scope(),
            "https://x/login",
            "alice@example.com",
            SECRET,
            Duration::from_secs(900),
        );
        // First redemption succeeds.
        let email = MagicLink::verify_single_use(&scope(), &url, SECRET, &cache)
            .await
            .unwrap();
        assert_eq!(email, "alice@example.com");
        // Audit N1 — replaying the same link is rejected.
        let replay = MagicLink::verify_single_use(&scope(), &url, SECRET, &cache).await;
        assert!(
            matches!(replay, Err(AuthFlowError::AlreadyUsed)),
            "{replay:?}"
        );
        // A *different* link (fresh signature) is unaffected.
        let other = MagicLink::issue(
            &scope(),
            "https://x/login",
            "bob@example.com",
            SECRET,
            Duration::from_secs(900),
        );
        assert!(
            MagicLink::verify_single_use(&scope(), &other, SECRET, &cache)
                .await
                .is_ok()
        );
    }

    #[cfg(feature = "cache")]
    fn link() -> String {
        MagicLink::issue(
            &scope(),
            "https://x/login",
            "alice@example.com",
            SECRET,
            Duration::from_secs(900),
        )
    }

    /// `InMemoryCache` whose `exists` waits until two callers are inside it.
    #[cfg(feature = "cache")]
    struct Racy(crate::cache::InMemoryCache, tokio::sync::Barrier);

    #[cfg(feature = "cache")]
    #[async_trait::async_trait]
    impl crate::cache::Cache for Racy {
        async fn get(&self, k: &str) -> Result<Option<String>, crate::cache::CacheError> {
            self.0.get(k).await
        }
        async fn set(
            &self,
            k: &str,
            v: &str,
            t: Option<Duration>,
        ) -> Result<(), crate::cache::CacheError> {
            self.0.set(k, v, t).await
        }
        async fn delete(&self, k: &str) -> Result<(), crate::cache::CacheError> {
            self.0.delete(k).await
        }
        async fn exists(&self, k: &str) -> Result<bool, crate::cache::CacheError> {
            let seen = self.0.exists(k).await;
            self.1.wait().await;
            seen
        }
        async fn clear(&self) -> Result<(), crate::cache::CacheError> {
            self.0.clear().await
        }
        async fn add(
            &self,
            k: &str,
            v: &str,
            t: Option<Duration>,
        ) -> Result<bool, crate::cache::CacheError> {
            self.0.add(k, v, t).await
        }
    }

    /// #1853 — two simultaneous redemptions of one link: exactly one wins.
    #[cfg(feature = "cache")]
    #[tokio::test]
    async fn concurrent_redemptions_of_one_link_yield_one() {
        let cache: std::sync::Arc<dyn crate::cache::Cache> = std::sync::Arc::new(Racy(
            crate::cache::InMemoryCache::new(),
            tokio::sync::Barrier::new(2),
        ));
        let (url, scope) = (link(), scope());
        let (a, b) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                MagicLink::verify_single_use(&scope, &url, SECRET, &cache),
                MagicLink::verify_single_use(&scope, &url, SECRET, &cache),
            )
        })
        .await
        .expect("no deadlock");
        assert_eq!(u8::from(a.is_ok()) + u8::from(b.is_ok()), 1, "{a:?} {b:?}");
    }

    /// #1853 — a cache that keeps nothing cannot enforce single use, so it refuses.
    #[cfg(feature = "cache")]
    #[tokio::test]
    async fn a_null_cache_refuses_single_use() {
        let cache: std::sync::Arc<dyn crate::cache::Cache> =
            std::sync::Arc::new(crate::cache::NullCache);
        let r = MagicLink::verify_single_use(&scope(), &link(), SECRET, &cache).await;
        assert_eq!(r, Err(AuthFlowError::AlreadyUsed));
    }

    /// Accepts reads, fails every write.
    #[cfg(feature = "cache")]
    struct WriteFails;

    #[cfg(feature = "cache")]
    #[async_trait::async_trait]
    impl crate::cache::Cache for WriteFails {
        async fn get(&self, _: &str) -> Result<Option<String>, crate::cache::CacheError> {
            Ok(None)
        }
        async fn set(
            &self,
            _: &str,
            _: &str,
            _: Option<Duration>,
        ) -> Result<(), crate::cache::CacheError> {
            Err(crate::cache::CacheError::Connection("down".into()))
        }
        async fn delete(&self, _: &str) -> Result<(), crate::cache::CacheError> {
            Ok(())
        }
        async fn exists(&self, _: &str) -> Result<bool, crate::cache::CacheError> {
            Ok(false)
        }
        async fn clear(&self) -> Result<(), crate::cache::CacheError> {
            Ok(())
        }
    }

    /// #1853 — a failed marker write would leave the link reusable, so it refuses.
    #[cfg(feature = "cache")]
    #[tokio::test]
    async fn a_failed_marker_write_refuses() {
        let cache: std::sync::Arc<dyn crate::cache::Cache> = std::sync::Arc::new(WriteFails);
        let r = MagicLink::verify_single_use(&scope(), &link(), SECRET, &cache).await;
        assert_eq!(r, Err(AuthFlowError::AlreadyUsed));
    }

    // -------------------------------- query string handling

    #[test]
    fn extract_query_picks_right_param() {
        let url = "https://x/path?a=1&b=2&c=3";
        assert_eq!(extract_query(url, "b"), Some("2".to_owned()));
        assert_eq!(extract_query(url, "missing"), None);
    }

    #[test]
    fn extract_query_handles_url_encoded_value() {
        let url = "https://x/path?email=alice%40x.com";
        assert_eq!(extract_query(url, "email"), Some("alice@x.com".to_owned()));
    }

    #[test]
    fn missing_purpose_param_treated_as_malformed() {
        // Hand-crafted signed URL without purpose
        let url = sign(
            "https://x/r?user_id=42",
            SECRET,
            Some(Duration::from_secs(60)),
        );
        let r = PasswordReset::verify(&scope(), &url, SECRET);
        assert_eq!(r.unwrap_err(), AuthFlowError::Malformed);
    }
}
