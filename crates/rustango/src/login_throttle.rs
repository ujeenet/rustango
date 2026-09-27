//! One gate in front of every built-in password login.
//!
//! [`LoginThrottle::begin`] runs before the user lookup and applies, in
//! order, a global ceiling, a per-IP limit and a per-account lock. The
//! account key is the normalized *submitted* username, so an unknown
//! name is limited exactly like a real one and the answer never says
//! which it was. The trade-off: a locked name is locked for its owner
//! too, until the lock expires.
//!
//! State: the per-IP and global buckets are in-process; the account
//! lock uses [`crate::account_lockout::shared`], which you can back
//! with a shared cache at boot.

use std::sync::OnceLock;
use std::time::Duration;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{Extensions, HeaderMap};
use axum::response::Response;

use crate::rate_limit::RateLimitLayer;

/// Default attempts per IP per [`DEFAULT_IP_WINDOW_SECS`].
pub const DEFAULT_IP_LIMIT: u32 = 20;
/// Default per-IP window.
pub const DEFAULT_IP_WINDOW_SECS: u64 = 60;
/// Default attempts across all clients per [`DEFAULT_GLOBAL_WINDOW_SECS`].
pub const DEFAULT_GLOBAL_LIMIT: u32 = 600;
/// Default global window.
pub const DEFAULT_GLOBAL_WINDOW_SECS: u64 = 60;

/// Limits for [`LoginThrottle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoginLimits {
    /// Attempts one IP may make per `ip_window`.
    pub ip_limit: u32,
    pub ip_window: Duration,
    /// Attempts all clients together may make per `global_window`.
    pub global_limit: u32,
    pub global_window: Duration,
}

impl Default for LoginLimits {
    fn default() -> Self {
        Self {
            ip_limit: DEFAULT_IP_LIMIT,
            ip_window: Duration::from_secs(DEFAULT_IP_WINDOW_SECS),
            global_limit: DEFAULT_GLOBAL_LIMIT,
            global_window: Duration::from_secs(DEFAULT_GLOBAL_WINDOW_SECS),
        }
    }
}

impl LoginLimits {
    /// Read the `[auth] login_*` keys; unset keys keep the defaults.
    #[cfg(feature = "config")]
    #[must_use]
    pub fn from_settings(s: &crate::config::AuthSettings) -> Self {
        let d = Self::default();
        Self {
            ip_limit: s.login_ip_limit.unwrap_or(d.ip_limit),
            ip_window: s
                .login_ip_window_secs
                .map_or(d.ip_window, Duration::from_secs),
            global_limit: s.login_global_limit.unwrap_or(d.global_limit),
            global_window: s
                .login_global_window_secs
                .map_or(d.global_window, Duration::from_secs),
        }
    }
}

/// Why a login was refused before its password was checked, or while
/// waiting to check it. The response is the same whether or not the
/// account exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginRefused {
    /// Too many attempts: `429` with `Retry-After`.
    Throttled { retry_after_secs: u64 },
    /// No password-hashing slot freed up in time: `503` with `Retry-After`.
    Busy,
}

impl LoginRefused {
    /// The `429` / `503` response, with `Retry-After`.
    #[must_use]
    pub fn into_response(self) -> Response {
        use axum::response::IntoResponse as _;
        match self {
            Self::Throttled { retry_after_secs } => {
                crate::api_errors::ApiError::rate_limited_response(
                    "too many login attempts",
                    retry_after_secs,
                )
            }
            Self::Busy => {
                let mut resp =
                    crate::api_errors::ApiError::service_unavailable("login is busy, try again")
                        .into_response();
                resp.headers_mut()
                    .insert(axum::http::header::RETRY_AFTER, 1u64.into());
                resp
            }
        }
    }
}

impl axum::response::IntoResponse for LoginRefused {
    fn into_response(self) -> Response {
        LoginRefused::into_response(self)
    }
}

/// The client address a login is limited by: a trusted forwarded IP,
/// else the connecting socket, else unknown (no per-IP limit).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientIp(Option<String>);

impl ClientIp {
    /// Resolve from request parts, as [`crate::rate_limit`] does.
    #[must_use]
    pub fn from_parts(extensions: &Extensions, headers: &HeaderMap) -> Self {
        Self(crate::rate_limit::client_ip(extensions, headers))
    }
}

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self::from_parts(&parts.extensions, &parts.headers))
    }
}

/// The login gate. Use [`shared`] unless you are testing.
pub struct LoginThrottle {
    ip: RateLimitLayer,
    global: RateLimitLayer,
}

impl LoginThrottle {
    #[must_use]
    pub fn new(limits: LoginLimits) -> Self {
        Self {
            ip: RateLimitLayer::per_ip(limits.ip_limit.max(1), limits.ip_window),
            global: RateLimitLayer::global(limits.global_limit.max(1), limits.global_window),
        }
    }

    /// Admit one attempt at `username` in `scope` (for example `admin`
    /// or `tenant:<slug>`). Call before the user lookup, then report
    /// the outcome on the returned [`LoginAttempt`].
    ///
    /// # Errors
    /// [`LoginRefused::Throttled`] when any limit is spent.
    pub async fn begin(
        &self,
        scope: &str,
        ip: &ClientIp,
        username: &str,
    ) -> Result<LoginAttempt, LoginRefused> {
        let throttled = |retry_after_secs| LoginRefused::Throttled { retry_after_secs };
        self.global.take("<global>").await.map_err(throttled)?;
        if let Some(ip) = &ip.0 {
            self.ip.take(ip).await.map_err(throttled)?;
        }
        Self::account(scope, username).await
    }

    /// Only the account lock. For credentials sent on every request
    /// (HTTP Basic), where a per-IP login limit would throttle normal use.
    ///
    /// # Errors
    /// [`LoginRefused::Throttled`] while the username is locked.
    pub async fn account(scope: &str, username: &str) -> Result<LoginAttempt, LoginRefused> {
        let key = account_key(scope, username);
        let lockout = crate::account_lockout::shared();
        if lockout.is_locked(&key).await {
            return Err(LoginRefused::Throttled {
                retry_after_secs: lockout.lock_duration().as_secs().max(1),
            });
        }
        Ok(LoginAttempt { key })
    }
}

/// One admitted attempt. Report [`Self::failed`] or [`Self::succeeded`].
#[must_use = "report the outcome, or failures never lock the account"]
pub struct LoginAttempt {
    key: String,
}

impl LoginAttempt {
    /// Count a failure against the submitted username, known or not.
    pub async fn failed(self) {
        let _ = crate::account_lockout::shared()
            .record_failure(&self.key)
            .await;
    }

    /// Clear the username's failures.
    pub async fn succeeded(self) {
        crate::account_lockout::shared().clear(&self.key).await;
    }
}

/// `login:<scope>:<sha256 of the trimmed, lowercased username>`. Hashed
/// so the key has a fixed size and a shared cache never holds the name.
fn account_key(scope: &str, username: &str) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    let digest = Sha256::digest(username.trim().to_lowercase().as_bytes());
    let mut key = format!("login:{scope}:");
    for b in digest {
        let _ = write!(key, "{b:02x}");
    }
    key
}

static SHARED: OnceLock<LoginThrottle> = OnceLock::new();

/// The gate every built-in login uses, with [`LoginLimits::default`]
/// unless [`configure_shared`] ran first.
#[must_use]
pub fn shared() -> &'static LoginThrottle {
    SHARED.get_or_init(|| LoginThrottle::new(LoginLimits::default()))
}

/// Install the [`shared`] gate at boot. First call wins; `false` after.
pub fn configure_shared(throttle: LoginThrottle) -> bool {
    SHARED.set(throttle).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> ClientIp {
        ClientIp(Some(s.to_owned()))
    }

    #[test]
    fn account_key_normalizes_and_scopes() {
        assert_eq!(
            account_key("admin", " Alice "),
            account_key("admin", "alice")
        );
        assert_ne!(account_key("admin", "alice"), account_key("op", "alice"));
        assert!(!account_key("admin", "alice").contains("alice"));
    }

    #[tokio::test]
    async fn per_ip_limit_spares_other_ips() {
        let t = LoginThrottle::new(LoginLimits {
            ip_limit: 2,
            ..LoginLimits::default()
        });
        for n in 0..2 {
            let a = t.begin("t-ip", &ip("10.9.0.1"), &format!("u{n}")).await;
            assert!(a.is_ok());
        }
        let r = t.begin("t-ip", &ip("10.9.0.1"), "u9").await;
        assert!(matches!(r, Err(LoginRefused::Throttled { .. })));
        assert!(t.begin("t-ip", &ip("10.9.0.2"), "u9").await.is_ok());
    }

    #[tokio::test]
    async fn global_ceiling_applies_without_an_ip() {
        let t = LoginThrottle::new(LoginLimits {
            global_limit: 1,
            ..LoginLimits::default()
        });
        assert!(t.begin("t-g", &ClientIp::default(), "a").await.is_ok());
        let r = t.begin("t-g", &ClientIp::default(), "b").await;
        assert!(matches!(r, Err(LoginRefused::Throttled { .. })));
    }

    #[test]
    fn refusals_carry_retry_after() {
        let r = LoginRefused::Throttled {
            retry_after_secs: 9,
        }
        .into_response();
        assert_eq!(r.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(r.headers()[axum::http::header::RETRY_AFTER], "9");
        let r = LoginRefused::Busy.into_response();
        assert_eq!(r.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert!(r.headers().contains_key(axum::http::header::RETRY_AFTER));
    }
}
