//! Minimal JWT (HS256) — sign, verify, decode.
//!
//! Standalone alternative to [`crate::tenancy::jwt_lifecycle`] (which
//! wraps this with refresh + JTI blacklist + sliding rotation, but is
//! gated on the `tenancy` feature). Reach for this module when you
//! want plain JWTs for:
//!
//! - Magic-link tokens that carry a few claims (user id, purpose, exp)
//! - Service-to-service tokens (sister to [`crate::hmac_auth`] —
//!   pick HMAC for AWS-style request signing, JWT for stateless
//!   bearer tokens)
//! - Single-sign-on tokens you hand to a third party
//!
//! ## Algorithm
//!
//! HS256 only — symmetric, single shared secret. RS256 / ES256 (public
//! / private keypair) are out of scope: the rustls / ring deps would
//! triple the always-on dep tree, and most callers picking JWT in a
//! single-service codebase use HS256 anyway.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::jwt::{Claims, encode, decode};
//! use std::time::Duration;
//!
//! let secret = b"thirty-two-bytes-of-shared-secret-mat";
//!
//! let mut claims = Claims::new("user-42")
//!     .ttl(Duration::from_secs(3600))
//!     .issuer("api.example.com");
//! claims.set("role", "admin");
//!
//! let token = encode(&claims, secret).unwrap();
//!
//! let verified = decode(&token, secret).unwrap();
//! assert_eq!(verified.subject(), Some("user-42"));
//! assert_eq!(verified.get::<String>("role").as_deref(), Some("admin"));
//! ```

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Map, Value};
use subtle::ConstantTimeEq;

#[derive(Debug, thiserror::Error)]
pub enum JwtError {
    #[error("malformed token: expected three base64url segments")]
    Malformed,
    #[error("unsupported algorithm: {0} (only HS256)")]
    UnsupportedAlg(String),
    #[error("signature mismatch")]
    BadSignature,
    #[error("token expired (exp={0})")]
    Expired(u64),
    #[error("token not yet valid (nbf={0})")]
    NotYetValid(u64),
    #[error("decode error: {0}")]
    Decode(String),
}

/// JWT claims — wraps a JSON object so callers can mix standard
/// claims (`sub`, `exp`, `iat`, `iss`, `aud`, `nbf`, `jti`) with
/// arbitrary extension fields.
#[derive(Debug, Clone, Default)]
pub struct Claims {
    inner: Map<String, Value>,
}

impl Claims {
    /// New claims with `sub` set + `iat` set to now.
    #[must_use]
    pub fn new(subject: impl Into<String>) -> Self {
        let mut c = Self::default();
        c.inner.insert("sub".into(), Value::String(subject.into()));
        c.inner.insert("iat".into(), Value::from(now_secs()));
        c
    }

    /// Empty claims (no subject, no iat). Useful when callers want
    /// total control over the payload.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Set an arbitrary claim. Reserved standard names work too.
    pub fn set<T: Serialize>(&mut self, name: impl Into<String>, value: T) {
        if let Ok(v) = serde_json::to_value(value) {
            self.inner.insert(name.into(), v);
        }
    }

    /// Get a typed claim. Returns `None` for missing or wrong-shaped
    /// values.
    pub fn get<T: DeserializeOwned>(&self, name: &str) -> Option<T> {
        self.inner
            .get(name)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    #[must_use]
    pub fn subject(&self) -> Option<&str> {
        self.inner.get("sub").and_then(Value::as_str)
    }

    #[must_use]
    pub fn issuer(self, iss: impl Into<String>) -> Self {
        let mut c = self;
        c.inner.insert("iss".into(), Value::String(iss.into()));
        c
    }

    #[must_use]
    pub fn audience(self, aud: impl Into<String>) -> Self {
        let mut c = self;
        c.inner.insert("aud".into(), Value::String(aud.into()));
        c
    }

    /// Set both `iat` (now) + `exp` (now + ttl) in one call.
    #[must_use]
    pub fn ttl(self, ttl: Duration) -> Self {
        let mut c = self;
        let now = now_secs();
        c.inner.insert("iat".into(), Value::from(now));
        c.inner.insert("exp".into(), Value::from(now + ttl.as_secs()));
        c
    }

    /// Override `exp` to an absolute unix-seconds value.
    #[must_use]
    pub fn expires_at(self, unix_secs: u64) -> Self {
        let mut c = self;
        c.inner.insert("exp".into(), Value::from(unix_secs));
        c
    }

    /// Override `nbf` (not-before) to an absolute unix-seconds value.
    #[must_use]
    pub fn not_before(self, unix_secs: u64) -> Self {
        let mut c = self;
        c.inner.insert("nbf".into(), Value::from(unix_secs));
        c
    }

    /// Set `jti` — token id, useful for blacklisting after use
    /// (typical magic-link pattern).
    #[must_use]
    pub fn jti(self, jti: impl Into<String>) -> Self {
        let mut c = self;
        c.inner.insert("jti".into(), Value::String(jti.into()));
        c
    }

    fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(&self.inner).unwrap_or_else(|_| b"{}".to_vec())
    }

    fn from_json(json: &[u8]) -> Result<Self, JwtError> {
        let inner: Map<String, Value> = serde_json::from_slice(json)
            .map_err(|e| JwtError::Decode(format!("claims: {e}")))?;
        Ok(Self { inner })
    }
}

/// Encode + sign claims as an HS256 JWT. Returns the standard
/// three-part base64url-encoded token: `header.payload.signature`.
///
/// # Errors
/// Only on the rare case where `secret.len() == 0` (HMAC accepts any
/// non-empty key); we surface that as [`JwtError::Decode`] for
/// uniformity rather than panicking.
pub fn encode(claims: &Claims, secret: &[u8]) -> Result<String, JwtError> {
    if secret.is_empty() {
        return Err(JwtError::Decode("HMAC secret must not be empty".into()));
    }
    let header = json!({"alg": "HS256", "typ": "JWT"});
    let header_b = URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&header).expect("header serialize"));
    let payload_b = URL_SAFE_NO_PAD.encode(claims.to_json());
    let signing_input = format!("{header_b}.{payload_b}");
    let sig = hmac_sha256(secret, signing_input.as_bytes());
    let sig_b = URL_SAFE_NO_PAD.encode(sig);
    Ok(format!("{signing_input}.{sig_b}"))
}

/// Decode + verify an HS256 JWT. Checks signature, `exp`, and `nbf`.
/// Does NOT check `iss` / `aud` — callers should validate those
/// against expected values from the returned claims.
///
/// # Errors
/// See [`JwtError`].
pub fn decode(token: &str, secret: &[u8]) -> Result<Claims, JwtError> {
    decode_at(token, secret, now_secs())
}

/// `decode` but with an explicit "current" time. Useful for tests +
/// for systems with clock skew tolerance baked in elsewhere.
///
/// # Errors
/// See [`JwtError`].
pub fn decode_at(token: &str, secret: &[u8], now: u64) -> Result<Claims, JwtError> {
    let mut it = token.split('.');
    let header_b = it.next().ok_or(JwtError::Malformed)?;
    let payload_b = it.next().ok_or(JwtError::Malformed)?;
    let sig_b = it.next().ok_or(JwtError::Malformed)?;
    if it.next().is_some() {
        return Err(JwtError::Malformed);
    }

    // Verify signature first (fail-fast before decoding).
    let signing_input = format!("{header_b}.{payload_b}");
    let expected = hmac_sha256(secret, signing_input.as_bytes());
    let provided = URL_SAFE_NO_PAD
        .decode(sig_b.as_bytes())
        .map_err(|_| JwtError::BadSignature)?;
    if expected.ct_eq(&provided).unwrap_u8() == 0 {
        return Err(JwtError::BadSignature);
    }

    // Decode header — sanity check the algorithm.
    let header_bytes = URL_SAFE_NO_PAD
        .decode(header_b.as_bytes())
        .map_err(|e| JwtError::Decode(format!("header b64: {e}")))?;
    let header: Value = serde_json::from_slice(&header_bytes)
        .map_err(|e| JwtError::Decode(format!("header json: {e}")))?;
    let alg = header.get("alg").and_then(Value::as_str).unwrap_or("");
    if alg != "HS256" {
        return Err(JwtError::UnsupportedAlg(alg.to_owned()));
    }

    // Decode claims + check temporal validity.
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b.as_bytes())
        .map_err(|e| JwtError::Decode(format!("payload b64: {e}")))?;
    let claims = Claims::from_json(&payload_bytes)?;

    if let Some(exp) = claims.get::<u64>("exp") {
        if now > exp {
            return Err(JwtError::Expired(exp));
        }
    }
    if let Some(nbf) = claims.get::<u64>("nbf") {
        if now < nbf {
            return Err(JwtError::NotYetValid(nbf));
        }
    }

    Ok(claims)
}

/// Decode WITHOUT verifying the signature or temporal claims. Useful
/// for inspecting a token to find which key id signed it (when you
/// rotate keys), then calling [`decode`] with the right secret.
///
/// **Never trust the result for authorization** — there's no integrity
/// guarantee.
///
/// # Errors
/// See [`JwtError`].
pub fn decode_unverified(token: &str) -> Result<Claims, JwtError> {
    let mut it = token.split('.');
    let _header = it.next().ok_or(JwtError::Malformed)?;
    let payload_b = it.next().ok_or(JwtError::Malformed)?;
    let _sig = it.next().ok_or(JwtError::Malformed)?;
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b.as_bytes())
        .map_err(|e| JwtError::Decode(format!("payload b64: {e}")))?;
    Claims::from_json(&payload_bytes)
}

// HMAC-SHA256 lives in [`crate::crypto`] — same shape, one
// implementation for hmac_auth + jwt + storage::s3 to share.
use crate::crypto::hmac_sha256;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"test-shared-secret-32-byte-string";

    #[test]
    fn round_trip_encode_decode() {
        let mut c = Claims::new("user-42");
        c.set("role", "admin");
        c.set("count", 7_i64);
        let token = encode(&c, SECRET).unwrap();
        let v = decode(&token, SECRET).unwrap();
        assert_eq!(v.subject(), Some("user-42"));
        assert_eq!(v.get::<String>("role").as_deref(), Some("admin"));
        assert_eq!(v.get::<i64>("count"), Some(7));
    }

    #[test]
    fn empty_secret_rejected() {
        let c = Claims::new("x");
        assert!(matches!(encode(&c, b""), Err(JwtError::Decode(_))));
    }

    #[test]
    fn token_format_is_three_segments() {
        let c = Claims::new("x");
        let t = encode(&c, SECRET).unwrap();
        assert_eq!(t.matches('.').count(), 2);
    }

    #[test]
    fn wrong_secret_fails_signature_check() {
        let c = Claims::new("x");
        let t = encode(&c, SECRET).unwrap();
        let err = decode(&t, b"wrong-secret-bytes").unwrap_err();
        assert!(matches!(err, JwtError::BadSignature));
    }

    #[test]
    fn payload_tampering_fails_signature_check() {
        let c = Claims::new("alice");
        let t = encode(&c, SECRET).unwrap();
        // Swap the payload segment for one that decodes to {"sub":"bob"}
        let parts: Vec<&str> = t.split('.').collect();
        let evil_payload = URL_SAFE_NO_PAD.encode(b"{\"sub\":\"bob\"}");
        let tampered = format!("{}.{}.{}", parts[0], evil_payload, parts[2]);
        assert!(matches!(decode(&tampered, SECRET), Err(JwtError::BadSignature)));
    }

    #[test]
    fn malformed_token_rejected() {
        assert!(matches!(decode("only.two", SECRET), Err(JwtError::Malformed)));
        assert!(matches!(decode("a.b.c.d", SECRET), Err(JwtError::Malformed)));
    }

    #[test]
    fn expired_token_rejected_at_decode_time() {
        let c = Claims::new("x").expires_at(now_secs() - 100);
        let t = encode(&c, SECRET).unwrap();
        let err = decode(&t, SECRET).unwrap_err();
        assert!(matches!(err, JwtError::Expired(_)));
    }

    #[test]
    fn ttl_helper_sets_iat_and_exp() {
        let c = Claims::new("x").ttl(Duration::from_secs(3600));
        assert!(c.get::<u64>("iat").is_some());
        let exp = c.get::<u64>("exp").unwrap();
        assert!(exp > now_secs(), "exp must be in the future");
    }

    #[test]
    fn not_before_rejected_when_future() {
        let c = Claims::new("x").not_before(now_secs() + 3600);
        let t = encode(&c, SECRET).unwrap();
        assert!(matches!(decode(&t, SECRET), Err(JwtError::NotYetValid(_))));
    }

    #[test]
    fn decode_at_specific_time_lets_us_test_clock_window() {
        let c = Claims::new("x").expires_at(1000);
        let t = encode(&c, SECRET).unwrap();
        // Decode AT t=500 — token still valid.
        let v = decode_at(&t, SECRET, 500).unwrap();
        assert_eq!(v.subject(), Some("x"));
        // Decode AT t=2000 — token expired.
        assert!(matches!(decode_at(&t, SECRET, 2000), Err(JwtError::Expired(1000))));
    }

    #[test]
    fn alg_other_than_hs256_rejected() {
        // Hand-build a token with alg=none (the classic JWT footgun).
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\",\"typ\":\"JWT\"}");
        let payload = URL_SAFE_NO_PAD.encode(b"{\"sub\":\"x\"}");
        let signing_input = format!("{header}.{payload}");
        // Sign it with the right secret so the signature WOULD check out
        // — and verify we still reject because of alg.
        let sig = URL_SAFE_NO_PAD
            .encode(hmac_sha256(SECRET, signing_input.as_bytes()));
        let token = format!("{signing_input}.{sig}");
        let err = decode(&token, SECRET).unwrap_err();
        assert!(matches!(err, JwtError::UnsupportedAlg(_)));
    }

    #[test]
    fn issuer_audience_jti_round_trip() {
        let c = Claims::new("x")
            .issuer("api.example.com")
            .audience("client.example.com")
            .jti("token-1");
        let t = encode(&c, SECRET).unwrap();
        let v = decode(&t, SECRET).unwrap();
        assert_eq!(v.get::<String>("iss").as_deref(), Some("api.example.com"));
        assert_eq!(v.get::<String>("aud").as_deref(), Some("client.example.com"));
        assert_eq!(v.get::<String>("jti").as_deref(), Some("token-1"));
    }

    #[test]
    fn decode_unverified_skips_signature_and_exp() {
        let c = Claims::new("x").expires_at(now_secs() - 100);
        let t = encode(&c, SECRET).unwrap();
        // decode() rejects (expired); decode_unverified() reads it.
        assert!(decode(&t, SECRET).is_err());
        let v = decode_unverified(&t).unwrap();
        assert_eq!(v.subject(), Some("x"));
    }

    #[test]
    fn empty_claims_round_trip_when_no_sub() {
        let c = Claims::empty();
        let t = encode(&c, SECRET).unwrap();
        let v = decode(&t, SECRET).unwrap();
        assert_eq!(v.subject(), None);
    }
}
