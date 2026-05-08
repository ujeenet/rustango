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

/// Default lifetime for an *impersonation* session (operator
/// "Open admin as superuser →"). 1 hour by default — short
/// enough that an idle operator gets dropped before they can
/// forget they're impersonating, long enough for a debugging
/// session. Override via `RUSTANGO_OPERATOR_IMPERSONATION_TTL_SECS`.
/// (#78, v0.27.8)
pub const IMPERSONATION_TTL_SECS: i64 = 60 * 60;

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
    /// `rustango_users.id` in the tenant's storage. For
    /// **impersonation** cookies (#78), this is `0` — the
    /// operator is acting as a synthetic principal that doesn't
    /// correspond to any real tenant user. The middleware
    /// branches on `imp.is_some()` rather than `uid`.
    pub uid: i64,
    /// Tenant slug the cookie was minted for.
    pub slug: String,
    /// Expiry as Unix seconds.
    pub exp: i64,
    /// `Some(operator_id)` when this cookie was minted by the
    /// operator console's "Open admin as superuser →" flow
    /// (#78). The tenant admin treats it as superuser, renders
    /// an impersonation banner, and tags audit-log entries with
    /// `source = "operator:<id>:impersonating"`. `None` for
    /// regular tenant-user logins.
    ///
    /// `#[serde(default)]` keeps cookies minted on pre-0.27.8
    /// servers parseable when the user upgrades.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imp: Option<i64>,
    /// Issued-at as Unix seconds (v0.28.4, #77 follow-up). The
    /// session middleware compares this to `rustango_users.password_changed_at`
    /// — sessions issued before the latest password rotation are
    /// rejected. `0` for cookies minted on pre-0.28.4 servers
    /// (`#[serde(default)]` keeps them parseable; the comparison
    /// treats `0` as "issued at the dawn of time" so they're
    /// invalidated by any password change).
    #[serde(default)]
    pub iat: i64,
}

impl TenantSessionPayload {
    #[must_use]
    pub fn new(user_id: i64, slug: impl Into<String>, ttl_secs: i64) -> Self {
        let now = chrono::Utc::now().timestamp();
        Self {
            uid: user_id,
            slug: slug.into(),
            exp: now + ttl_secs,
            imp: None,
            iat: now,
        }
    }

    /// Mint an impersonation payload — operator-as-superuser
    /// for the named tenant. Distinct from [`Self::new`] so
    /// the call site reads as intent. (#78)
    #[must_use]
    pub fn impersonation(operator_id: i64, slug: impl Into<String>, ttl_secs: i64) -> Self {
        let now = chrono::Utc::now().timestamp();
        Self {
            uid: 0,
            slug: slug.into(),
            exp: now + ttl_secs,
            imp: Some(operator_id),
            iat: now,
        }
    }

    fn is_expired(&self) -> bool {
        chrono::Utc::now().timestamp() >= self.exp
    }

    /// `true` for impersonation payloads (operator opened the
    /// tenant admin from the operator console). The middleware
    /// uses this to switch on the auth path. (#78)
    #[must_use]
    pub fn is_impersonation(&self) -> bool {
        self.imp.is_some()
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

    // v0.27.8 (#78) — impersonation cookie shape regression
    // tests. Pin the wire format so an upgrade can't silently
    // change `imp` semantics.

    #[test]
    fn impersonation_payload_has_imp_set() {
        let p = TenantSessionPayload::impersonation(42, "acme", 3600);
        assert_eq!(p.uid, 0);
        assert_eq!(p.slug, "acme");
        assert_eq!(p.imp, Some(42));
        assert!(p.is_impersonation());
    }

    #[test]
    fn regular_payload_has_no_imp_field() {
        let p = TenantSessionPayload::new(7, "acme", 3600);
        assert_eq!(p.imp, None);
        assert!(!p.is_impersonation());
    }

    #[test]
    fn impersonation_round_trip_through_cookie() {
        let secret = key();
        let p = TenantSessionPayload::impersonation(99, "acme", 3600);
        let cookie = encode(&secret, &p);
        let back = decode(&secret, "acme", &cookie).unwrap();
        assert_eq!(back, p);
        assert_eq!(back.imp, Some(99));
    }

    #[test]
    fn impersonation_cookie_still_pinned_to_slug() {
        // Cross-tenant replay must fail even for impersonation
        // cookies — operator opening tenant A shouldn't have a
        // cookie that authenticates them at tenant B.
        let secret = key();
        let p = TenantSessionPayload::impersonation(99, "acme", 3600);
        let cookie = encode(&secret, &p);
        let err = decode(&secret, "globex", &cookie).unwrap_err();
        assert!(matches!(err, SessionError::WrongTenant));
    }

    #[test]
    fn pre_0_27_8_cookie_without_imp_still_decodes() {
        // Old cookie format omits the `imp` field. `serde_default`
        // must fill in `None` so a server upgrade doesn't sign
        // every operator out.
        let secret = key();
        let json = br#"{"uid":7,"slug":"acme","exp":99999999999}"#;
        let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
        let sig = sign(&secret, payload_b64.as_bytes());
        let sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig);
        let cookie = format!("{payload_b64}.{sig_b64}");
        let back = decode(&secret, "acme", &cookie).unwrap();
        assert_eq!(back.uid, 7);
        assert_eq!(back.imp, None);
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

    /// v0.28.4 (#77) — `iat` is set on every newly-minted payload so
    /// the session middleware can compare it against
    /// `rustango_users.password_changed_at`.
    #[test]
    fn new_payload_stamps_iat_at_construction_time() {
        let p = TenantSessionPayload::new(7, "acme", 3600);
        let now = chrono::Utc::now().timestamp();
        // iat lands within ±2s of "now" — wide enough to absorb
        // schedule jitter, tight enough to fail if we forgot to
        // populate it.
        assert!((p.iat - now).abs() < 2, "iat = {} but now = {now}", p.iat);
        assert!(p.iat > 0);
        assert_eq!(p.exp - p.iat, 3600);
    }

    /// Pre-0.28.4 cookies don't carry `iat`. `#[serde(default)]` on
    /// the field keeps them parseable; `iat` decodes as `0`. The
    /// session middleware treats `0 < password_changed_at.timestamp()`
    /// as "issued at the dawn of time, invalidate", so any
    /// post-rotation login wins. This test pins the parse
    /// behavior — losing it would silently break the security
    /// guarantee on upgrade.
    #[test]
    fn legacy_pre_v0_28_4_cookie_decodes_with_iat_zero() {
        // Hand-write a payload JSON without `iat` to simulate a
        // pre-0.28.4 cookie.
        use base64::Engine;
        let secret = key();
        let json = br#"{"uid":7,"slug":"acme","exp":99999999999}"#;
        let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
        let sig = sign(&secret, payload_b64.as_bytes());
        let sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig);
        let cookie = format!("{payload_b64}.{sig_b64}");
        let p = decode(&secret, "acme", &cookie).expect("legacy cookie still parses");
        assert_eq!(p.uid, 7);
        assert_eq!(p.iat, 0, "missing iat should default to 0");
    }
}
