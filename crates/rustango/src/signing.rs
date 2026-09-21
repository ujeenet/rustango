//! Django-shape value signer.
//!
//! Mirrors `django.core.signing`: it signs string values and detects
//! tampering on the way back. Used by password-reset URLs, email
//! verification tokens, magic links and signed cookies.
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
//! How it works:
//!
//! * Tags come from `salted_hmac(salt, value, secret)` in
//!   [`crate::crypto`]. A different `salt` gives a different key, so
//!   one purpose cannot forge another's tokens.
//! * The tag is URL-safe base64, no padding.
//! * Format is `<value><sep><tag>`, or
//!   `<value><sep><base62 timestamp><sep><tag>` with a timestamp.
//! * Tags are checked with `crypto::constant_time_compare`. Never
//!   compare them with `==`: the timing difference leaks the tag.
//!
//! The separator defaults to `:` and can be changed per signer.

use std::time::Duration;

use crate::crypto::{constant_time_compare, salted_hmac};
use crate::url_codec::urlsafe_base64_encode;

/// Errors returned by [`Signer::unsign`] / [`TimestampSigner::unsign`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SignError {
    /// No separator and tag, so this is not a signed value.
    #[error("signing: malformed value (missing tag separator)")]
    Malformed,
    /// The tag does not match. The value was tampered with.
    #[error("signing: bad signature (tampered or wrong secret)")]
    BadSignature,
    /// The embedded timestamp is older than the `max_age` passed to
    /// [`TimestampSigner::unsign`].
    #[error("signing: signature expired (age {age_secs} > max_age {max_age_secs})")]
    Expired { age_secs: u64, max_age_secs: u64 },
    /// The timestamp segment is not base62. The value was tampered
    /// with, or it was never timestamped.
    #[error("signing: bad timestamp in signed value")]
    BadTimestamp,
}

/// Value signer, like `django.core.signing.Signer`. Holds a secret
/// and a salt, and signs or verifies with salted HMAC-SHA256.
#[derive(Clone, Debug)]
pub struct Signer {
    secret: Vec<u8>,
    salt: Vec<u8>,
    sep: char,
}

impl Signer {
    /// Signer with the Django defaults: `sep = ':'` and
    /// `salt = "django.core.signing.Signer"`. If one secret backs
    /// several token types, add [`Signer::with_salt`].
    #[must_use]
    pub fn new(secret: impl Into<Vec<u8>>) -> Self {
        Self {
            secret: secret.into(),
            salt: b"django.core.signing.Signer".to_vec(),
            sep: ':',
        }
    }

    /// Set the salt. Each salt derives its own HMAC key, so callers
    /// sharing one secret cannot forge each other's tokens.
    #[must_use]
    pub fn with_salt(mut self, salt: impl Into<Vec<u8>>) -> Self {
        self.salt = salt.into();
        self
    }

    /// Set the separator between `<value>` and `<tag>`. Defaults to
    /// `:`. Pick a char that cannot appear inside `value`.
    #[must_use]
    pub fn with_sep(mut self, sep: char) -> Self {
        self.sep = sep;
        self
    }

    /// Sign `value`, returning `"<value><sep><base64 tag>"`. The tag
    /// uses the URL-safe base64 alphabet, so the result drops into a
    /// URL path, query parameter or cookie as-is.
    #[must_use]
    pub fn sign(&self, value: &str) -> String {
        let tag = self.compute_tag(value.as_bytes());
        format!("{}{}{}", value, self.sep, urlsafe_base64_encode(&tag))
    }

    /// Verify a signed value and return the original `value`.
    ///
    /// # Errors
    /// * [`SignError::Malformed`] — no separator and tag.
    /// * [`SignError::BadSignature`] — tag mismatch (tampering).
    pub fn unsign(&self, signed: &str) -> Result<String, SignError> {
        // Split on the LAST `sep`: the value may contain `sep` too.
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

/// Timestamped signer, like `django.core.signing.TimestampSigner`.
/// It puts a base62 Unix timestamp between the value and the tag so
/// `unsign` can expire old values.
///
/// Shape: `<value><sep><base62 ts><sep><base64 tag>`. The tag covers
/// `<value><sep><base62 ts>`, so the timestamp is protected too and
/// an attacker cannot roll it back.
#[derive(Clone, Debug)]
pub struct TimestampSigner {
    inner: Signer,
}

impl TimestampSigner {
    /// Signer with the Django default salt
    /// (`"django.core.signing.TimestampSigner"`).
    #[must_use]
    pub fn new(secret: impl Into<Vec<u8>>) -> Self {
        Self {
            inner: Signer::new(secret).with_salt("django.core.signing.TimestampSigner"),
        }
    }

    /// Set the salt. Same purpose isolation as
    /// [`Signer::with_salt`].
    #[must_use]
    pub fn with_salt(mut self, salt: impl Into<Vec<u8>>) -> Self {
        self.inner = self.inner.with_salt(salt);
        self
    }

    /// Sign `value` with the current time from `SystemTime::now()`.
    ///
    /// # Panics
    /// Panics if the system clock reads before 1970-01-01. Only a
    /// machine with no real-time clock is likely to hit this.
    #[must_use]
    pub fn sign(&self, value: &str) -> String {
        self.sign_at(value, current_unix_seconds())
    }

    /// [`Self::sign`] with an explicit `unix_seconds`. Used by tests
    /// and replay protection.
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

    /// Verify `signed` and return the value.
    ///
    /// * `max_age = Some(d)` — check the tag, then reject a value
    ///   older than `d` with [`SignError::Expired`].
    /// * `max_age = None` — check the tag only. The value never
    ///   expires, so pass a duration for anything security-sensitive
    ///   such as a reset link or a magic link.
    ///
    /// # Errors
    /// * [`SignError::Malformed`] — missing separator or tag.
    /// * [`SignError::BadSignature`] — tag mismatch (tampering).
    /// * [`SignError::BadTimestamp`] — timestamp is not base62.
    /// * [`SignError::Expired`] — value older than `max_age`.
    pub fn unsign(&self, signed: &str, max_age: Option<Duration>) -> Result<String, SignError> {
        self.unsign_at(signed, max_age, current_unix_seconds())
    }

    /// [`Self::unsign`] with an explicit `now_secs`. Used by tests
    /// and replay protection.
    pub fn unsign_at(
        &self,
        signed: &str,
        max_age: Option<Duration>,
        now_secs: u64,
    ) -> Result<String, SignError> {
        // The inner `Signer` strips and checks the trailing tag.
        let payload = self.inner.unsign(signed)?;
        // `payload` is now "<value><sep><base62 ts>": split the
        // timestamp off the LAST `sep`.
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

/// Django-parity
/// [`django.core.signing.dumps(obj, key=None, salt='django.core.signing',
/// serializer=JSONSerializer, compress=False)`](https://docs.djangoproject.com/en/6.0/topics/signing/#django.core.signing.dumps) —
/// serialize `value` as JSON, encode it as URL-safe base64, then
/// sign it with a [TimestampSigner] built from `salt` and `secret`.
///
/// Output is `"<base64 JSON>:<base62 ts>:<base64 tag>"` and needs no
/// escaping in a URL or cookie. The base64 step means the JSON can
/// hold `:` without confusing the parser.
///
/// # Errors
/// Returns [`serde_json::Error`] if `value` fails to serialize, for
/// example a struct that rejects NaN floats.
///
/// ```ignore
/// use rustango::signing::{dumps, loads};
/// use std::time::Duration;
/// #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
/// struct Reset { user_id: u64, action: String }
///
/// let token = dumps(
///     &Reset { user_id: 42, action: "password_reset".into() },
///     "password-reset-salt",
///     b"app-secret-key",
/// )?;
/// // Embed `token` in a URL: /reset/<token>/
///
/// let v: Reset = loads(&token, "password-reset-salt", b"app-secret-key",
///                      Some(Duration::from_secs(3600)))?;
/// assert_eq!(v.user_id, 42);
/// ```
pub fn dumps<T: serde::Serialize>(
    value: &T,
    salt: &str,
    secret: &[u8],
) -> Result<String, serde_json::Error> {
    let json = serde_json::to_vec(value)?;
    let payload = crate::url_codec::urlsafe_base64_encode(&json);
    let signer = TimestampSigner::new(secret.to_vec()).with_salt(salt);
    Ok(signer.sign(&payload))
}

/// Django-parity
/// [`django.core.signing.loads(s, key=None, salt='django.core.signing',
/// serializer=JSONSerializer, max_age=None)`](https://docs.djangoproject.com/en/6.0/topics/signing/#django.core.signing.loads) —
/// the inverse of [`dumps`]: verify, base64-decode, deserialize.
///
/// `max_age` expires the token. `None` skips that check, so the
/// token lives forever. Pass a duration for reset links and other
/// security-sensitive tokens.
///
/// # Errors
/// * [`LoadsError::Sign`] — tampered, malformed or expired; the
///   inner [`SignError`] says which.
/// * [`LoadsError::Decode`] — payload is not URL-safe base64.
/// * [`LoadsError::Deserialize`] — JSON did not parse as `T`.
pub fn loads<T: serde::de::DeserializeOwned>(
    signed: &str,
    salt: &str,
    secret: &[u8],
    max_age: Option<Duration>,
) -> Result<T, LoadsError> {
    let signer = TimestampSigner::new(secret.to_vec()).with_salt(salt);
    let payload = signer.unsign(signed, max_age).map_err(LoadsError::Sign)?;
    let bytes = crate::url_codec::urlsafe_base64_decode(&payload).ok_or(LoadsError::Decode)?;
    serde_json::from_slice(&bytes).map_err(|e| LoadsError::Deserialize(e.to_string()))
}

/// Failure modes for [`loads`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LoadsError {
    /// Verification failed: tampering, wrong salt, or expired. The
    /// inner [`SignError`] gives the exact cause.
    #[error("loads: {0}")]
    Sign(#[from] SignError),
    /// The payload is not URL-safe base64.
    #[error("loads: base64 decode failed")]
    Decode,
    /// The JSON did not parse into the target type.
    #[error("loads: JSON deserialize failed: {0}")]
    Deserialize(String),
}

/// Current Unix epoch seconds. Panics if the clock reads before
/// 1970.
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
        // Change one byte of the value.
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
        // A different secret cannot unsign A's value.
        assert_eq!(b.unsign(&signed_a), Err(SignError::BadSignature));
    }

    #[test]
    fn signer_with_salt_isolates_purposes() {
        let a = Signer::new(b"shared-secret").with_salt("purpose-A");
        let b = Signer::new(b"shared-secret").with_salt("purpose-B");
        let signed = a.sign("user=42");
        assert!(a.unsign(&signed).is_ok());
        // B has another salt, so it cannot verify A's signature.
        assert_eq!(b.unsign(&signed), Err(SignError::BadSignature));
    }

    #[test]
    fn signer_value_can_contain_separator() {
        // We split on the LAST `:`, so an internal `:` survives.
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
        // No max_age, so age does not matter.
        assert_eq!(s.unsign(&signed, None).unwrap(), "user=42");
    }

    #[test]
    fn timestamp_signer_within_max_age_passes() {
        let s = TimestampSigner::new(b"secret");
        let signed = s.sign_at("user=42", 1_000);
        // 30s later, well inside the 1h max_age.
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
        // 7200s later, past the 3600s max_age.
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
        // The tag covers the whole payload, so an attacker cannot
        // roll the timestamp back by editing the base62 segment.
        let s = TimestampSigner::new(b"secret");
        let signed = s.sign_at("user=42", 8_000);
        // Signed = "user=42:<base62 8000>:<tag>". Swap the last char
        // of the timestamp for another valid base62 char.
        let parts: Vec<&str> = signed.rsplitn(2, ':').collect();
        let head = parts[1]; // value:ts
        let tag = parts[0]; // tag
        let head_parts: Vec<&str> = head.rsplitn(2, ':').collect();
        let ts = head_parts[0];
        let value = head_parts[1];
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
        // With max_age = None, even a year-old token verifies.
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

    // -------- dumps / loads (Django parity) --------

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    struct ResetPayload {
        user_id: u64,
        action: String,
    }

    #[test]
    fn dumps_loads_round_trip() {
        let v = ResetPayload {
            user_id: 42,
            action: "password_reset".into(),
        };
        let token = dumps(&v, "reset-salt", b"secret").unwrap();
        let got: ResetPayload = loads(&token, "reset-salt", b"secret", None).unwrap();
        assert_eq!(got, v);
    }

    #[test]
    fn dumps_loads_detects_tampering() {
        let v = ResetPayload {
            user_id: 42,
            action: "x".into(),
        };
        let token = dumps(&v, "salt", b"secret").unwrap();
        // Flip the first char of the base64 payload.
        let tampered = if let Some(stripped) = token.strip_prefix('e') {
            format!("X{stripped}")
        } else {
            format!("X{}", &token[1..])
        };
        let err = loads::<ResetPayload>(&tampered, "salt", b"secret", None).unwrap_err();
        assert!(matches!(err, LoadsError::Sign(_) | LoadsError::Decode));
    }

    #[test]
    fn dumps_loads_wrong_salt_fails() {
        let v = ResetPayload {
            user_id: 42,
            action: "x".into(),
        };
        let token = dumps(&v, "salt-A", b"secret").unwrap();
        let err = loads::<ResetPayload>(&token, "salt-B", b"secret", None).unwrap_err();
        assert!(matches!(err, LoadsError::Sign(SignError::BadSignature)));
    }

    #[test]
    fn dumps_loads_wrong_secret_fails() {
        let v = ResetPayload {
            user_id: 42,
            action: "x".into(),
        };
        let token = dumps(&v, "salt", b"secret-1").unwrap();
        let err = loads::<ResetPayload>(&token, "salt", b"secret-2", None).unwrap_err();
        assert!(matches!(err, LoadsError::Sign(SignError::BadSignature)));
    }

    #[test]
    fn dumps_loads_wrong_type_surfaces_as_deserialize_error() {
        // Sign one shape, read it back as another: JSON error.
        #[derive(serde::Serialize)]
        struct Wrong {
            user: String, // string instead of u64
        }
        let token = dumps(
            &Wrong {
                user: "alice".into(),
            },
            "salt",
            b"secret",
        )
        .unwrap();
        let err = loads::<ResetPayload>(&token, "salt", b"secret", None).unwrap_err();
        assert!(matches!(err, LoadsError::Deserialize(_)));
    }

    #[test]
    fn dumps_loads_works_with_simple_types() {
        // Plain integer, string and list all serialize as JSON.
        let token = dumps(&42u64, "salt", b"secret").unwrap();
        let got: u64 = loads(&token, "salt", b"secret", None).unwrap();
        assert_eq!(got, 42);

        let token = dumps(&"hello", "salt", b"secret").unwrap();
        let got: String = loads(&token, "salt", b"secret", None).unwrap();
        assert_eq!(got, "hello");

        let token = dumps(&vec![1, 2, 3], "salt", b"secret").unwrap();
        let got: Vec<i32> = loads(&token, "salt", b"secret", None).unwrap();
        assert_eq!(got, vec![1, 2, 3]);
    }
}
