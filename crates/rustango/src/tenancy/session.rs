//! HMAC-SHA256 signed session cookies for the operator console.
//!
//! Stateless — the cookie carries the principal (`operator_id` + `exp`)
//! and a signature. Server validates the signature on every request;
//! no server-side session table for v1. Trade-offs:
//!
//! * **Idle and absolute expiry** — `exp` is the idle deadline, slid by
//!   each request; `iat` + [`SESSION_TTL_SECS`] is the hard cap.
//! * **No revocation list** — a cookie is valid until `exp`, unless the
//!   operator is deactivated, changes password or logs out. Logout stamps
//!   `sessions_revoked_at`, which ends the operator's sessions on every device.
//! * **Secret rotation invalidates all cookies** — a server restart
//!   with auto-generated secret signs everyone out. With
//!   `RUSTANGO_SESSION_SECRET` set in env, sessions survive restarts.
//!
//! Cookie format:
//!
//! ```text
//! Cookie: rustango_op_session=<base64(payload)>.<base64(hmac_sha256)>
//! payload  = JSON {"oid": <i64>, "exp": <unix_seconds>, "iat": .., "pwf": ..}
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
pub use crate::session::{PasswordFingerprint, SessionSecret, SessionSecretError};
// `sign` re-exported at crate-internal visibility so existing
// callers (`tenant_console`, `impersonation_handoff`) keep working
// after the v0.45 move to `crate::session`.
pub(crate) use crate::session::{session_survives, sign};

/// Default cookie name. Visible in browser devtools — namespaced so
/// it doesn't collide with tenant cookies.
pub const COOKIE_NAME: &str = "rustango_op_session";

/// Longest a console session lasts from sign-in, however active.
pub const SESSION_TTL_SECS: i64 = 12 * 60 * 60;

/// A console session unused this long ends; each request slides `exp`.
pub const SESSION_IDLE_SECS: i64 = 30 * 60;

/// How stale `exp` must be before a request re-issues the cookie.
const RENEW_AFTER_SECS: i64 = 60;

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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionPayload {
    /// Operator id (from `rustango_operators.id`).
    pub oid: i64,
    /// Expiry as Unix seconds.
    pub exp: i64,
    /// Issued-at as Unix seconds. A session issued before
    /// `rustango_operators.password_changed_at` is rejected. `0` for
    /// cookies minted on pre-0.28.4 servers.
    #[serde(default)]
    pub iat: i64,
    /// Fingerprint of the operator's `password_hash` at login. Any
    /// password change makes it stop matching (#1338).
    #[serde(default)]
    pub pwf: PasswordFingerprint,
}

impl SessionPayload {
    /// `pwf` is the [`PasswordFingerprint`] of the operator's current hash.
    #[must_use]
    pub fn new(operator_id: i64, ttl_secs: i64, pwf: PasswordFingerprint) -> Self {
        let now = chrono::Utc::now().timestamp();
        Self {
            oid: operator_id,
            exp: now + ttl_secs,
            iat: now,
            pwf,
        }
    }

    /// Past the idle deadline `exp`, or [`SESSION_TTL_SECS`] after sign-in.
    fn is_expired(&self) -> bool {
        let now = chrono::Utc::now().timestamp();
        now >= self.exp || now >= self.iat.saturating_add(SESSION_TTL_SECS)
    }

    /// This session with its idle deadline moved to now + [`SESSION_IDLE_SECS`],
    /// capped at the absolute lifetime. `None` while `exp` is still fresh.
    #[must_use]
    pub fn renewed(&self) -> Option<Self> {
        let now = chrono::Utc::now().timestamp();
        let exp = (now + SESSION_IDLE_SECS).min(self.iat.saturating_add(SESSION_TTL_SECS));
        // An older cookie's far `exp` is pulled in, too.
        (exp < self.exp || exp - self.exp >= RENEW_AFTER_SECS).then(|| Self {
            exp,
            ..self.clone()
        })
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

    fn fp() -> PasswordFingerprint {
        PasswordFingerprint::default()
    }

    #[test]
    fn round_trip_valid_payload() {
        let secret = SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec());
        let payload = SessionPayload::new(42, 3600, PasswordFingerprint::of(&secret, "$h"));
        let cookie = encode(&secret, &payload);
        let back = decode(&secret, &cookie).unwrap();
        assert_eq!(back, payload);
    }

    #[test]
    fn rejects_tampered_payload() {
        let secret = SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec());
        let payload = SessionPayload::new(42, 3600, fp());
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
        let cookie = encode(&s1, &SessionPayload::new(1, 3600, fp()));
        let err = decode(&s2, &cookie).unwrap_err();
        assert!(matches!(err, SessionError::BadSignature));
    }

    #[test]
    fn rejects_expired() {
        let secret = SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec());
        let payload = SessionPayload::new(1, -10, fp());
        let cookie = encode(&secret, &payload);
        let err = decode(&secret, &cookie).unwrap_err();
        assert!(matches!(err, SessionError::Expired));
    }

    /// Renewal slides `exp` to the idle window, never past the cap.
    #[test]
    fn renewal_slides_exp_up_to_the_cap() {
        let now = chrono::Utc::now().timestamp();
        let renewed = SessionPayload::new(1, 30, fp()).renewed().expect("stale");
        assert!(renewed.exp >= now + SESSION_IDLE_SECS);
        assert!(renewed.renewed().is_none(), "fresh needs no new cookie");

        let mut near_cap = SessionPayload::new(1, 30, fp());
        near_cap.iat = now - SESSION_TTL_SECS + 10;
        assert!(near_cap.renewed().unwrap().exp <= now + 10);

        // An older week-long cookie is cut to the idle limit.
        let old = SessionPayload::new(1, 7 * 24 * 3600, fp());
        let cut = old.renewed().expect("a far exp is pulled in");
        assert!(cut.exp <= chrono::Utc::now().timestamp() + SESSION_IDLE_SECS);
    }

    /// A session older than the absolute cap is refused, whatever its `exp`.
    #[test]
    fn rejects_a_session_past_the_absolute_cap() {
        let secret = SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec());
        let mut payload = SessionPayload::new(1, 3600, fp());
        payload.iat -= SESSION_TTL_SECS;
        let err = decode(&secret, &encode(&secret, &payload)).unwrap_err();
        assert!(matches!(err, SessionError::Expired));
    }

    #[test]
    fn rejects_malformed_no_dot() {
        let secret = SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec());
        let err = decode(&secret, "not-a-cookie").unwrap_err();
        assert!(matches!(err, SessionError::Malformed));
    }

    /// A new hash ends the session even when `iat` equals the change second.
    #[test]
    fn a_new_password_hash_ends_a_same_second_session() {
        let secret = SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec());
        let now = chrono::Utc::now();
        let pwf = PasswordFingerprint::of(&secret, "$old");
        assert!(session_survives(
            &secret,
            &pwf,
            now.timestamp(),
            "$old",
            Some(now),
            None
        ));
        assert!(!session_survives(
            &secret,
            &pwf,
            now.timestamp(),
            "$new",
            Some(now),
            None
        ));
        // A cookie from before this field decodes empty and never matches.
        assert!(!session_survives(
            &secret,
            &fp(),
            now.timestamp(),
            "$old",
            None,
            None
        ));
    }

    /// With the hash unchanged, a `password_changed_at` bump after `iat` still ends it.
    #[test]
    fn a_later_password_changed_at_ends_a_session_with_the_same_hash() {
        let secret = SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec());
        let pwf = PasswordFingerprint::of(&secret, "$h");
        let iat = 1_700_000_000;
        let at = |secs| chrono::DateTime::from_timestamp(secs, 0);
        assert!(session_survives(&secret, &pwf, iat, "$h", None, None));
        assert!(session_survives(&secret, &pwf, iat, "$h", at(iat), None));
        assert!(!session_survives(
            &secret,
            &pwf,
            iat,
            "$h",
            at(iat + 1),
            None
        ));
    }
}
