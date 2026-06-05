//! Django-shape generic value signer.
//!
//! Mirrors `django.core.signing` — produces and verifies signed
//! string values with tamper detection. Used internally by
//! password-reset URLs, email verification tokens, magic-link
//! authentication, signed cookies.
//!
//! ```ignore
//! use rustango::signing::{Signer, TimestampSigner};
//! use std::time::Duration;
//!
//! // Generic Signer — value + HMAC tag.
//! let signer = Signer::new(b"my-secret-key");
//! let signed = signer.sign("user=42");
//! // signed = "user=42:LX-DqQfXqq...32-char-tag"
//! assert_eq!(signer.unsign(&signed).unwrap(), "user=42");
//!
//! // TimestampSigner adds a unix-time component; loads can enforce a TTL.
//! let ts = TimestampSigner::new(b"my-secret-key");
//! let signed = ts.sign("password-reset:42");
//! // signed = "password-reset:42:<base62 timestamp>:LX-DqQfXqq..."
//! let val = ts.unsign(&signed, Some(Duration::from_secs(3600))).unwrap();
//! assert_eq!(val, "password-reset:42");
//! ```
//!
//! ## Architecture
//!
//! * **HMAC primitive**: `salted_hmac(salt, value, secret)` from
//!   [`crate::crypto`] — purpose-isolated per `salt`.
//! * **Encoding**: tag rendered as URL-safe base64 (no padding).
//! * **Format**: `<value><sep><tag>` for `Signer`,
//!   `<value><sep><base62-timestamp><sep><tag>` for `TimestampSigner`.
//! * **Constant-time comparison**: `crypto::constant_time_compare`
//!   used to verify tags so timing leaks can't recover the secret.
//!
//! Default separator is `:` (Django shape). Configurable per-Signer
//! for callers that need a different field separator.

use std::time::Duration;

use crate::crypto::{constant_time_compare, salted_hmac};
use crate::url_codec::urlsafe_base64_encode;

/// Errors returned by [`Signer::unsign`] / [`TimestampSigner::unsign`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SignError {
    /// Input is missing the separator + tag — not a valid signed value.
    #[error("signing: malformed value (missing tag separator)")]
    Malformed,
    /// HMAC tag doesn't match — value was tampered with.
    #[error("signing: bad signature (tampered or wrong secret)")]
    BadSignature,
    /// `TimestampSigner::unsign(value, Some(max_age))` failed because
    /// the timestamp embedded in `value` is older than `max_age`.
    #[error("signing: signature expired (age {age_secs} > max_age {max_age_secs})")]
    Expired { age_secs: u64, max_age_secs: u64 },
    /// `TimestampSigner` value's timestamp segment doesn't parse as
    /// base62 — value was tampered with or never timestamped.
    #[error("signing: bad timestamp in signed value")]
    BadTimestamp,
}

/// Generic value signer — mirrors `django.core.signing.Signer`.
/// Wraps a `secret` + `salt` (purpose tag) into a sign / unsign
/// pair using salted HMAC-SHA256 for tamper detection.
#[derive(Clone, Debug)]
pub struct Signer {
    secret: Vec<u8>,
    salt: Vec<u8>,
    sep: char,
}

impl Signer {
    /// Construct a signer with the canonical Django defaults:
    /// `sep = ':'`, `salt = "django.core.signing.Signer"`. Callers
    /// who want to isolate purposes (e.g. one secret backing many
    /// token types) should construct via [`Signer::with_salt`].
    #[must_use]
    pub fn new(secret: impl Into<Vec<u8>>) -> Self {
        Self {
            secret: secret.into(),
            salt: b"django.core.signing.Signer".to_vec(),
            sep: ':',
        }
    }

    /// Override the per-Signer salt — derives a purpose-specific
    /// HMAC key so distinct callers can't forge each other's tokens
    /// against the shared secret.
    #[must_use]
    pub fn with_salt(mut self, salt: impl Into<Vec<u8>>) -> Self {
        self.salt = salt.into();
        self
    }

    /// Override the separator between `<value>` and `<tag>`. Default
    /// `:`. Use a char that can't appear inside `value` (Django shape).
    #[must_use]
    pub fn with_sep(mut self, sep: char) -> Self {
        self.sep = sep;
        self
    }

    /// Sign `value` — produce `"<value><sep><base64-tag>"`. The
    /// returned string is safe to embed in URL paths / query
    /// parameters / cookie values (base64-url-safe alphabet).
    #[must_use]
    pub fn sign(&self, value: &str) -> String {
        let tag = self.compute_tag(value.as_bytes());
        format!("{}{}{}", value, self.sep, urlsafe_base64_encode(&tag))
    }

    /// Verify a previously-signed value. Returns the original
    /// `value` portion on success.
    ///
    /// # Errors
    /// * [`SignError::Malformed`] — input missing separator + tag.
    /// * [`SignError::BadSignature`] — tag mismatch (tampering).
    pub fn unsign(&self, signed: &str) -> Result<String, SignError> {
        // Split on the LAST occurrence of `sep` — value may itself
        // contain `sep` chars (Django shape: split from the right).
        let idx = signed
            .char_indices()
            .rev()
            .find(|&(_, c)| c == self.sep)
            .map(|(i, _)| i)
            .ok_or(SignError::Malformed)?;
        let value = &signed[..idx];
        let tag_b64 = &signed[idx + self.sep.len_utf8()..];
        let supplied_tag =
            crate::url_codec::urlsafe_base64_decode(tag_b64).ok_or(SignError::BadSignature)?;
        let expected_tag = self.compute_tag(value.as_bytes());
        if !constant_time_compare(&supplied_tag, &expected_tag) {
            return Err(SignError::BadSignature);
        }
        Ok(value.to_owned())
    }

    fn compute_tag(&self, value: &[u8]) -> Vec<u8> {
        salted_hmac(&self.salt, value, &self.secret)
    }
}

/// Timestamped signer — mirrors `django.core.signing.TimestampSigner`.
/// Adds a base62-encoded Unix timestamp between value and tag so
/// `unsign` can enforce a max-age (TTL) at verification time.
///
/// The shape: `<value><sep><base62 ts><sep><base64 tag>`. The tag
/// is computed over `<value><sep><base62 ts>` so both the value
/// AND the timestamp are tamper-protected.
#[derive(Clone, Debug)]
pub struct TimestampSigner {
    inner: Signer,
}

impl TimestampSigner {
    /// Construct with the canonical Django default salt
    /// (`"django.core.signing.TimestampSigner"`).
    #[must_use]
    pub fn new(secret: impl Into<Vec<u8>>) -> Self {
        Self {
            inner: Signer::new(secret).with_salt("django.core.signing.TimestampSigner"),
        }
    }

    /// Override the salt — same purpose-isolation as
    /// [`Signer::with_salt`].
    #[must_use]
    pub fn with_salt(mut self, salt: impl Into<Vec<u8>>) -> Self {
        self.inner = self.inner.with_salt(salt);
        self
    }

    /// Sign `value` at the current Unix epoch. The timestamp is
    /// taken from `SystemTime::now()` and base62-encoded.
    ///
    /// # Panics
    /// Panics if the system clock is set before 1970-01-01 (which
    /// would make `duration_since(UNIX_EPOCH)` fail). Production
    /// machines never hit this; embedded targets with no RTC might.
    #[must_use]
    pub fn sign(&self, value: &str) -> String {
        self.sign_at(value, current_unix_seconds())
    }

    /// Same as [`Self::sign`] but takes an explicit `unix_seconds`
    /// timestamp — used by tests + replay protection.
    #[must_use]
    pub fn sign_at(&self, value: &str, unix_seconds: u64) -> String {
        let ts = crate::base62::int_to_base62(unix_seconds);
        let payload = format!("{}{}{}", value, self.inner.sep, ts);
        let tag = self.inner.compute_tag(payload.as_bytes());
        format!(
            "{}{}{}",
            payload,
            self.inner.sep,
            urlsafe_base64_encode(&tag)
        )
    }

    /// Verify `signed`, optionally enforcing a max age.
    ///
    /// * `max_age = None` — verify tag only, don't check timestamp.
    ///   Useful when the embedded timestamp is informational only.
    /// * `max_age = Some(duration)` — verify tag AND assert the
    ///   timestamp embedded in `signed` is no older than `duration`
    ///   ago. Returns [`SignError::Expired`] when the value has
    ///   aged out.
    ///
    /// # Errors
    /// * [`SignError::Malformed`] — input missing separator(s) / tag.
    /// * [`SignError::BadSignature`] — tag mismatch (tampering).
    /// * [`SignError::BadTimestamp`] — timestamp segment doesn't
    ///   parse as base62.
    /// * [`SignError::Expired`] — `max_age` set and timestamp too old.
    pub fn unsign(&self, signed: &str, max_age: Option<Duration>) -> Result<String, SignError> {
        self.unsign_at(signed, max_age, current_unix_seconds())
    }

    /// Same as [`Self::unsign`] but takes an explicit `now_secs`
    /// reference timestamp — used by tests + replay protection.
    pub fn unsign_at(
        &self,
        signed: &str,
        max_age: Option<Duration>,
        now_secs: u64,
    ) -> Result<String, SignError> {
        // First peel off `<tag>` from the end — the inner `Signer`
        // takes care of that.
        let payload = self.inner.unsign(signed)?;
        // Now `payload = "<value><sep><base62 ts>"`. Split off the
        // timestamp on the LAST `sep`.
        let idx = payload
            .char_indices()
            .rev()
            .find(|&(_, c)| c == self.inner.sep)
            .map(|(i, _)| i)
            .ok_or(SignError::Malformed)?;
        let value = &payload[..idx];
        let ts_str = &payload[idx + self.inner.sep.len_utf8()..];
        let ts = crate::base62::base62_to_int(ts_str).map_err(|_| SignError::BadTimestamp)?;
        if let Some(max) = max_age {
            let age = now_secs.saturating_sub(ts);
            let max_secs = max.as_secs();
            if age > max_secs {
                return Err(SignError::Expired {
                    age_secs: age,
                    max_age_secs: max_secs,
                });
            }
        }
        Ok(value.to_owned())
    }
}

/// Current Unix epoch seconds. Panics on pre-1970 system clocks
/// (same shape as `SystemTime::duration_since`).
fn current_unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------- Signer --------

    #[test]
    fn signer_round_trips_simple_value() {
        let s = Signer::new(b"secret");
        let signed = s.sign("user=42");
        assert_eq!(s.unsign(&signed).unwrap(), "user=42");
    }

    #[test]
    fn signer_output_starts_with_value_then_sep() {
        let s = Signer::new(b"secret");
        let signed = s.sign("hello");
        assert!(signed.starts_with("hello:"));
    }

    #[test]
    fn signer_detects_tampering() {
        let s = Signer::new(b"secret");
        let signed = s.sign("hello");
        // Modify one byte of the value portion.
        let tampered = signed.replacen('h', "H", 1);
        assert_eq!(s.unsign(&tampered), Err(SignError::BadSignature));
    }

    #[test]
    fn signer_detects_tag_truncation() {
        let s = Signer::new(b"secret");
        let signed = s.sign("hello");
        let truncated = &signed[..signed.len() - 2];
        assert_eq!(s.unsign(truncated), Err(SignError::BadSignature));
    }

    #[test]
    fn signer_rejects_missing_separator() {
        let s = Signer::new(b"secret");
        let err = s.unsign("nosepoir").unwrap_err();
        assert_eq!(err, SignError::Malformed);
    }

    #[test]
    fn signer_different_secrets_distinct_tags() {
        let a = Signer::new(b"secret-1");
        let b = Signer::new(b"secret-2");
        let signed_a = a.sign("hello");
        // Signer B with a different secret can't unsign A's value.
        assert_eq!(b.unsign(&signed_a), Err(SignError::BadSignature));
    }

    #[test]
    fn signer_with_salt_isolates_purposes() {
        let a = Signer::new(b"shared-secret").with_salt("purpose-A");
        let b = Signer::new(b"shared-secret").with_salt("purpose-B");
        let signed = a.sign("user=42");
        assert!(a.unsign(&signed).is_ok());
        // B's salt is different → can't verify A's signature.
        assert_eq!(b.unsign(&signed), Err(SignError::BadSignature));
    }

    #[test]
    fn signer_value_can_contain_separator() {
        // Value with internal `:` — we split on LAST `:`, so the value
        // survives intact.
        let s = Signer::new(b"secret");
        let signed = s.sign("user:42:active");
        assert_eq!(s.unsign(&signed).unwrap(), "user:42:active");
    }

    #[test]
    fn signer_empty_value_works() {
        let s = Signer::new(b"secret");
        let signed = s.sign("");
        assert_eq!(s.unsign(&signed).unwrap(), "");
    }

    #[test]
    fn signer_custom_separator() {
        let s = Signer::new(b"secret").with_sep('|');
        let signed = s.sign("hello");
        assert!(signed.contains('|'));
        assert!(!signed.contains(':'));
        assert_eq!(s.unsign(&signed).unwrap(), "hello");
    }

    // -------- TimestampSigner --------

    #[test]
    fn timestamp_signer_round_trip_at_now() {
        let s = TimestampSigner::new(b"secret");
        let signed = s.sign("user=42");
        // No max_age check — round-trips regardless of how stale.
        assert_eq!(s.unsign(&signed, None).unwrap(), "user=42");
    }

    #[test]
    fn timestamp_signer_within_max_age_passes() {
        let s = TimestampSigner::new(b"secret");
        let signed = s.sign_at("user=42", 1_000);
        // Now is 30s later — well within 1h max_age.
        assert_eq!(
            s.unsign_at(&signed, Some(Duration::from_secs(3600)), 1_030)
                .unwrap(),
            "user=42"
        );
    }

    #[test]
    fn timestamp_signer_past_max_age_expired() {
        let s = TimestampSigner::new(b"secret");
        let signed = s.sign_at("user=42", 1_000);
        // Now is 7200s later — past 3600s max_age.
        let err = s
            .unsign_at(&signed, Some(Duration::from_secs(3600)), 8_200)
            .unwrap_err();
        match err {
            SignError::Expired {
                age_secs,
                max_age_secs,
            } => {
                assert_eq!(age_secs, 7200);
                assert_eq!(max_age_secs, 3600);
            }
            other => panic!("expected Expired, got {other:?}"),
        }
    }

    #[test]
    fn timestamp_signer_tamper_detection() {
        let s = TimestampSigner::new(b"secret");
        let signed = s.sign_at("user=42", 1_000);
        let tampered = signed.replacen("user", "USER", 1);
        assert_eq!(
            s.unsign_at(&tampered, None, 1_000),
            Err(SignError::BadSignature)
        );
    }

    #[test]
    fn timestamp_signer_detects_timestamp_tampering() {
        // Attacker can't roll back the embedded timestamp by editing
        // the base62 segment — the tag covers the whole payload.
        let s = TimestampSigner::new(b"secret");
        let signed = s.sign_at("user=42", 8_000);
        // Find the timestamp segment and swap a digit.
        // Signed = "user=42:<base62 8000>:<tag>"
        // Tamper the last char of the base62 timestamp segment by
        // swapping in a different valid base62 char.
        let parts: Vec<&str> = signed.rsplitn(2, ':').collect();
        let head = parts[1]; // value:ts
        let tag = parts[0]; // tag
        let head_parts: Vec<&str> = head.rsplitn(2, ':').collect();
        let ts = head_parts[0];
        let value = head_parts[1];
        // Replace last char of timestamp: e.g. '0' → '1', else 'a' → 'b'.
        let mut tampered_ts: String = ts.to_owned();
        let last = tampered_ts.pop().unwrap();
        tampered_ts.push(if last == '0' { '1' } else { '0' });
        let tampered = format!("{}:{}:{}", value, tampered_ts, tag);
        assert_eq!(
            s.unsign_at(&tampered, None, 8_000),
            Err(SignError::BadSignature)
        );
    }

    #[test]
    fn timestamp_signer_max_age_none_skips_check() {
        // Even a year-old token verifies when max_age = None.
        let s = TimestampSigner::new(b"secret");
        let signed = s.sign_at("user=42", 1_000);
        let one_year_later = 1_000 + 365 * 86_400;
        assert_eq!(
            s.unsign_at(&signed, None, one_year_later).unwrap(),
            "user=42"
        );
    }

    #[test]
    fn timestamp_signer_with_salt_isolates() {
        let a = TimestampSigner::new(b"shared").with_salt("purpose-A");
        let b = TimestampSigner::new(b"shared").with_salt("purpose-B");
        let signed = a.sign_at("user=42", 1_000);
        assert_eq!(
            b.unsign_at(&signed, None, 1_000),
            Err(SignError::BadSignature)
        );
    }
}
