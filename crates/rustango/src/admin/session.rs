//! Signed-cookie session auth for the bare `admin` module: a `/login`
//! form, a signed cookie and a sidebar `Logout`, without the tenancy
//! stack.
//!
//! The HMAC primitive ([`crate::session::SessionSecret`] and
//! [`crate::session::sign`]) lives at the crate root and is shared with
//! `tenancy::session`. Each module adds its own payload on top, so the
//! two cookies differ in name and shape while the crypto stays in one
//! place.

use base64::Engine;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::session::sign;
pub use crate::session::SessionSecret as AdminSessionSecret;

tokio::task_local! {
    /// Per-request session, set by the `require_session` middleware, so
    /// deep-stack helpers such as the chrome context can read the
    /// current user without every handler passing it down. Tokio clears
    /// it when the scoped future finishes.
    pub(crate) static CURRENT_SESSION: AdminSession;
}

/// The current request's session, if the middleware installed one.
/// `None` outside an admin request, or when the request is
/// unauthenticated.
#[must_use]
pub fn current() -> Option<AdminSession> {
    CURRENT_SESSION.try_with(|s| s.clone()).ok()
}

tokio::task_local! {
    /// Per-request CSRF token, set by `csrf_context`.
    ///
    /// Same reasoning as `CURRENT_SESSION`: `chrome_context` builds the
    /// variables for every admin template and is called from many
    /// render sites. A task-local avoids threading one request-scoped
    /// value through all of them.
    pub(crate) static CURRENT_CSRF_TOKEN: String;
}

/// The current request's CSRF token, if the admin middleware installed
/// one.
///
/// `None` outside an admin request, so `chrome_context` treats a
/// missing token as normal: hand-rendered pages and tests run with no
/// request in scope. This does not weaken enforcement. `CsrfLayer`
/// still rejects an unsafe request whatever the template rendered.
#[must_use]
pub fn current_csrf_token() -> Option<String> {
    CURRENT_CSRF_TOKEN.try_with(Clone::clone).ok()
}

/// Session lifetime: 8 hours, so an operator signs in once a workday.
const DEFAULT_TTL_SECS: i64 = 8 * 60 * 60;

/// Cookie name the admin session is stored under. Distinct from any
/// `tenancy` cookies so the two layers can coexist on one host.
pub(crate) const SESSION_COOKIE: &str = "rustango_admin_session";

/// The session the login middleware puts in the request extensions on
/// every authenticated request. Use it as an extractor in an admin
/// handler to see who is signed in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminSession {
    /// Primary key of the [`AdminUser`](super::user::AdminUser) row.
    pub user_id: i64,
    /// Username, cached on the cookie so the chrome can render
    /// "Signed in as …" without a query per request.
    pub username: String,
    /// The user's `is_superuser` flag at login time, cached on the
    /// cookie so the chrome's visibility check needs no query.
    pub is_superuser: bool,
}

/// Wire payload: signed, then base64-encoded. Wraps [`AdminSession`]
/// with an `exp` timestamp so an expired cookie fails closed.
#[derive(Serialize, Deserialize)]
struct CookiePayload {
    user_id: i64,
    username: String,
    is_superuser: bool,
    /// Unix timestamp the session expires at.
    exp: i64,
    /// Fingerprint of the user's `password_hash` at login. The gate
    /// recomputes it from the current hash on every request, so a
    /// password change or reset invalidates every cookie minted before
    /// it. `#[serde(default)]` lets older cookies decode: they carry
    /// `""`, which never matches, so they need one fresh login.
    #[serde(default)]
    auth_hash: String,
}

impl CookiePayload {
    fn is_expired(&self) -> bool {
        chrono::Utc::now().timestamp() >= self.exp
    }
}

/// Fingerprint of a user's `password_hash`, bound to the signing
/// secret. Stored in the cookie at login and recomputed each request.
/// It changes with the password, so old sessions stop validating. It
/// cannot be reversed back to the hash.
#[must_use]
pub(crate) fn password_fingerprint(secret: &AdminSessionSecret, password_hash: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sign(secret, password_hash.as_bytes()))
}

/// Sign a fresh session and return the cookie value to set. Lasts 8
/// hours. `auth_hash` is the [`password_fingerprint`] of the user's
/// current `password_hash`.
#[must_use]
pub(crate) fn encode(
    secret: &AdminSessionSecret,
    session: AdminSession,
    auth_hash: &str,
) -> String {
    let payload = CookiePayload {
        user_id: session.user_id,
        username: session.username,
        is_superuser: session.is_superuser,
        exp: chrono::Utc::now().timestamp() + DEFAULT_TTL_SECS,
        auth_hash: auth_hash.to_owned(),
    };
    let json = serde_json::to_vec(&payload).expect("payload serializes");
    let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&json);
    let sig = sign(secret, body.as_bytes());
    let sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig);
    format!("{body}.{sig_b64}")
}

/// Verify and decode a cookie value. Returns `Some(session)` only when
/// the signature is valid **and** the payload has not expired. Every
/// other case, such as a malformed or tampered cookie, returns `None`,
/// and the caller must treat the request as unauthenticated.
#[must_use]
pub(crate) fn decode(secret: &AdminSessionSecret, value: &str) -> Option<AdminSession> {
    decode_full(secret, value).map(|(session, _auth_hash)| session)
}

/// Like [`decode`], but also returns the cookie's stored password
/// fingerprint, so the gate can compare it with the user's current
/// hash and drop sessions from before a password change.
#[must_use]
pub(crate) fn decode_full(
    secret: &AdminSessionSecret,
    value: &str,
) -> Option<(AdminSession, String)> {
    let (body, sig_b64) = value.split_once('.')?;
    let expected = sign(secret, body.as_bytes());
    let provided = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(sig_b64)
        .ok()?;
    // Constant-time comparison, the same primitive `tenancy::session`
    // uses. It blocks timing probes that forge a cookie byte by byte.
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
    Some((
        AdminSession {
            user_id: payload.user_id,
            username: payload.username,
            is_superuser: payload.is_superuser,
        },
        payload.auth_hash,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(user_id: i64, username: &str, is_superuser: bool) -> AdminSession {
        AdminSession {
            user_id,
            username: username.into(),
            is_superuser,
        }
    }

    #[test]
    fn round_trip_recovers_session_fields_and_auth_hash() {
        let secret = AdminSessionSecret::from_bytes(vec![42u8; 32]);
        let fp = password_fingerprint(&secret, "$argon2id$fake-hash");
        let cookie = encode(&secret, session(7, "alice", true), &fp);
        let (s, auth_hash) = decode_full(&secret, &cookie).expect("valid cookie verifies");
        assert_eq!(s.user_id, 7);
        assert!(s.is_superuser);
        assert_eq!(auth_hash, fp);
    }

    #[test]
    fn auth_hash_changes_with_password_hash() {
        // The fingerprint changes with the password hash, so the
        // gate's compare invalidates old cookies.
        let secret = AdminSessionSecret::from_bytes(vec![9u8; 32]);
        let before = password_fingerprint(&secret, "$argon2id$old");
        let after = password_fingerprint(&secret, "$argon2id$new");
        assert_ne!(before, after);
    }

    #[test]
    fn tampered_signature_rejected() {
        let secret = AdminSessionSecret::from_bytes(vec![1u8; 32]);
        let cookie = encode(&secret, session(1, "bob", false), "fp");
        let (body, _sig) = cookie.split_once('.').unwrap();
        let bad = format!("{body}.AAAA");
        assert!(decode(&secret, &bad).is_none());
    }

    #[test]
    fn wrong_secret_rejected() {
        let secret_a = AdminSessionSecret::from_bytes(vec![1u8; 32]);
        let secret_b = AdminSessionSecret::from_bytes(vec![2u8; 32]);
        let cookie = encode(&secret_a, session(1, "bob", false), "fp");
        assert!(decode(&secret_b, &cookie).is_none());
    }
}
