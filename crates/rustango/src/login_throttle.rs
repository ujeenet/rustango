//! One gate in front of every built-in password login.
//!
//! [`LoginThrottle::begin`] runs before the user lookup and checks, in
//! order, the account lock, a per-IP limit and the scope's global limit.
//! A request refused by one check spends nothing from the later ones.
//!
//! - **Account lock**: keyed by the normalized *submitted* username, so
//!   an unknown name is limited exactly like a real one. Once the row is
//!   found, [`LoginAttempt::resolve`] adds the stored username, so every
//!   spelling the database matches to one account shares one lock.
//! - **Per IP** (IPv6 by /64) and **per scope** ([`LoginScope`]): only
//!   failed attempts use them up; a successful login gives its token back.
//!   Credentials sent on every request (HTTP Basic, API keys) are only
//!   charged when they fail.
//!
//! Shared across scopes: the per-IP buckets, the account-lock store and
//! the password-hashing queue ([`crate::passwords`]).
//!
//! State: the per-IP and global buckets are in-process unless built
//! with [`LoginThrottle::with_cache`]; the account lock uses
//! [`crate::account_lockout::shared`]. Back both with a shared cache at
//! boot when you run more than one replica.
//!
//! Behind a reverse proxy, mount [`crate::real_ip::RealIpLayer`] with
//! trusted proxies. Without it every client shares the proxy's per-IP
//! bucket (a warning is logged once when forwarding headers arrive).

use std::sync::Arc;
use std::time::Duration;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{Extensions, HeaderMap};
use axum::response::Response;

use crate::cache::BoxedCache;
use crate::rate_limit::RateLimitLayer;
use crate::rate_limit_cache::CacheRateLimitLayer;

/// Default failed attempts per IP per [`DEFAULT_IP_WINDOW_SECS`].
pub const DEFAULT_IP_LIMIT: u32 = 20;
/// Default per-IP window.
pub const DEFAULT_IP_WINDOW_SECS: u64 = 60;
/// Default failed attempts per scope per [`DEFAULT_GLOBAL_WINDOW_SECS`].
pub const DEFAULT_GLOBAL_LIMIT: u32 = 600;
/// Default global window.
pub const DEFAULT_GLOBAL_WINDOW_SECS: u64 = 60;

/// Limits for [`LoginThrottle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoginLimits {
    /// Failed attempts one IP may make per `ip_window`.
    pub ip_limit: u32,
    pub ip_window: Duration,
    /// Failed attempts all clients together may make per scope per
    /// `global_window`.
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

/// Which login an attempt belongs to. Each scope has its own global
/// limit and its own account locks.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LoginScope {
    /// The bare admin login form.
    Admin,
    /// The operator console login form.
    Operator,
    /// A tenant's admin login form and JWT login (one user table).
    Tenant(String),
    /// HTTP Basic against a tenant. Only failures count.
    TenantBasic(String),
    /// API keys against a tenant. Only failures count; no account lock.
    TenantApiKey(String),
}

impl LoginScope {
    fn key(&self) -> String {
        match self {
            Self::Admin => "admin".to_owned(),
            Self::Operator => "operator".to_owned(),
            Self::Tenant(slug) => format!("tenant:{slug}"),
            Self::TenantBasic(slug) => format!("tenant-basic:{slug}"),
            Self::TenantApiKey(slug) => format!("tenant-apikey:{slug}"),
        }
    }

    /// Sent on every request, so normal traffic must not use up limits.
    fn per_request(&self) -> bool {
        matches!(self, Self::TenantBasic(_) | Self::TenantApiKey(_))
    }

    fn locks_accounts(&self) -> bool {
        !matches!(self, Self::TenantApiKey(_))
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
/// else the connecting socket, else unknown (no per-IP limit). IPv6
/// addresses are grouped by /64.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientIp(Option<String>);

impl ClientIp {
    /// Resolve from request parts, as [`crate::rate_limit`] does.
    #[must_use]
    pub fn from_parts(extensions: &Extensions, headers: &HeaderMap) -> Self {
        Self(crate::rate_limit::client_ip(extensions, headers).map(crate::rate_limit::ip_bucket))
    }
}

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self::from_parts(&parts.extensions, &parts.headers))
    }
}

struct Buckets {
    limits: LoginLimits,
    ip: Limit,
    /// Keyed by [`LoginScope::key`].
    global: Limit,
}

/// Where one limit counts.
enum Limit {
    /// A token bucket in this process.
    Memory(RateLimitLayer),
    /// A fixed-window counter in a cache; on a cache error the attempt
    /// counts in the in-process `fallback`, so the limit never opens.
    Cache {
        shared: CacheRateLimitLayer,
        fallback: RateLimitLayer,
    },
}

/// What [`Limit::take`] counted, so the refund goes to the same bucket.
#[derive(Debug, Clone, Copy)]
enum Spent {
    Nothing,
    Memory,
    /// The cache window it was counted in.
    Window(u64),
}

impl Limit {
    async fn peek(&self, key: &str) -> Result<(), u64> {
        match self {
            Self::Memory(l) => l.peek(key).await,
            Self::Cache { shared, fallback } => match shared.try_peek(key).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(target: "rustango::rate_limit", error = %e, "login limit cache read failed");
                    fallback.peek(key).await
                }
            },
        }
    }

    async fn take(&self, key: &str) -> Result<Spent, u64> {
        match self {
            Self::Memory(l) => l.take(key).await.map(|_| Spent::Memory),
            Self::Cache { shared, fallback } => match shared.try_take(key).await {
                Ok(r) => r.map(|(_, window)| Spent::Window(window)),
                Err(e) => {
                    tracing::warn!(target: "rustango::rate_limit", error = %e, "login limit cache incr failed");
                    fallback.take(key).await.map(|_| Spent::Memory)
                }
            },
        }
    }

    async fn give_back(&self, key: &str, spent: Spent) {
        match (self, spent) {
            (Self::Memory(l) | Self::Cache { fallback: l, .. }, Spent::Memory) => {
                l.give_back(key).await;
            }
            (Self::Cache { shared, .. }, Spent::Window(w)) => shared.give_back_at(key, w).await,
            _ => {}
        }
    }

    fn cache(&self) -> Option<&BoxedCache> {
        match self {
            Self::Memory(_) => None,
            Self::Cache { shared, .. } => Some(shared.cache()),
        }
    }
}

/// The login gate. Use [`shared`] unless you are testing.
pub struct LoginThrottle(Arc<Buckets>);

impl LoginThrottle {
    /// Per-IP and global limits counted in this process.
    #[must_use]
    pub fn new(limits: LoginLimits) -> Self {
        Self(Arc::new(Buckets {
            limits,
            ip: Limit::Memory(memory_ip(limits)),
            global: Limit::Memory(memory_global(limits)),
        }))
    }

    /// Per-IP and global limits counted in `cache`, so every replica on
    /// one Redis or database cache shares them (#1809). On a cache error
    /// they count in process.
    ///
    /// Fixed windows: a burst across a window edge can reach 2× a limit.
    /// Keys are `login-ip:*` / `login-global:*`, so apps on one cache share
    /// them; give each app its own [`crate::cache::ScopedCache`].
    #[must_use]
    pub fn with_cache(limits: LoginLimits, cache: BoxedCache) -> Self {
        let ip = CacheRateLimitLayer::new(cache.clone(), limits.ip_limit.max(1), limits.ip_window);
        let global =
            CacheRateLimitLayer::new(cache, limits.global_limit.max(1), limits.global_window);
        Self(Arc::new(Buckets {
            limits,
            ip: Limit::Cache {
                shared: ip.key_prefix("login-ip"),
                fallback: memory_ip(limits),
            },
            global: Limit::Cache {
                shared: global.key_prefix("login-global"),
                fallback: memory_global(limits),
            },
        }))
    }

    /// `true` when each replica counts its own per-IP and global limits.
    #[must_use]
    pub fn is_process_local(&self) -> bool {
        self.0.ip.cache().is_none_or(|c| c.is_process_local())
    }

    /// The warning for limits that never count or count per process.
    fn store_warning(&self) -> Option<&'static str> {
        if self.0.ip.cache().is_some_and(|c| c.stores_nothing()) {
            return Some(NULL_STORE_WARNING);
        }
        self.is_process_local().then_some(PROCESS_LOCAL_WARNING)
    }

    fn warn_once_on_weak_store(&self) {
        static WARNED: std::sync::Once = std::sync::Once::new();
        if let Some(msg) = self.store_warning() {
            WARNED.call_once(|| tracing::warn!(target: "rustango::rate_limit", "{msg}"));
        }
    }

    /// The limits this gate enforces.
    #[must_use]
    pub fn limits(&self) -> LoginLimits {
        self.0.limits
    }

    /// Admit one attempt at `username` in `scope`. Call before the user
    /// lookup, then report the outcome on the returned [`LoginAttempt`].
    /// `username` is ignored for [`LoginScope::TenantApiKey`].
    ///
    /// # Errors
    /// [`LoginRefused::Throttled`] when the account is locked or a limit
    /// is spent.
    pub async fn begin(
        &self,
        scope: &LoginScope,
        ip: &ClientIp,
        username: &str,
    ) -> Result<LoginAttempt, LoginRefused> {
        let throttled = |retry_after_secs| LoginRefused::Throttled { retry_after_secs };
        self.warn_once_on_weak_store();
        let scope_key = scope.key();
        let per_request = scope.per_request();
        let mut keys = Vec::with_capacity(2);
        if scope.locks_accounts() {
            let key = account_key(&scope_key, username);
            check_lock(&key).await?;
            keys.push(key);
        }
        let b = &self.0;
        let mut ip_spent = Spent::Nothing;
        if let Some(ip) = &ip.0 {
            if per_request {
                b.ip.peek(ip).await.map_err(throttled)?;
            } else {
                ip_spent = b.ip.take(ip).await.map_err(throttled)?;
            }
        }
        let global = if per_request {
            b.global.peek(&scope_key).await.map(|()| Spent::Nothing)
        } else {
            b.global.take(&scope_key).await
        };
        let global_spent = match global {
            Ok(spent) => spent,
            Err(secs) => {
                if let Some(ip) = &ip.0 {
                    b.ip.give_back(ip, ip_spent).await;
                }
                return Err(throttled(secs));
            }
        };
        Ok(LoginAttempt {
            buckets: Arc::clone(b),
            scope_key,
            ip: ip.0.clone(),
            per_request,
            spent: (ip_spent, global_spent),
            keys,
        })
    }

    /// Run a signed-in user's current-password check through the gate, so a
    /// stolen session cannot guess it at hash speed (#1873). A miss counts
    /// toward the same account lock as the login form.
    ///
    /// # Errors
    /// [`LoginRefused`] when throttled, or when `verify` returns one.
    pub async fn verify_current_password(
        &self,
        scope: &LoginScope,
        ip: &ClientIp,
        username: &str,
        verify: impl std::future::Future<Output = Result<bool, LoginRefused>>,
    ) -> Result<bool, LoginRefused> {
        let attempt = self.begin(scope, ip, username).await?;
        let ok = match verify.await {
            Ok(ok) => ok,
            // A full hash queue says nothing about the password: give the tokens back.
            Err(refused) => {
                attempt.prompted().await;
                return Err(refused);
            }
        };
        if ok {
            attempt.succeeded().await;
        } else {
            attempt.failed().await;
        }
        Ok(ok)
    }
}

fn memory_ip(limits: LoginLimits) -> RateLimitLayer {
    RateLimitLayer::per_ip(limits.ip_limit.max(1), limits.ip_window)
}

fn memory_global(limits: LoginLimits) -> RateLimitLayer {
    RateLimitLayer::global(limits.global_limit.max(1), limits.global_window)
}

async fn check_lock(key: &str) -> Result<(), LoginRefused> {
    let lockout = crate::account_lockout::shared();
    crate::account_lockout::warn_once_on_weak_store(lockout);
    if lockout.is_locked(key).await {
        return Err(LoginRefused::Throttled {
            retry_after_secs: lockout.lock_duration().as_secs().max(1),
        });
    }
    Ok(())
}

/// One admitted attempt. Report [`Self::failed`] or [`Self::succeeded`].
#[must_use = "report the outcome, or failures never lock the account"]
pub struct LoginAttempt {
    buckets: Arc<Buckets>,
    scope_key: String,
    ip: Option<String>,
    per_request: bool,
    /// What `begin` counted per IP and globally, refunded on success.
    spent: (Spent, Spent),
    /// Account-lock keys: the submitted name, then the stored one.
    keys: Vec<String>,
}

impl LoginAttempt {
    /// Call once the row is found, before the password check, with the
    /// username as stored. Every spelling that finds this row then shares
    /// its lock.
    ///
    /// # Errors
    /// [`LoginRefused::Throttled`] while the stored username is locked.
    pub async fn resolve(&mut self, stored_username: &str) -> Result<(), LoginRefused> {
        if self.keys.is_empty() {
            return Ok(());
        }
        let key = account_key(&self.scope_key, stored_username);
        if !self.keys.contains(&key) {
            check_lock(&key).await?;
            self.keys.push(key);
        }
        Ok(())
    }

    /// Count a failure against the username, known or not.
    pub async fn failed(self) {
        let lockout = crate::account_lockout::shared();
        for key in &self.keys {
            let _ = lockout.record_failure(key).await;
        }
        if self.per_request {
            if let Some(ip) = &self.ip {
                let _ = self.buckets.ip.take(ip).await;
            }
            let _ = self.buckets.global.take(&self.scope_key).await;
        }
    }

    /// Clear the username's failures and give back the limit tokens.
    pub async fn succeeded(self) {
        let lockout = crate::account_lockout::shared();
        for key in &self.keys {
            if lockout.attempt_count(key).await > 0 {
                lockout.clear(key).await;
            }
        }
        self.give_back().await;
    }

    /// The password was right and the form now asks for a second
    /// factor: give back the limit tokens, keep the username's failures.
    pub async fn prompted(self) {
        self.give_back().await;
    }

    async fn give_back(&self) {
        let (ip_spent, global_spent) = self.spent;
        if let Some(ip) = &self.ip {
            self.buckets.ip.give_back(ip, ip_spent).await;
        }
        self.buckets
            .global
            .give_back(&self.scope_key, global_spent)
            .await;
    }
}

/// `login:<scope>:<sha256 of the trimmed, lowercased username>`. Hashed
/// so the key has a fixed size and a shared cache never holds the name.
fn account_key(scope_key: &str, username: &str) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    let digest = Sha256::digest(username.trim().to_lowercase().as_bytes());
    let mut key = format!("login:{scope_key}:");
    for b in digest {
        let _ = write!(key, "{b:02x}");
    }
    key
}

/// `check --deploy` can't run the app's startup code, so it only advises.
pub(crate) const PROCESS_LOCAL_NOTE: &str =
    "login per-IP and global limits: unless your app installs \
     `login_throttle::configure_shared(LoginThrottle::with_cache(limits, cache))` with a Redis or \
     database cache, each replica counts its own, so N replicas allow N times each limit";

/// Logged once, at the first login, when the limits count per process.
const PROCESS_LOCAL_WARNING: &str =
    "login per-IP and global limits are counted in process memory, so each replica allows its own \
     attempts; install `login_throttle::configure_shared(LoginThrottle::with_cache(limits, cache))` \
     with a Redis or database cache";

/// Logged once when the limits sit on a cache that drops writes.
const NULL_STORE_WARNING: &str =
    "login per-IP and global limits use a cache that stores nothing (`NullCache`), so they never \
     count; use a Redis, database or in-memory cache";

static SHARED: crate::boot_slot::BootSlot<LoginThrottle> = crate::boot_slot::BootSlot::new();

/// The gate every built-in login uses, with [`LoginLimits::default`]
/// unless [`configure_shared`] or `[auth] login_*` settings set it.
#[must_use]
pub fn shared() -> &'static LoginThrottle {
    SHARED.get(|| LoginThrottle::new(LoginLimits::default()))
}

/// Install the [`shared`] gate at boot. It replaces the default and the
/// one built from `[auth]` settings; `false` if an earlier call won.
pub fn configure_shared(throttle: LoginThrottle) -> bool {
    SHARED.set_explicit(throttle)
}

/// The `[auth] login_*` gate; `false` if app code already set one.
// Only `manage` applies settings (#1948).
#[cfg(all(feature = "config", feature = "manage"))]
pub(crate) fn configure_from_settings(throttle: LoginThrottle) -> bool {
    SHARED.set_from_settings(throttle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> ClientIp {
        ClientIp(Some(s.to_owned()))
    }

    fn tenant(s: &str) -> LoginScope {
        LoginScope::Tenant(s.to_owned())
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

    #[test]
    fn each_scope_has_its_own_key() {
        let keys = [
            LoginScope::Admin.key(),
            LoginScope::Operator.key(),
            tenant("a").key(),
            LoginScope::TenantBasic("a".into()).key(),
            LoginScope::TenantApiKey("a".into()).key(),
        ];
        for (i, a) in keys.iter().enumerate() {
            for b in &keys[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[tokio::test]
    async fn per_ip_limit_spares_other_ips() {
        let t = LoginThrottle::new(LoginLimits {
            ip_limit: 2,
            ..LoginLimits::default()
        });
        let s = tenant("t-ip");
        for n in 0..2 {
            let a = t.begin(&s, &ip("10.9.0.1"), &format!("u{n}")).await;
            a.unwrap().failed().await;
        }
        let r = t.begin(&s, &ip("10.9.0.1"), "u9").await;
        assert!(matches!(r, Err(LoginRefused::Throttled { .. })));
        assert!(t.begin(&s, &ip("10.9.0.2"), "u9").await.is_ok());
    }

    /// A busy verify spends no limit token.
    #[tokio::test]
    async fn busy_current_password_check_gives_tokens_back() {
        let t = LoginThrottle::new(LoginLimits {
            global_limit: 1,
            ..LoginLimits::default()
        });
        let s = tenant("t-busy");
        for _ in 0..2 {
            let r = t
                .verify_current_password(&s, &ClientIp::default(), "u", async {
                    Err(LoginRefused::Busy)
                })
                .await;
            assert_eq!(r, Err(LoginRefused::Busy));
        }
    }

    #[tokio::test]
    async fn global_ceiling_applies_without_an_ip() {
        let t = LoginThrottle::new(LoginLimits {
            global_limit: 1,
            ..LoginLimits::default()
        });
        let s = tenant("t-g");
        let a = t.begin(&s, &ClientIp::default(), "a").await.unwrap();
        a.failed().await;
        let r = t.begin(&s, &ClientIp::default(), "b").await;
        assert!(matches!(r, Err(LoginRefused::Throttled { .. })));
    }

    /// A request the per-IP limit refuses takes nothing from the global
    /// budget, so one IP cannot drain it for everyone.
    #[tokio::test]
    async fn an_ip_refusal_leaves_the_global_budget() {
        let t = LoginThrottle::new(LoginLimits {
            ip_limit: 1,
            global_limit: 3,
            ..LoginLimits::default()
        });
        let s = tenant("t-order");
        t.begin(&s, &ip("10.9.1.1"), "a")
            .await
            .unwrap()
            .failed()
            .await;
        for _ in 0..10 {
            assert!(t.begin(&s, &ip("10.9.1.1"), "a").await.is_err());
        }
        for n in 2..4 {
            let a = t.begin(&s, &ip(&format!("10.9.1.{n}")), "b").await;
            assert!(a.is_ok(), "global budget was drained by refused requests");
        }
    }

    /// One scope using up its global limit leaves the others alone.
    #[tokio::test]
    async fn global_limits_are_per_scope() {
        let t = LoginThrottle::new(LoginLimits {
            global_limit: 1,
            ..LoginLimits::default()
        });
        let none = ClientIp::default();
        t.begin(&tenant("t-a"), &none, "x")
            .await
            .unwrap()
            .failed()
            .await;
        assert!(t.begin(&tenant("t-a"), &none, "y").await.is_err());
        assert!(t.begin(&tenant("t-b"), &none, "y").await.is_ok());
        assert!(t.begin(&LoginScope::Operator, &none, "y").await.is_ok());
    }

    /// Successful logins give their tokens back, so users behind one
    /// address are not limited by their own logins.
    #[tokio::test]
    async fn successes_do_not_use_up_the_limits() {
        let t = LoginThrottle::new(LoginLimits {
            ip_limit: 2,
            global_limit: 2,
            ..LoginLimits::default()
        });
        let s = tenant("t-ok");
        for n in 0..10 {
            let a = t.begin(&s, &ip("10.9.2.1"), &format!("ok{n}")).await;
            a.unwrap().succeeded().await;
        }
    }

    /// Per-request credentials are charged only when they fail.
    #[tokio::test]
    async fn per_request_scopes_count_only_failures() {
        let t = LoginThrottle::new(LoginLimits {
            ip_limit: 2,
            ..LoginLimits::default()
        });
        let s = LoginScope::TenantApiKey("t-key".into());
        // Five in flight at once, more than the limit.
        let mut held = Vec::new();
        for _ in 0..5 {
            held.push(t.begin(&s, &ip("10.9.3.1"), "").await);
        }
        for a in held {
            a.unwrap().succeeded().await;
        }
        for _ in 0..2 {
            t.begin(&s, &ip("10.9.3.1"), "")
                .await
                .unwrap()
                .failed()
                .await;
        }
        assert!(t.begin(&s, &ip("10.9.3.1"), "").await.is_err());
    }

    fn on_cache(cache: &BoxedCache, limits: LoginLimits) -> LoginThrottle {
        LoginThrottle::with_cache(limits, cache.clone())
    }

    /// Two replicas on one cache share the per-IP and global limits (#1809).
    #[tokio::test]
    async fn replicas_on_one_cache_share_the_limits() {
        let cache: BoxedCache = Arc::new(crate::cache::InMemoryCache::new());
        let limits = LoginLimits {
            ip_limit: 2,
            global_limit: 3,
            ..LoginLimits::default()
        };
        let (a, b) = (on_cache(&cache, limits), on_cache(&cache, limits));
        let s = tenant("t-shared");
        let one = ip("10.9.4.1");
        a.begin(&s, &one, "u").await.unwrap().failed().await;
        b.begin(&s, &one, "v").await.unwrap().failed().await;
        assert!(a.begin(&s, &one, "w").await.is_err(), "per-IP not shared");
        b.begin(&s, &ip("10.9.4.2"), "x")
            .await
            .unwrap()
            .failed()
            .await;
        let r = a.begin(&s, &ip("10.9.4.3"), "y").await;
        assert!(r.is_err(), "global not shared");
    }

    /// On the cache, successes still give their tokens back.
    #[tokio::test]
    async fn cache_backed_successes_do_not_use_up_the_limits() {
        let cache: BoxedCache = Arc::new(crate::cache::InMemoryCache::new());
        let t = on_cache(
            &cache,
            LoginLimits {
                ip_limit: 2,
                global_limit: 2,
                ..LoginLimits::default()
            },
        );
        for n in 0..10 {
            let a = t
                .begin(&tenant("t-ok2"), &ip("10.9.5.1"), &format!("ok{n}"))
                .await;
            a.unwrap().succeeded().await;
        }
    }

    struct Down;

    #[async_trait::async_trait]
    impl crate::cache::Cache for Down {
        async fn get(&self, _: &str) -> Result<Option<String>, crate::cache::CacheError> {
            Err(crate::cache::CacheError::Connection("down".into()))
        }
        async fn set(
            &self,
            _: &str,
            _: &str,
            _: Option<Duration>,
        ) -> Result<(), crate::cache::CacheError> {
            Err(crate::cache::CacheError::Connection("down".into()))
        }
        async fn delete(&self, _: &str) -> Result<(), crate::cache::CacheError> {
            Err(crate::cache::CacheError::Connection("down".into()))
        }
        async fn exists(&self, _: &str) -> Result<bool, crate::cache::CacheError> {
            Err(crate::cache::CacheError::Connection("down".into()))
        }
        async fn clear(&self) -> Result<(), crate::cache::CacheError> {
            Err(crate::cache::CacheError::Connection("down".into()))
        }
    }

    fn down(ip_limit: u32, global_limit: u32) -> LoginThrottle {
        let cache: BoxedCache = Arc::new(Down);
        on_cache(
            &cache,
            LoginLimits {
                ip_limit,
                global_limit,
                ..LoginLimits::default()
            },
        )
    }

    /// A cache outage keeps the per-IP limit, counted in process.
    #[tokio::test]
    async fn a_cache_outage_keeps_the_per_ip_limit() {
        let t = down(1, 10);
        let s = tenant("t-down");
        t.begin(&s, &ip("10.9.6.1"), "a")
            .await
            .unwrap()
            .failed()
            .await;
        assert!(t.begin(&s, &ip("10.9.6.1"), "a").await.is_err());
        assert!(t.begin(&s, &ip("10.9.6.2"), "b").await.is_ok());
    }

    /// A cache outage keeps the global limit too: many IPs are still capped.
    #[tokio::test]
    async fn a_cache_outage_keeps_the_global_limit() {
        let t = down(10, 1);
        let s = tenant("t-down-g");
        t.begin(&s, &ip("10.9.7.1"), "a")
            .await
            .unwrap()
            .failed()
            .await;
        assert!(t.begin(&s, &ip("10.9.7.2"), "b").await.is_err());
    }

    /// Successes during an outage refund the in-process fallback buckets.
    #[tokio::test]
    async fn a_cache_outage_refunds_the_fallback() {
        let t = down(1, 1);
        let s = tenant("t-down-ok");
        for n in 0..5 {
            let a = t.begin(&s, &ip("10.9.8.1"), &format!("ok{n}")).await;
            a.unwrap().succeeded().await;
        }
    }

    #[test]
    fn weak_stores_are_reported() {
        let limits = LoginLimits::default();
        let memory = LoginThrottle::new(limits);
        assert!(memory.is_process_local());
        assert_eq!(memory.store_warning(), Some(PROCESS_LOCAL_WARNING));
        let null: BoxedCache = Arc::new(crate::cache::NullCache);
        let null = LoginThrottle::with_cache(limits, null);
        assert_eq!(null.store_warning(), Some(NULL_STORE_WARNING));
        assert!(LoginThrottle::with_cache(limits, Arc::new(Down))
            .store_warning()
            .is_none());
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
