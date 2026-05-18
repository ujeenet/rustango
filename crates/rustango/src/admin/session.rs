//! Signed-cookie session auth for the bare `admin` module.
//!
//! Issue #253. Gives non-tenancy projects the Django-shape admin
//! login UX (`/login` form, signed cookie, sidebar `Logout`) without
//! pulling in the tenancy stack.
//!
//! ## Reuse with `tenancy::session`
//!
//! The HMAC signing primitive ([`crate::session::SessionSecret`] +
//! [`crate::session::sign`]) lives at the crate root and is shared
//! with `tenancy::session`. Both modules layer their own payload
//! shape on top — tenancy carries `(operator_id, exp, iat)`, admin
//! carries `(user_id, is_superuser, exp)` — so the cookies are
//! distinct (different name AND different shape) but the crypto is
//! identical and lives in exactly one place.

use base64::Engine;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::session::sign;
pub use crate::session::SessionSecret as AdminSessionSecret;

tokio::task_local! {
    /// Per-request session set by the `require_session` middleware
    /// so deep-stack helpers (chrome context, audit emit, …) can
    /// read the current user without every handler threading
    /// `Option<Extension<AdminSession>>` through its arg list.
    /// Cleared by tokio when the scoped future completes.
    pub(crate) static CURRENT_SESSION: AdminSession;
}

/// Read the current request's session, if the middleware installed
/// one. Returns `None` outside an admin request (the task-local was
/// never scoped) or when the request is unauthenticated.
#[must_use]
pub fn current() -> Option<AdminSession> {
    CURRENT_SESSION.try_with(|s| s.clone()).ok()
}

/// Default session TTL — 8 hours. Operators get re-prompted once a
/// workday. Future slice exposes this as a knob on
/// [`crate::admin::Builder`].
const DEFAULT_TTL_SECS: i64 = 8 * 60 * 60;

/// Cookie name the admin session is stored under. Distinct from any
/// `tenancy` cookies so the two layers can coexist on one host.
pub(crate) const SESSION_COOKIE: &str = "rustango_admin_session";

/// The user-facing handle the login middleware drops into the
/// request extension on every authenticated request. Use as an
/// extractor on admin-side handlers if you need to know who's
/// signed in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminSession {
    /// Primary key of the [`AdminUser`](super::user::AdminUser) row.
    pub user_id: i64,
    /// Username of the logged-in admin, cached on the cookie so the
    /// chrome can render "Signed in as <username>" without a DB hit
    /// per request. Added in slice B (#253).
    pub username: String,
    /// `true` when the user's `is_superuser` flag was set at
    /// login time. Cached on the cookie so the visibility check
    /// in the chrome doesn't need a DB hit per request.
    pub is_superuser: bool,
}

/// Wire payload — sent through `sign` + base64. Wraps [`AdminSession`]
/// with an `exp` timestamp so expired cookies fail closed.
#[derive(Serialize, Deserialize)]
struct CookiePayload {
    user_id: i64,
    username: String,
    is_superuser: bool,
    /// Unix timestamp the session expires at.
    exp: i64,
}

impl CookiePayload {
    fn is_expired(&self) -> bool {
        chrono::Utc::now().timestamp() >= self.exp
    }
}

/// Sign a fresh session — returns the cookie value to set on the
/// response. TTL defaults to 8 hours.
#[must_use]
pub(crate) fn encode(secret: &AdminSessionSecret, session: AdminSession) -> String {
    let payload = CookiePayload {
        user_id: session.user_id,
        username: session.username,
        is_superuser: session.is_superuser,
        exp: chrono::Utc::now().timestamp() + DEFAULT_TTL_SECS,
    };
    let json = serde_json::to_vec(&payload).expect("payload serializes");
    let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&json);
    let sig = sign(secret, body.as_bytes());
    let sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig);
    format!("{body}.{sig_b64}")
}

/// Verify + decode a cookie value. Returns `Some(session)` only when
/// the signature is valid AND the payload hasn't expired. Any other
/// failure (malformed, tampered signature, expired) maps to `None`
/// so the caller treats the request as unauthenticated.
#[must_use]
pub(crate) fn decode(secret: &AdminSessionSecret, value: &str) -> Option<AdminSession> {
    let (body, sig_b64) = value.split_once('.')?;
    let expected = sign(secret, body.as_bytes());
    let provided = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(sig_b64)
        .ok()?;
    // Constant-time comparison — same primitive `tenancy::session`
    // uses. Defends against timing-channel cookie forgery probes.
    if expected.ct_eq(&provided[..]).unwrap_u8() == 0 {
        return None;
    }
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body)
        .ok()?;
    let payload: CookiePayload = serde_json::from_slice(&json).ok()?;
    if payload.is_expired() {
        return None;
    }
    Some(AdminSession {
        user_id: payload.user_id,
        username: payload.username,
        is_superuser: payload.is_superuser,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_recovers_session_fields() {
        let secret = AdminSessionSecret::from_bytes(vec![42u8; 32]);
        let cookie = encode(
            &secret,
            AdminSession {
                user_id: 7,
                username: "alice".into(),
                is_superuser: true,
            },
        );
        let session = decode(&secret, &cookie).expect("valid cookie verifies");
        assert_eq!(session.user_id, 7);
        assert!(session.is_superuser);
    }

    #[test]
    fn tampered_signature_rejected() {
        let secret = AdminSessionSecret::from_bytes(vec![1u8; 32]);
        let cookie = encode(
            &secret,
            AdminSession {
                user_id: 1,
                username: "bob".into(),
                is_superuser: false,
            },
        );
        let (body, _sig) = cookie.split_once('.').unwrap();
        let bad = format!("{body}.AAAA");
        assert!(decode(&secret, &bad).is_none());
    }

    #[test]
    fn wrong_secret_rejected() {
        let secret_a = AdminSessionSecret::from_bytes(vec![1u8; 32]);
        let secret_b = AdminSessionSecret::from_bytes(vec![2u8; 32]);
        let cookie = encode(
            &secret_a,
            AdminSession {
                user_id: 1,
                username: "bob".into(),
                is_superuser: false,
            },
        );
        assert!(decode(&secret_b, &cookie).is_none());
    }
}
