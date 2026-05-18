//! HMAC-SHA256 signed session cookies for the operator console.
//!
//! Stateless — the cookie carries the principal (`operator_id` + `exp`)
//! and a signature. Server validates the signature on every request;
//! no server-side session table for v1. Trade-offs:
//!
//! * **No revocation** — once issued, a cookie is valid until `exp`.
//!   Operator deletion / password change doesn't invalidate live
//!   cookies. Acceptable for v1; v2 can add a short-lived cookie +
//!   server-side revocation list.
//! * **Secret rotation invalidates all cookies** — a server restart
//!   with auto-generated secret signs everyone out. With
//!   `RUSTANGO_SESSION_SECRET` set in env, sessions survive restarts.
//!
//! Cookie format:
//!
//! ```text
//! Cookie: rustango_op_session=<base64(payload)>.<base64(hmac_sha256)>
//! payload  = JSON {"oid": <i64>, "exp": <unix_seconds>}
//! signature = HMAC-SHA256(secret, payload_base64) [first 32 bytes]
//! ```

use base64::Engine;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

// v0.45 (#253) — `SessionSecret` + `sign` were promoted to the
// crate-root `crate::session` module so the bare `admin` module can
// reuse the same HMAC primitives without compiling in `tenancy`.
// The re-exports below keep every pre-existing `crate::tenancy::session::SessionSecret`
// caller working.
pub use crate::session::{SessionSecret, SessionSecretError};
// `sign` re-exported at crate-internal visibility so existing
// callers (`tenant_console`, `impersonation_handoff`) keep working
// after the v0.45 move to `crate::session`.
pub(crate) use crate::session::sign;

/// Default cookie name. Visible in browser devtools — namespaced so
/// it doesn't collide with tenant cookies.
pub const COOKIE_NAME: &str = "rustango_op_session";

/// Default session lifetime (7 days). Configurable later; locked
/// for v1.
pub const SESSION_TTL_SECS: i64 = 7 * 24 * 60 * 60;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("session cookie is malformed")]
    Malformed,
    #[error("session signature mismatch")]
    BadSignature,
    #[error("session expired")]
    Expired,
    /// Cookie's tenant slug doesn't match the resolved tenant — used
    /// by the tenant console to defend against cross-tenant cookie
    /// replay (cookie issued for `acme` should not authenticate at
    /// `globex`'s subdomain).
    #[error("session is bound to a different tenant")]
    WrongTenant,
}

/// Principal payload carried inside the cookie. Compact field names
/// to keep the cookie short (browsers truncate aggressively).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionPayload {
    /// Operator id (from `rustango_operators.id`).
    pub oid: i64,
    /// Expiry as Unix seconds.
    pub exp: i64,
    /// Issued-at as Unix seconds (v0.28.4, #77 follow-up).
    /// Compared to `rustango_operators.password_changed_at` —
    /// sessions issued before the latest password rotation are
    /// rejected. `0` for cookies minted on pre-0.28.4 servers
    /// (`#[serde(default)]` keeps them parseable; the comparison
    /// treats `0` as "issued at the dawn of time" so they're
    /// invalidated by any password change).
    #[serde(default)]
    pub iat: i64,
}

impl SessionPayload {
    #[must_use]
    pub fn new(operator_id: i64, ttl_secs: i64) -> Self {
        let now = chrono::Utc::now().timestamp();
        Self {
            oid: operator_id,
            exp: now + ttl_secs,
            iat: now,
        }
    }

    fn is_expired(&self) -> bool {
        chrono::Utc::now().timestamp() >= self.exp
    }
}

/// Serialize and sign a payload into a cookie value.
#[must_use]
pub fn encode(secret: &SessionSecret, payload: &SessionPayload) -> String {
    let json = serde_json::to_vec(payload).expect("payload serializes");
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
    let sig = sign(secret, payload_b64.as_bytes());
    let sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig);
    format!("{payload_b64}.{sig_b64}")
}

/// Verify and deserialize a cookie value.
///
/// # Errors
/// Returns [`SessionError::Malformed`] for missing/bad-base64 parts,
/// [`SessionError::BadSignature`] when HMAC doesn't match (covers
/// secret rotation + tampering), [`SessionError::Expired`] when the
/// payload's `exp` is in the past.
pub fn decode(secret: &SessionSecret, value: &str) -> Result<SessionPayload, SessionError> {
    let (payload_b64, sig_b64) = value.split_once('.').ok_or(SessionError::Malformed)?;
    let expected = sign(secret, payload_b64.as_bytes());
    let provided = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| SessionError::Malformed)?;
    if expected.ct_eq(&provided[..]).unwrap_u8() == 0 {
        return Err(SessionError::BadSignature);
    }
    let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| SessionError::Malformed)?;
    let payload: SessionPayload =
        serde_json::from_slice(&payload_bytes).map_err(|_| SessionError::Malformed)?;
    if payload.is_expired() {
        return Err(SessionError::Expired);
    }
    Ok(payload)
}

// `sign` moved to `crate::session` in v0.45 (#253) — `use` above.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_valid_payload() {
        let secret = SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec());
        let payload = SessionPayload::new(42, 3600);
        let cookie = encode(&secret, &payload);
        let back = decode(&secret, &cookie).unwrap();
        assert_eq!(back, payload);
    }

    #[test]
    fn rejects_tampered_payload() {
        let secret = SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec());
        let payload = SessionPayload::new(42, 3600);
        let cookie = encode(&secret, &payload);
        // Replace payload but keep signature → BadSignature.
        let (_, sig) = cookie.split_once('.').unwrap();
        let evil_payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"oid":999,"exp":9999999999}"#);
        let tampered = format!("{evil_payload}.{sig}");
        let err = decode(&secret, &tampered).unwrap_err();
        assert!(matches!(err, SessionError::BadSignature));
    }

    #[test]
    fn rejects_wrong_secret() {
        let s1 = SessionSecret::from_bytes(b"first-test-secret-thirty-2-bytes".to_vec());
        let s2 = SessionSecret::from_bytes(b"second-test-secret-thirty2-bytes".to_vec());
        let cookie = encode(&s1, &SessionPayload::new(1, 3600));
        let err = decode(&s2, &cookie).unwrap_err();
        assert!(matches!(err, SessionError::BadSignature));
    }

    #[test]
    fn rejects_expired() {
        let secret = SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec());
        let payload = SessionPayload::new(1, -10);
        let cookie = encode(&secret, &payload);
        let err = decode(&secret, &cookie).unwrap_err();
        assert!(matches!(err, SessionError::Expired));
    }

    #[test]
    fn rejects_malformed_no_dot() {
        let secret = SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec());
        let err = decode(&secret, "not-a-cookie").unwrap_err();
        assert!(matches!(err, SessionError::Malformed));
    }
}
