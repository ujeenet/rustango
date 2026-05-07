//! Tenant-facing login + session — analog of [`super::operator_console`]
//! but scoped to a single tenant's user pool.
//!
//! The tenant admin (built via [`crate::tenancy::admin::TenantAdminBuilder`])
//! opts in by calling `with_session(SessionSecret)`. With the opt-in:
//!
//! * `GET  /__login` / `POST /__login` — tenant login form, verified
//!   against `rustango_users` in the resolved tenant's storage.
//! * `POST /__logout` — clear cookie.
//! * `GET  /__static__/rustango.png` — embedded brand image.
//! * Every other path requires a valid session cookie. Anon traffic
//!   gets `303 → /__login?next=<sanitized-path>`.
//! * Authenticated **superusers** get full read/write admin.
//! * Authenticated **non-superusers** get a `read_only_all` admin —
//!   list/detail views render but every mutating route returns 403
//!   and write-buttons are hidden.
//!
//! ## Cookie shape
//!
//! Cookie name `rustango_tenant_session`. Payload + HMAC-SHA256
//! signature, same wire format as the operator console
//! (`<base64url(payload)>.<base64url(hmac)>`). Payload includes the
//! tenant `slug` so a cookie minted for `acme` can't authenticate at
//! `globex` even if the browser somehow forwards it.

use base64::Engine;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use super::operator_console::session::{sign, SessionError};
pub use super::operator_console::SessionSecret;

/// Default tenant cookie name. Distinct from the operator console's
/// `rustango_op_session` so the two never collide on a host that
/// serves both UIs.
pub const COOKIE_NAME: &str = "rustango_tenant_session";

/// Default lifetime — 7 days, matches the operator console.
pub const SESSION_TTL_SECS: i64 = 7 * 24 * 60 * 60;

/// Embedded brand image, served at `/__static__/rustango.png`.
pub(crate) const RUSTANGO_PNG: &[u8] = include_bytes!("static/rustango.png");

/// Principal payload carried inside the cookie. Compact field names
/// (`uid`, `slug`, `exp`) keep the cookie short.
///
/// The `slug` field binds the session to a single tenant — the
/// middleware rejects with [`SessionError::WrongTenant`] if the
/// resolved org's slug doesn't match.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TenantSessionPayload {
    /// `rustango_users.id` in the tenant's storage.
    pub uid: i64,
    /// Tenant slug the cookie was minted for.
    pub slug: String,
    /// Expiry as Unix seconds.
    pub exp: i64,
}

impl TenantSessionPayload {
    #[must_use]
    pub fn new(user_id: i64, slug: impl Into<String>, ttl_secs: i64) -> Self {
        let exp = chrono::Utc::now().timestamp() + ttl_secs;
        Self {
            uid: user_id,
            slug: slug.into(),
            exp,
        }
    }

    fn is_expired(&self) -> bool {
        chrono::Utc::now().timestamp() >= self.exp
    }
}

/// Serialize and sign a tenant session payload.
#[must_use]
pub fn encode(secret: &SessionSecret, payload: &TenantSessionPayload) -> String {
    let json = serde_json::to_vec(payload).expect("payload serializes");
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
    let sig = sign(secret, payload_b64.as_bytes());
    let sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig);
    format!("{payload_b64}.{sig_b64}")
}

/// Verify, deserialize, and tenant-bind-check a cookie value.
///
/// `expected_slug` is the slug of the resolved tenant — the cookie's
/// `slug` field must match (defense against a cookie minted for one
/// tenant being replayed against another).
///
/// # Errors
/// * [`SessionError::Malformed`] — bad split, non-base64, bad JSON.
/// * [`SessionError::BadSignature`] — HMAC mismatch.
/// * [`SessionError::Expired`] — `exp` in the past.
/// * [`SessionError::WrongTenant`] — `payload.slug != expected_slug`.
pub fn decode(
    secret: &SessionSecret,
    expected_slug: &str,
    value: &str,
) -> Result<TenantSessionPayload, SessionError> {
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
    let payload: TenantSessionPayload =
        serde_json::from_slice(&payload_bytes).map_err(|_| SessionError::Malformed)?;
    if payload.is_expired() {
        return Err(SessionError::Expired);
    }
    if payload.slug != expected_slug {
        return Err(SessionError::WrongTenant);
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SessionSecret {
        SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec())
    }

    #[test]
    fn round_trip_valid_payload() {
        let secret = key();
        let payload = TenantSessionPayload::new(7, "acme", 3600);
        let cookie = encode(&secret, &payload);
        let back = decode(&secret, "acme", &cookie).unwrap();
        assert_eq!(back, payload);
    }

    #[test]
    fn rejects_cookie_minted_for_a_different_tenant() {
        let secret = key();
        let payload = TenantSessionPayload::new(7, "acme", 3600);
        let cookie = encode(&secret, &payload);
        let err = decode(&secret, "globex", &cookie).unwrap_err();
        assert!(matches!(err, SessionError::WrongTenant));
    }

    #[test]
    fn rejects_tampered_payload() {
        let secret = key();
        let payload = TenantSessionPayload::new(7, "acme", 3600);
        let cookie = encode(&secret, &payload);
        let (_, sig) = cookie.split_once('.').unwrap();
        let evil = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"uid":999,"slug":"acme","exp":9999999999}"#);
        let tampered = format!("{evil}.{sig}");
        let err = decode(&secret, "acme", &tampered).unwrap_err();
        assert!(matches!(err, SessionError::BadSignature));
    }

    #[test]
    fn rejects_wrong_secret() {
        let s1 = SessionSecret::from_bytes(b"first-test-secret-thirty-2-bytes".to_vec());
        let s2 = SessionSecret::from_bytes(b"second-test-secret-thirty2-bytes".to_vec());
        let cookie = encode(&s1, &TenantSessionPayload::new(1, "acme", 3600));
        let err = decode(&s2, "acme", &cookie).unwrap_err();
        assert!(matches!(err, SessionError::BadSignature));
    }

    #[test]
    fn rejects_expired() {
        let secret = key();
        let cookie = encode(&secret, &TenantSessionPayload::new(1, "acme", -10));
        let err = decode(&secret, "acme", &cookie).unwrap_err();
        assert!(matches!(err, SessionError::Expired));
    }
}
