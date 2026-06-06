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
//! use rustango::auth_flows::{PasswordReset, AuthFlowError};
//! use std::time::Duration;
//!
//! let secret: &[u8] = b"32-byte-app-secret-...";
//!
//! // Step 1: issue
//! let url = PasswordReset::issue(
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
//! match PasswordReset::verify(&incoming_url, secret) {
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

use std::time::Duration;

use crate::signed_url::{sign, verify, SignedUrlError};

/// Errors from auth-flow helpers.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
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
    /// the account.
    #[must_use]
    pub fn issue(base_url: &str, user_id: i64, secret: &[u8], ttl: Duration) -> String {
        let url = format!(
            "{}?user_id={}&purpose={}",
            base_url.trim_end_matches('?'),
            user_id,
            Self::PURPOSE,
        );
        sign(&url, secret, Some(ttl))
    }

    /// Verify a reset URL. On success returns the `user_id` extracted from
    /// the URL — caller writes the new password against this id.
    ///
    /// # Errors
    /// [`AuthFlowError`] variants describe the failure mode.
    pub fn verify(url: &str, secret: &[u8]) -> Result<i64, AuthFlowError> {
        verify(url, secret)?;
        let purpose = extract_query(url, "purpose").ok_or(AuthFlowError::Malformed)?;
        if purpose != Self::PURPOSE {
            return Err(AuthFlowError::WrongPurpose(purpose));
        }
        let user_id_str = extract_query(url, "user_id").ok_or(AuthFlowError::Malformed)?;
        user_id_str
            .parse::<i64>()
            .map_err(|_| AuthFlowError::Malformed)
    }
}

/// Django-shape `PasswordResetConfirmView` — verifies a reset URL,
/// validates the new password, hashes it, and updates the named
/// user row. Returns the `user_id` on success. Issue #391.
///
/// Sensible defaults:
/// - `user_table` = `"rustango_users"`
/// - `pk_column` = `"id"`
/// - `password_column` = `"password_hash"`
///
/// The password is rejected if shorter than 8 characters
/// (`AuthFlowError::WeakPassword`); operators wanting stricter rules
/// run their own validators on the input before calling this helper.
///
/// Pairs with [`PasswordReset::issue`] — issue the URL, email it,
/// and call this helper from your POST `/password-reset/confirm`
/// endpoint to land the new password.
///
/// # Errors
/// - [`AuthFlowError`] from `PasswordReset::verify` (Malformed,
///   InvalidSignature, Expired, WrongPurpose).
/// - [`AuthFlowError::WeakPassword`] when `new_password.len() < 8`.
/// - [`AuthFlowError::Database`] for SQL / driver failures.
#[cfg(feature = "passwords")]
pub async fn confirm_password_reset_pool(
    pool: &crate::sql::Pool,
    url: &str,
    new_password: &str,
    secret: &[u8],
) -> Result<i64, AuthFlowError> {
    confirm_password_reset_pool_into(
        pool,
        url,
        new_password,
        secret,
        "rustango_users",
        "id",
        "password_hash",
    )
    .await
}

/// As [`confirm_password_reset_pool`] but writes the new hash into a
/// caller-named table / columns. Use when the user model lives in a
/// custom table (e.g. tenant `app_users`) rather than the framework's
/// default `rustango_users`. Issue #391.
///
/// # Errors
/// Same shape as [`confirm_password_reset_pool`].
#[cfg(feature = "passwords")]
pub async fn confirm_password_reset_pool_into(
    pool: &crate::sql::Pool,
    url: &str,
    new_password: &str,
    secret: &[u8],
    user_table: &str,
    pk_column: &str,
    password_column: &str,
) -> Result<i64, AuthFlowError> {
    let user_id = PasswordReset::verify(url, secret)?;
    if new_password.len() < 8 {
        return Err(AuthFlowError::WeakPassword(
            "Password must be at least 8 characters.".into(),
        ));
    }
    let hash =
        crate::passwords::hash(new_password).map_err(|e| AuthFlowError::Database(e.to_string()))?;
    let dialect = pool.dialect();
    let t = dialect.quote_ident(user_table);
    let pw = dialect.quote_ident(password_column);
    let pk = dialect.quote_ident(pk_column);
    let p1 = dialect.placeholder(1);
    let p2 = dialect.placeholder(2);
    let sql = format!("UPDATE {t} SET {pw} = {p1} WHERE {pk} = {p2}");
    crate::sql::raw_execute_pool(
        pool,
        &sql,
        vec![
            crate::core::SqlValue::String(hash),
            crate::core::SqlValue::I64(user_id),
        ],
    )
    .await
    .map_err(|e| AuthFlowError::Database(e.to_string()))?;
    Ok(user_id)
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
        sign(&url, secret, Some(ttl))
    }

    /// Verify the URL and return the `(user_id, email)` it was issued for.
    /// Caller compares email against the user's current email to detect
    /// stale verification links.
    ///
    /// # Errors
    /// As [`PasswordReset::verify`].
    pub fn verify(url: &str, secret: &[u8]) -> Result<(i64, String), AuthFlowError> {
        verify(url, secret)?;
        let purpose = extract_query(url, "purpose").ok_or(AuthFlowError::Malformed)?;
        if purpose != Self::PURPOSE {
            return Err(AuthFlowError::WrongPurpose(purpose));
        }
        let user_id_str = extract_query(url, "user_id").ok_or(AuthFlowError::Malformed)?;
        let user_id = user_id_str
            .parse::<i64>()
            .map_err(|_| AuthFlowError::Malformed)?;
        let email = extract_query(url, "email").ok_or(AuthFlowError::Malformed)?;
        Ok((user_id, email))
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
    pub fn issue(base_url: &str, email: &str, secret: &[u8], ttl: Duration) -> String {
        let url = format!(
            "{}?email={}&purpose={}",
            base_url.trim_end_matches('?'),
            url_encode(email),
            Self::PURPOSE,
        );
        sign(&url, secret, Some(ttl))
    }

    /// Verify the URL and return the email it was issued for.
    ///
    /// # Errors
    /// As [`PasswordReset::verify`].
    pub fn verify(url: &str, secret: &[u8]) -> Result<String, AuthFlowError> {
        verify(url, secret)?;
        let purpose = extract_query(url, "purpose").ok_or(AuthFlowError::Malformed)?;
        if purpose != Self::PURPOSE {
            return Err(AuthFlowError::WrongPurpose(purpose));
        }
        extract_query(url, "email").ok_or(AuthFlowError::Malformed)
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

    // -------------------------------- Password reset

    #[test]
    fn password_reset_issue_and_verify_roundtrip() {
        let url = PasswordReset::issue(
            "https://app.example.com/reset",
            42,
            SECRET,
            Duration::from_secs(3600),
        );
        let user_id = PasswordReset::verify(&url, SECRET).unwrap();
        assert_eq!(user_id, 42);
    }

    #[test]
    fn password_reset_wrong_secret_fails() {
        let url = PasswordReset::issue("https://x/r", 42, SECRET, Duration::from_secs(3600));
        let r = PasswordReset::verify(&url, b"different");
        assert_eq!(r.unwrap_err(), AuthFlowError::InvalidSignature);
    }

    #[test]
    fn password_reset_tampered_user_id_fails() {
        let url = PasswordReset::issue("https://x/r", 42, SECRET, Duration::from_secs(3600));
        let tampered = url.replace("user_id=42", "user_id=99");
        let r = PasswordReset::verify(&tampered, SECRET);
        assert_eq!(r.unwrap_err(), AuthFlowError::InvalidSignature);
    }

    #[test]
    fn password_reset_rejects_email_verification_token() {
        // Same URL shape but issued for a different purpose
        let url = EmailVerification::issue(
            "https://x/r",
            42,
            "alice@x.com",
            SECRET,
            Duration::from_secs(3600),
        );
        let r = PasswordReset::verify(&url, SECRET);
        assert!(matches!(r, Err(AuthFlowError::WrongPurpose(_))));
    }

    // -------------------------------- Email verification

    #[test]
    fn email_verification_roundtrip() {
        let url = EmailVerification::issue(
            "https://x/v",
            42,
            "alice@example.com",
            SECRET,
            Duration::from_secs(86_400),
        );
        let (uid, email) = EmailVerification::verify(&url, SECRET).unwrap();
        assert_eq!(uid, 42);
        assert_eq!(email, "alice@example.com");
    }

    #[test]
    fn email_verification_handles_special_chars() {
        let url = EmailVerification::issue(
            "https://x/v",
            42,
            "a+b@example.com",
            SECRET,
            Duration::from_secs(86_400),
        );
        let (_, email) = EmailVerification::verify(&url, SECRET).unwrap();
        assert_eq!(email, "a+b@example.com");
    }

    #[test]
    fn email_verification_rejects_password_reset_token() {
        let url = PasswordReset::issue("https://x/v", 42, SECRET, Duration::from_secs(3600));
        let r = EmailVerification::verify(&url, SECRET);
        assert!(matches!(r, Err(AuthFlowError::WrongPurpose(_))));
    }

    // -------------------------------- Magic link

    #[test]
    fn magic_link_roundtrip() {
        let url = MagicLink::issue(
            "https://x/login",
            "alice@example.com",
            SECRET,
            Duration::from_secs(900),
        );
        let email = MagicLink::verify(&url, SECRET).unwrap();
        assert_eq!(email, "alice@example.com");
    }

    #[test]
    fn magic_link_rejects_password_reset_token() {
        let url = PasswordReset::issue("https://x/r", 42, SECRET, Duration::from_secs(3600));
        let r = MagicLink::verify(&url, SECRET);
        assert!(matches!(r, Err(AuthFlowError::WrongPurpose(_))));
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
        let r = PasswordReset::verify(&url, SECRET);
        assert_eq!(r.unwrap_err(), AuthFlowError::Malformed);
    }
}
