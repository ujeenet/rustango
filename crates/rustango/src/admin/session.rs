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

pub use crate::session::SessionSecret as AdminSessionSecret;
use crate::session::{sign, PasswordFingerprint};

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

/// Request marker the admin router adds: `true` under `with_user_perms`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PermsScoped(pub(crate) bool);

/// The one "acts as superuser" rule for admin writes: no per-user perm set
/// and, when signed in, a superuser session. No login means full access.
pub(crate) fn acting_superuser(perms_scoped: bool) -> bool {
    !perms_scoped && current().is_none_or(|s| s.is_superuser)
}

/// [`acting_superuser`] for an object-permission hook; `false` outside
/// an admin request.
#[must_use]
pub fn is_superuser(parts: &axum::http::request::Parts) -> bool {
    parts
        .extensions
        .get::<PermsScoped>()
        .is_some_and(|p| acting_superuser(p.0))
}

/// Object-permission hook: a superuser's row needs a superuser (#2521).
pub(crate) fn superuser_row_needs_superuser(
    parts: &axum::http::request::Parts,
    row: Option<&serde_json::Value>,
) -> bool {
    let row_is_superuser = row.and_then(|r| r.get("is_superuser")).is_some_and(|v| {
        v.as_bool()
            .unwrap_or_else(|| v.as_i64().is_some_and(|n| n != 0))
    });
    !row_is_superuser || is_superuser(parts)
}

/// The request's session: the extension, else the task-local. A
/// handler that reads only one of them misses the other mount path.
#[must_use]
pub fn from_extensions(extensions: &axum::http::Extensions) -> Option<AdminSession> {
    extensions.get::<AdminSession>().cloned().or_else(current)
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
    /// `Some(operator id)` when an operator is impersonating a tenant;
    /// then `user_id` is 0 and `username` empty (#2110).
    #[serde(default)]
    pub impersonated_by: Option<i64>,
}

impl AdminSession {
    /// A signed-in user's session.
    #[must_use]
    pub fn new(user_id: i64, username: impl Into<String>, is_superuser: bool) -> Self {
        Self {
            user_id,
            username: username.into(),
            is_superuser,
            impersonated_by: None,
        }
    }

    /// An operator impersonating a tenant, as superuser.
    #[must_use]
    pub fn impersonation(operator_id: i64) -> Self {
        Self {
            user_id: 0,
            username: String::new(),
            is_superuser: true,
            impersonated_by: Some(operator_id),
        }
    }

    /// Who authored a write, by id only: a username could pose as an
    /// operator. `as_token()` gives `user:<id>` or `operator:<id>:impersonating`.
    #[must_use]
    pub fn actor(&self) -> crate::audit::AuditSource {
        match self.impersonated_by {
            Some(id) => crate::audit::AuditSource::Custom(format!("operator:{id}:impersonating")),
            None => crate::audit::AuditSource::User {
                id: self.user_id.to_string(),
            },
        }
    }
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
    auth_hash: PasswordFingerprint,
    /// Issued-at, Unix seconds; checked against `sessions_revoked_at` (#1855).
    /// `0` for older cookies.
    #[serde(default)]
    iat: i64,
}

/// What the gate checks a cookie against the user's live row with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CookieAuth {
    /// Fingerprint of the password hash at login.
    pub(crate) auth_hash: PasswordFingerprint,
    /// Issued-at, Unix seconds.
    pub(crate) iat: i64,
}

impl CookiePayload {
    fn is_expired(&self) -> bool {
        chrono::Utc::now().timestamp() >= self.exp
    }
}

/// Sign a fresh session and return the cookie value to set. Lasts 8
/// hours. `auth_hash` is the fingerprint of the user's current
/// `password_hash`; `sessions_revoked_at` is the user's logout cut-off.
#[must_use]
pub(crate) fn encode(
    secret: &AdminSessionSecret,
    session: AdminSession,
    auth_hash: &PasswordFingerprint,
    sessions_revoked_at: Option<chrono::DateTime<chrono::Utc>>,
) -> String {
    let payload = CookiePayload {
        user_id: session.user_id,
        username: session.username,
        is_superuser: session.is_superuser,
        exp: chrono::Utc::now().timestamp() + DEFAULT_TTL_SECS,
        auth_hash: auth_hash.clone(),
        iat: crate::session::issued_at(sessions_revoked_at),
    };
    let json = serde_json::to_vec(&payload).expect("payload serializes");
    let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&json);
    let sig = sign(secret, body.as_bytes());
    let sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig);
    format!("{body}.{sig_b64}")
}

/// Verify and decode a cookie value. Returns `Some` only when the
/// signature is valid **and** the payload has not expired. Every other
/// case, such as a malformed or tampered cookie, returns `None`, and the
/// caller must treat the request as unauthenticated. The [`CookieAuth`]
/// lets the gate drop sessions from before a password change or logout.
#[must_use]
pub(crate) fn decode_full(
    secret: &AdminSessionSecret,
    value: &str,
) -> Option<(AdminSession, CookieAuth)> {
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
        AdminSession::new(payload.user_id, payload.username, payload.is_superuser),
        CookieAuth {
            auth_hash: payload.auth_hash,
            iat: payload.iat,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(user_id: i64, username: &str, is_superuser: bool) -> AdminSession {
        AdminSession::new(user_id, username, is_superuser)
    }

    fn parts(scoped: Option<bool>) -> axum::http::request::Parts {
        let (mut p, ()) = axum::http::Request::new(()).into_parts();
        if let Some(b) = scoped {
            p.extensions.insert(PermsScoped(b));
        }
        p
    }

    /// Both halves of the rule: the perm set and the session (#2520).
    #[tokio::test]
    async fn acting_superuser_needs_no_perm_set_and_a_superuser_session() {
        assert!(acting_superuser(false), "no login is full access");
        assert!(!acting_superuser(true));
        let under = |su: bool, scoped: bool| {
            CURRENT_SESSION.scope(session(1, "u", su), async move { acting_superuser(scoped) })
        };
        assert!(under(true, false).await);
        assert!(!under(false, false).await, "a non-superuser session");
        assert!(!under(true, true).await, "with_user_perms wins");
        // Outside an admin request a hook fails closed.
        assert!(!is_superuser(&parts(None)));
        assert!(is_superuser(&parts(Some(false))));
        assert!(!is_superuser(&parts(Some(true))));
    }

    #[test]
    fn a_superuser_row_needs_a_superuser() {
        let su = serde_json::json!({"is_superuser": true});
        let su_int = serde_json::json!({"is_superuser": 1});
        let member = serde_json::json!({"is_superuser": false});
        let scoped = parts(Some(true));
        assert!(!superuser_row_needs_superuser(&scoped, Some(&su)));
        assert!(!superuser_row_needs_superuser(&scoped, Some(&su_int)));
        assert!(superuser_row_needs_superuser(&scoped, Some(&member)));
        assert!(superuser_row_needs_superuser(&scoped, None));
        assert!(superuser_row_needs_superuser(
            &parts(Some(false)),
            Some(&su)
        ));
    }

    #[test]
    fn round_trip_recovers_session_fields_and_auth_hash() {
        let secret = AdminSessionSecret::from_bytes(vec![42u8; 32]);
        let fp = PasswordFingerprint::of(&secret, "$argon2id$fake-hash");
        let cookie = encode(&secret, session(7, "alice", true), &fp, None);
        let (s, auth) = decode_full(&secret, &cookie).expect("valid cookie verifies");
        assert_eq!(s.user_id, 7);
        assert!(s.is_superuser);
        assert_eq!(auth.auth_hash, fp);
        assert!(auth.iat > 0);
    }

    #[test]
    fn auth_hash_changes_with_password_hash() {
        // The fingerprint changes with the password hash, so the
        // gate's compare invalidates old cookies.
        let secret = AdminSessionSecret::from_bytes(vec![9u8; 32]);
        let before = PasswordFingerprint::of(&secret, "$argon2id$old");
        let after = PasswordFingerprint::of(&secret, "$argon2id$new");
        assert_ne!(before, after);
    }

    #[test]
    fn tampered_signature_rejected() {
        let secret = AdminSessionSecret::from_bytes(vec![1u8; 32]);
        let cookie = encode(
            &secret,
            session(1, "bob", false),
            &PasswordFingerprint::default(),
            None,
        );
        let (body, _sig) = cookie.split_once('.').unwrap();
        let bad = format!("{body}.AAAA");
        assert!(decode_full(&secret, &bad).is_none());
    }

    #[test]
    fn wrong_secret_rejected() {
        let secret_a = AdminSessionSecret::from_bytes(vec![1u8; 32]);
        let secret_b = AdminSessionSecret::from_bytes(vec![2u8; 32]);
        let cookie = encode(
            &secret_a,
            session(1, "bob", false),
            &PasswordFingerprint::default(),
            None,
        );
        assert!(decode_full(&secret_b, &cookie).is_none());
    }
}
