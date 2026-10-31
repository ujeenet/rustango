//! One gate in front of every built-in password login.
//!
//! [`LoginThrottle::begin`] runs before the user lookup and reserves, in
//! order, an account-lock attempt, a per-IP token and a token from the
//! scope's global limit. Reserving before the password check means a
//! parallel burst cannot all pass a peek (#2378). A request refused by
//! one check spends nothing from the later ones. A limit full only with
//! attempts still in flight in this process makes the next one wait, so
//! parallel right credentials are delayed, never refused.
//!
//! - **Account lock**: keyed by the normalized *submitted* username, so
//!   an unknown name is limited exactly like a real one. Once the row is
//!   found, [`LoginAttempt::resolve`] adds the stored username, so every
//!   spelling the database matches to one account shares one lock.
//! - **Per IP** (IPv6 by /64) and **per scope** ([`LoginScope`]): only
//!   failed attempts use them up; a successful login gives its token back,
//!   so credentials sent on every request (HTTP Basic, API keys) only hold
//!   a token while they are checked. Only failures lock an account.
//! - **Known clients**: a username that signed in from an IP within
//!   [`KNOWN_CLIENT_TTL`] skips a spent global limit from that IP, so a
//!   spray from many IPs cannot lock every user out. Its per-IP limit and
//!   account lock still apply.
//!
//! Shared across scopes: the per-IP buckets (agent secrets have their
//! own), the account-lock store and the password-hashing queue
//! ([`crate::passwords`]).
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

use tokio::sync::Notify;

use crate::account_lockout::{Reservation, ReservedSlot};
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
/// How long a sign-in lets its username and IP skip a spent global limit.
pub const KNOWN_CLIENT_TTL: Duration = Duration::from_secs(30 * 24 * 3600);
/// Known clients one process remembers without a shared cache; the least
/// recently used go first.
const KNOWN_CLIENT_MEMORY_CAP: usize = 10_000;

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
    /// HTTP Basic against a tenant. Only failures keep their token.
    TenantBasic(String),
    /// API keys against a tenant. Only failures keep their token; no account lock.
    TenantApiKey(String),
    /// MCP agent secrets against a tenant, as for API keys.
    TenantAgent(String),
    /// SSO callbacks: `admin`, `tenant/<slug>` or `app/<tenant>`. Its own
    /// per-IP bucket, so refused SSO sign-ins never slow a password login.
    /// No account lock.
    Sso(String),
}

impl LoginScope {
    fn key(&self) -> String {
        match self {
            Self::Admin => "admin".to_owned(),
            Self::Operator => "operator".to_owned(),
            Self::Tenant(slug) => format!("tenant:{slug}"),
            Self::TenantBasic(slug) => format!("tenant-basic:{slug}"),
            Self::TenantApiKey(slug) => format!("tenant-apikey:{slug}"),
            Self::TenantAgent(slug) => format!("tenant-agent:{slug}"),
            Self::Sso(on) => format!("sso:{on}"),
        }
    }

    fn locks_accounts(&self) -> bool {
        !matches!(
            self,
            Self::TenantApiKey(_) | Self::TenantAgent(_) | Self::Sso(_)
        )
    }

    /// The per-IP bucket key. Agent secrets count apart, so a looping
    /// agent cannot spend the IP's human logins.
    fn ip_bucket(&self, ip: &str) -> String {
        match self {
            Self::TenantAgent(_) => format!("agent/{ip}"),
            Self::Sso(_) => format!("sso/{ip}"),
            _ => ip.to_owned(),
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
    /// Username and IP pairs that signed in; see [`KNOWN_CLIENT_TTL`].
    known: BoxedCache,
    in_flight: InFlight,
    /// A test's own lockout in place of the shared one.
    #[cfg(test)]
    lockout: Option<&'static crate::account_lockout::Lockout>,
}

/// How often a waiting attempt re-checks a limit held elsewhere.
const IN_FLIGHT_POLL: Duration = Duration::from_millis(50);

/// Attempts this process admitted and has not settled, per gate (an
/// account key, `ip:<ip>` or `global:<scope>`). A limit that is full only
/// because of these makes the next attempt wait, not fail.
#[derive(Default)]
struct InFlight(std::sync::Mutex<std::collections::HashMap<String, (usize, Arc<Notify>)>>);

impl InFlight {
    fn map(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, (usize, Arc<Notify>)>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn enter(&self, gate: &str) {
        self.map().entry(gate.to_owned()).or_default().0 += 1;
    }

    fn leave(&self, gate: &str) {
        let mut map = self.map();
        if let Some((n, notify)) = map.get_mut(gate) {
            *n -= 1;
            notify.notify_waiters();
            if *n == 0 {
                map.remove(gate);
            }
        }
    }

    /// `Some` while this process has attempts in flight at `gate`.
    fn busy(&self, gate: &str) -> Option<Arc<Notify>> {
        self.map().get(gate).map(|(_, notify)| Arc::clone(notify))
    }
}

/// The gates one attempt holds; leaving them on drop wakes the waiters.
struct Gates {
    buckets: Arc<Buckets>,
    held: Vec<String>,
}

impl Gates {
    fn enter(&mut self, gate: String) {
        self.buckets.in_flight.enter(&gate);
        self.held.push(gate);
    }
}

impl Drop for Gates {
    fn drop(&mut self) {
        for gate in &self.held {
            self.buckets.in_flight.leave(gate);
        }
    }
}

/// Why an attempt was not admitted.
enum Blocked {
    /// Locked, or a limit spent by settled failures.
    Refused(LoginRefused),
    /// Full at `gate`; it may free up when an attempt there settles.
    Full { gate: String, retry_after_secs: u64 },
}

impl Buckets {
    fn lockout(&self) -> &'static crate::account_lockout::Lockout {
        #[cfg(test)]
        if let Some(l) = self.lockout {
            return l;
        }
        crate::account_lockout::shared()
    }

    /// Wait for an attempt in flight at the full gate to settle; refuse
    /// when none is, and answer busy past `deadline`.
    async fn wait(
        &self,
        blocked: Blocked,
        deadline: tokio::time::Instant,
    ) -> Result<(), LoginRefused> {
        let (gate, retry_after_secs) = match blocked {
            Blocked::Refused(r) => return Err(r),
            Blocked::Full {
                gate,
                retry_after_secs,
            } => (gate, retry_after_secs),
        };
        let Some(notify) = self.in_flight.busy(&gate) else {
            return Err(LoginRefused::Throttled { retry_after_secs });
        };
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.in_flight.busy(&gate).is_none() {
            return Ok(());
        }
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return Err(LoginRefused::Busy);
        }
        let _ = tokio::time::timeout(left.min(IN_FLIGHT_POLL), notified).await;
        Ok(())
    }

    /// `true` when `username` signed in from `ip` in this scope lately.
    async fn is_known(&self, scope: &LoginScope, ip: &ClientIp, username: &str) -> bool {
        let Some(ip) = ip.0.as_deref().filter(|_| scope.locks_accounts()) else {
            return false;
        };
        let key = known_key(&account_key(&scope.key(), username), ip);
        matches!(self.known.get(&key).await, Ok(Some(_)))
    }
}

fn known_key(account_key: &str, ip: &str) -> String {
    format!("login-known:{account_key}:{ip}")
}

fn ip_gate(ip: &str) -> String {
    format!("ip:{ip}")
}

fn global_gate(scope_key: &str) -> String {
    format!("global:{scope_key}")
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
    /// Spend one token. A refused take counts nothing, so attempts that
    /// wait and retry do not fill the window themselves.
    async fn take(&self, key: &str) -> Result<Spent, u64> {
        match self {
            Self::Memory(l) => l.take(key).await.map(|_| Spent::Memory),
            Self::Cache { shared, fallback } => match shared.try_take(key).await {
                Ok(Ok((_, window))) => Ok(Spent::Window(window)),
                Ok(Err((secs, window))) => {
                    shared.give_back_at(key, window).await;
                    Err(secs)
                }
                Err(e) => {
                    tracing::warn!(target: "rustango::rate_limit", error = %e, "login limit cache incr failed");
                    fallback.take(key).await.map(|_| Spent::Memory)
                }
            },
        }
    }

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
            known: Arc::new(
                crate::cache::InMemoryCache::new().with_max_entries(KNOWN_CLIENT_MEMORY_CAP),
            ),
            in_flight: InFlight::default(),
            #[cfg(test)]
            lockout: None,
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
        let global = CacheRateLimitLayer::new(
            cache.clone(),
            limits.global_limit.max(1),
            limits.global_window,
        );
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
            known: cache,
            in_flight: InFlight::default(),
            #[cfg(test)]
            lockout: None,
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
    /// `username` is ignored for [`LoginScope::TenantApiKey`] and
    /// [`LoginScope::TenantAgent`].
    ///
    /// A limit that is full only with attempts still in flight here waits
    /// for one to settle, so right credentials are never refused by
    /// parallel successes; wrong ones still get no more than each limit.
    ///
    /// # Errors
    /// [`LoginRefused::Throttled`] when the account is locked or a limit
    /// is spent by failures; [`LoginRefused::Busy`] when in-flight
    /// attempts do not settle within [`crate::passwords::hash_wait`].
    pub async fn begin(
        &self,
        scope: &LoginScope,
        ip: &ClientIp,
        username: &str,
    ) -> Result<LoginAttempt, LoginRefused> {
        let deadline = tokio::time::Instant::now() + crate::passwords::hash_wait();
        self.begin_until(scope, ip, username, deadline).await
    }

    /// [`Self::begin`], giving up at `deadline`.
    async fn begin_until(
        &self,
        scope: &LoginScope,
        ip: &ClientIp,
        username: &str,
        deadline: tokio::time::Instant,
    ) -> Result<LoginAttempt, LoginRefused> {
        self.warn_once_on_weak_store();
        loop {
            match self.try_begin(scope, ip, username).await {
                Ok(held) => return Ok(LoginAttempt(Some(held))),
                Err(blocked) => self.0.wait(blocked, deadline).await?,
            }
        }
    }

    /// Reserve the account, per-IP and global slots in that order. A
    /// request refused by one spends nothing from the later ones.
    async fn try_begin(
        &self,
        scope: &LoginScope,
        ip: &ClientIp,
        username: &str,
    ) -> Result<Held, Blocked> {
        let b = &self.0;
        let mut held = Held {
            buckets: Arc::clone(b),
            scope_key: scope.key(),
            ip: ip.0.clone(),
            ip_bucket: ip.0.as_deref().map(|ip| scope.ip_bucket(ip)),
            spent: (Spent::Nothing, Spent::Nothing),
            locks: Vec::with_capacity(2),
            gates: Gates {
                buckets: Arc::clone(b),
                held: Vec::with_capacity(4),
            },
        };
        if scope.locks_accounts() {
            held.reserve_lock(account_key(&held.scope_key, username))
                .await?;
        }
        if let Some(ip) = held.ip_bucket.clone() {
            match b.ip.take(&ip).await {
                Ok(spent) => {
                    held.spent.0 = spent;
                    held.gates.enter(ip_gate(&ip));
                }
                Err(retry_after_secs) => {
                    held.release().await;
                    let gate = ip_gate(&ip);
                    return Err(Blocked::Full {
                        gate,
                        retry_after_secs,
                    });
                }
            }
        }
        match b.global.take(&held.scope_key).await {
            Ok(spent) => {
                held.spent.1 = spent;
                held.gates.enter(global_gate(&held.scope_key));
            }
            Err(_) if b.is_known(scope, ip, username).await => {}
            Err(retry_after_secs) => {
                let gate = global_gate(&held.scope_key);
                held.release().await;
                return Err(Blocked::Full {
                    gate,
                    retry_after_secs,
                });
            }
        }
        Ok(held)
    }

    /// Refuse, before the user lookup, what [`Self::begin`] would refuse
    /// for good: a locked account or a limit spent by failures. Reserves
    /// nothing; call [`Self::begin`] before checking the password.
    ///
    /// # Errors
    /// [`LoginRefused::Throttled`].
    pub async fn check(
        &self,
        scope: &LoginScope,
        ip: &ClientIp,
        username: &str,
    ) -> Result<(), LoginRefused> {
        let scope_key = scope.key();
        if scope.locks_accounts() {
            let lockout = self.0.lockout();
            if lockout.is_locked(&account_key(&scope_key, username)).await {
                return Err(LoginRefused::Throttled {
                    retry_after_secs: lockout.lock_duration().as_secs().max(1),
                });
            }
        }
        let b = &self.0;
        let spent = |gate: &str, r: Result<(), u64>| match r {
            Err(retry_after_secs) if b.in_flight.busy(gate).is_none() => {
                Err(LoginRefused::Throttled { retry_after_secs })
            }
            _ => Ok(()),
        };
        if let Some(ip) = ip.0.as_deref().map(|ip| scope.ip_bucket(ip)) {
            spent(&ip_gate(&ip), b.ip.peek(&ip).await)?;
        }
        let global = b.global.peek(&scope_key).await;
        if global.is_err() && b.is_known(scope, ip, username).await {
            return Ok(());
        }
        spent(&global_gate(&scope_key), global)
    }

    /// Admit a credential sent on every request (HTTP Basic, an API key)
    /// before it is compared. A known client at a clean account takes one
    /// slot on the counter that bounds guesses (the account's, else the
    /// per-IP or global one), shared by every replica on the cache and
    /// given back by a right credential; else it is reserved like a login.
    ///
    /// # Errors
    /// As [`Self::begin`].
    pub(crate) async fn begin_per_request(
        &self,
        scope: &LoginScope,
        ip: &ClientIp,
        username: &str,
    ) -> Result<PerRequestAttempt, LoginRefused> {
        let deadline = tokio::time::Instant::now() + crate::passwords::hash_wait();
        let held = match self.take_quiet(scope, ip, username).await {
            Some(slot) => PerRequestHeld::Quiet(slot),
            // Not quiet, or not counted: the login path counts, waits or refuses.
            None => {
                PerRequestHeld::Reserved(self.begin_until(scope, ip, username, deadline).await?)
            }
        };
        Ok(PerRequestAttempt {
            throttle: Self(Arc::clone(&self.0)),
            scope: scope.clone(),
            ip: ip.clone(),
            username: username.to_owned(),
            deadline,
            held,
        })
    }

    /// One atomic slot on the counter that bounds guesses here, when
    /// admitting a right credential would change nothing: a known client,
    /// room in the limits, and no lock or other attempt on the account.
    /// `None` otherwise, or when the slot could not be counted.
    async fn take_quiet(
        &self,
        scope: &LoginScope,
        ip: &ClientIp,
        username: &str,
    ) -> Option<QuietSlot> {
        let b = &self.0;
        let scope_key = scope.key();
        let ip_bucket = ip.0.as_deref().map(|ip| scope.ip_bucket(ip));
        if scope.locks_accounts() {
            if !b.is_known(scope, ip, username).await {
                return None;
            }
        } else if b.global.peek(&scope_key).await.is_err() {
            return None;
        }
        if let Some(bucket) = &ip_bucket {
            b.ip.peek(bucket).await.ok()?;
        }
        let mut gates = Gates {
            buckets: Arc::clone(b),
            held: Vec::with_capacity(1),
        };
        let counted = if scope.locks_accounts() {
            let key = account_key(&scope_key, username);
            let slot = b.lockout().reserve_sole(&key).await?;
            gates.enter(key.clone());
            Counted::Account(key, slot)
        } else if let Some(bucket) = ip_bucket {
            let spent = b.ip.take(&bucket).await.ok()?;
            gates.enter(ip_gate(&bucket));
            Counted::Ip(bucket, spent)
        } else {
            let spent = b.global.take(&scope_key).await.ok()?;
            gates.enter(global_gate(&scope_key));
            Counted::Global(scope_key, spent)
        };
        Some(QuietSlot {
            counted: Some(counted),
            gates,
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

/// One admitted attempt. Report [`Self::failed`] or [`Self::succeeded`].
/// `None` once reported; an unreported attempt (busy, DB error) is given
/// back on drop, as it checked nothing.
#[must_use = "report the outcome, or failures never lock the account"]
pub struct LoginAttempt(Option<Held>);

/// What an admitted attempt holds. Dropping it only leaves the in-process
/// gates and never spawns, so a refund that never runs (runtime shutting
/// down) cannot re-enter [`LoginAttempt`]'s `Drop`.
struct Held {
    buckets: Arc<Buckets>,
    scope_key: String,
    ip: Option<String>,
    /// The per-IP bucket key, from [`LoginScope::ip_bucket`].
    ip_bucket: Option<String>,
    /// What `begin` counted per IP and globally, refunded on success.
    spent: (Spent, Spent),
    /// Account-lock slots: the submitted name, then the stored one.
    locks: Vec<(String, ReservedSlot)>,
    /// Dropped last, after the refunds, so waiters see them.
    gates: Gates,
}

impl Drop for LoginAttempt {
    fn drop(&mut self) {
        let Some(held) = self.0.take() else {
            return;
        };
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(held.release());
        }
    }
}

impl LoginAttempt {
    fn held(&mut self) -> Held {
        self.0.take().expect("a LoginAttempt is reported once")
    }

    /// Call once the row is found, before the password check, with the
    /// username as stored. Every spelling that finds this row then shares
    /// its lock.
    ///
    /// # Errors
    /// [`LoginRefused::Throttled`] while the stored username is locked.
    pub async fn resolve(&mut self, stored_username: &str) -> Result<(), LoginRefused> {
        let Some(held) = self.0.as_mut() else {
            return Ok(());
        };
        if held.locks.is_empty() {
            return Ok(());
        }
        let key = account_key(&held.scope_key, stored_username);
        if held.locks.iter().any(|(k, _)| *k == key) {
            return Ok(());
        }
        // Waits only on attempts past `resolve`, which never wait again.
        let buckets = Arc::clone(&held.buckets);
        let deadline = tokio::time::Instant::now() + crate::passwords::hash_wait();
        loop {
            match held.reserve_lock(key.clone()).await {
                Ok(()) => return Ok(()),
                Err(blocked) => buckets.wait(blocked, deadline).await?,
            }
        }
    }

    /// Count a failure against the username, known or not. `begin`
    /// already counted it; this keeps the tokens and may lock the account.
    pub async fn failed(mut self) {
        detached(self.held().failed()).await;
    }

    /// Clear the username's failures, remember it as known from this IP,
    /// and give back the limit tokens.
    pub async fn succeeded(mut self) {
        detached(self.held().right(true)).await;
    }

    /// The password was right and the form now asks for a second
    /// factor: give back the limit tokens, keep the username's failures.
    pub async fn prompted(mut self) {
        detached(self.held().release()).await;
    }
}

/// Run a report on its own task and wait for it, so a request cancelled
/// half-way still finishes it (as [`QuietSlot::give_back`]).
async fn detached(report: impl std::future::Future<Output = ()> + Send + 'static) {
    match tokio::runtime::Handle::try_current() {
        Ok(rt) => {
            let _ = rt.spawn(report).await;
        }
        Err(_) => report.await,
    }
}

/// A per-request credential admitted by [`LoginThrottle::begin_per_request`].
/// Dropped unsettled, it gives back what it holds.
#[must_use = "settle the compare, or a miss is never counted"]
pub(crate) struct PerRequestAttempt {
    throttle: LoginThrottle,
    scope: LoginScope,
    ip: ClientIp,
    username: String,
    /// The one `hash_wait` this request gets, however often it waits.
    deadline: tokio::time::Instant,
    held: PerRequestHeld,
}

enum PerRequestHeld {
    Quiet(QuietSlot),
    Reserved(LoginAttempt),
}

/// The one counter a quiet credential took a slot on.
enum Counted {
    Account(String, ReservedSlot),
    Ip(String, Spent),
    Global(String, Spent),
}

/// A quiet credential's slot. Dropped unreported, it is given back.
struct QuietSlot {
    counted: Option<Counted>,
    /// Left after the counter, so waiters see the refund.
    gates: Gates,
}

impl Counted {
    /// The compare proved nothing, or was right: give the slot back. Failures
    /// counted meanwhile stay.
    async fn refund(self, buckets: &Buckets) {
        match self {
            Self::Account(key, slot) => buckets.lockout().release(&key, slot).await,
            Self::Ip(key, spent) => buckets.ip.give_back(&key, spent).await,
            Self::Global(key, spent) => buckets.global.give_back(&key, spent).await,
        }
    }
}

impl QuietSlot {
    /// Give the slot back and wait for it. The refund runs on its own task,
    /// so a request cancelled half-way still finishes it.
    async fn give_back(&mut self) {
        if let Some(refund) = self.spawn_give_back() {
            let _ = refund.await;
        }
    }

    /// `None` when already given back or with no runtime to run on.
    fn spawn_give_back(&mut self) -> Option<tokio::task::JoinHandle<()>> {
        let counted = self.counted.take()?;
        let buckets = Arc::clone(&self.gates.buckets);
        let gates = std::mem::replace(
            &mut self.gates,
            Gates {
                buckets: Arc::clone(&buckets),
                held: Vec::new(),
            },
        );
        let rt = tokio::runtime::Handle::try_current().ok()?;
        Some(rt.spawn(async move {
            counted.refund(&buckets).await;
            drop(gates);
        }))
    }
}

impl Drop for QuietSlot {
    fn drop(&mut self) {
        let _ = self.spawn_give_back();
    }
}

impl PerRequestAttempt {
    /// Reserve like a login now, before a check at hash cost.
    ///
    /// # Errors
    /// As [`LoginThrottle::begin`].
    #[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
    pub(crate) async fn reserve(&mut self) -> Result<(), LoginRefused> {
        if let PerRequestHeld::Quiet(slot) = &mut self.held {
            // Given back first, so the reservation is not refused by itself.
            slot.give_back().await;
            let attempt = self
                .throttle
                .begin_until(&self.scope, &self.ip, &self.username, self.deadline)
                .await?;
            self.held = PerRequestHeld::Reserved(attempt);
        }
        Ok(())
    }

    /// [`Self::reserve`], then [`LoginAttempt::resolve`] with the stored username.
    ///
    /// # Errors
    /// As [`Self::reserve`] and [`LoginAttempt::resolve`].
    #[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
    pub(crate) async fn resolve(&mut self, stored_username: &str) -> Result<(), LoginRefused> {
        self.reserve().await?;
        match &mut self.held {
            PerRequestHeld::Reserved(attempt) => attempt.resolve(stored_username).await,
            PerRequestHeld::Quiet(_) => Ok(()),
        }
    }

    /// Report the compare. A right one gives its slots back and leaves the
    /// account's failures; a quiet miss keeps its slot, counted, and counts
    /// against the other limits too.
    pub(crate) async fn settle(self, right: bool) {
        match self.held {
            PerRequestHeld::Quiet(mut slot) if right => slot.give_back().await,
            PerRequestHeld::Quiet(mut slot) => {
                let Some(counted) = slot.counted.take() else {
                    return;
                };
                let b = &self.throttle.0;
                let scope_key = self.scope.key();
                match counted {
                    Counted::Account(key, lock) => {
                        b.lockout().reserved_failed(&key, lock).await;
                        if let Some(ip) = self.ip.0.as_deref() {
                            let _ = b.ip.take(&self.scope.ip_bucket(ip)).await;
                        }
                        let _ = b.global.take(&scope_key).await;
                    }
                    Counted::Ip(..) => {
                        let _ = b.global.take(&scope_key).await;
                    }
                    Counted::Global(..) => {}
                }
            }
            PerRequestHeld::Reserved(mut attempt) if right => {
                detached(attempt.held().right(false)).await;
            }
            PerRequestHeld::Reserved(attempt) => attempt.failed().await,
        }
    }
}

impl Held {
    /// A wrong password: count it on every account lock held.
    async fn failed(mut self) {
        let lockout = self.buckets.lockout();
        for (key, slot) in self.locks.drain(..) {
            lockout.reserved_failed(&key, slot).await;
        }
    }

    /// Count the attempt against the account before the password check.
    async fn reserve_lock(&mut self, key: String) -> Result<(), Blocked> {
        let lockout = self.buckets.lockout();
        crate::account_lockout::warn_once_on_weak_store(lockout);
        match lockout.reserve(&key).await {
            Reservation::Counted(slot) => {
                self.gates.enter(key.clone());
                self.locks.push((key, slot));
                Ok(())
            }
            Reservation::Locked => Err(Blocked::Refused(LoginRefused::Throttled {
                retry_after_secs: lockout.lock_duration().as_secs().max(1),
            })),
            _ => Err(Blocked::Full {
                gate: key,
                retry_after_secs: 1,
            }),
        }
    }

    /// A right credential: remember the client, give back the tokens. Only
    /// a sign-in clears failures; a per-request credential is sent so often
    /// it would wipe a guesser's misses on the same name.
    async fn right(mut self, clears_failures: bool) {
        let lockout = self.buckets.lockout();
        for (key, slot) in std::mem::take(&mut self.locks) {
            if let Some(ip) = &self.ip {
                // Written once per pair: an existing marker is left alone.
                let (cache, known) = (&self.buckets.known, known_key(&key, ip));
                if !matches!(cache.get(&known).await, Ok(Some(_))) {
                    let _ = cache.set(&known, "1", Some(KNOWN_CLIENT_TTL)).await;
                }
            }
            if clears_failures {
                lockout.reserved_succeeded(&key, slot).await;
            } else {
                lockout.release(&key, slot).await;
            }
        }
        self.give_back().await;
    }

    /// Give back everything `begin` reserved.
    async fn release(mut self) {
        let lockout = self.buckets.lockout();
        for (key, slot) in self.locks.drain(..) {
            lockout.release(&key, slot).await;
        }
        self.give_back().await;
    }

    async fn give_back(&self) {
        let (ip_spent, global_spent) = self.spent;
        if let Some(ip) = &self.ip_bucket {
            self.buckets.ip.give_back(ip, ip_spent).await;
        }
        self.buckets
            .global
            .give_back(&self.scope_key, global_spent)
            .await;
    }
}

/// Forget the account locks of tenant `slug`'s logins, so a new tenant
/// reusing the slug does not inherit them.
///
/// # Errors
/// The lockout cache's `delete_prefix` error.
#[cfg(feature = "tenancy")]
pub(crate) async fn forget_tenant(slug: &str) -> Result<(), crate::cache::CacheError> {
    let lockout = crate::account_lockout::shared();
    for scope in [
        LoginScope::Tenant(slug.to_owned()),
        LoginScope::TenantBasic(slug.to_owned()),
    ] {
        lockout
            .clear_accounts_with_prefix(&account_key_prefix(&scope.key()))
            .await?;
    }
    Ok(())
}

/// Every [`account_key`] of `scope_key` starts with this.
fn account_key_prefix(scope_key: &str) -> String {
    format!("login:{scope_key}:")
}

/// `login:<scope>:<sha256 of the trimmed, lowercased username>`. Hashed
/// so the key has a fixed size and a shared cache never holds the name.
fn account_key(scope_key: &str, username: &str) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    let digest = Sha256::digest(username.trim().to_lowercase().as_bytes());
    let mut key = account_key_prefix(scope_key);
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

/// A test cache that counts the gate's writes.
#[cfg(test)]
pub(crate) mod counting {
    /// The cache key that counts `username`'s failures in `scope`.
    pub(crate) fn account_key(scope: &super::LoginScope, username: &str) -> String {
        super::account_key(&scope.key(), username)
    }

    /// Records the login gate's cache writes.
    #[derive(Default)]
    pub(crate) struct CountingWrites {
        inner: crate::cache::InMemoryCache,
        written: std::sync::Mutex<Vec<String>>,
        read_delay: std::time::Duration,
        write_delay: std::time::Duration,
    }

    impl CountingWrites {
        /// Every read waits `delay`, like a remote cache.
        pub(crate) fn with_read_delay(delay: std::time::Duration) -> Self {
            Self {
                read_delay: delay,
                ..Self::default()
            }
        }

        /// Every write waits `delay` first.
        pub(crate) fn with_write_delay(delay: std::time::Duration) -> Self {
            Self {
                write_delay: delay,
                ..Self::default()
            }
        }

        async fn read(&self) {
            if !self.read_delay.is_zero() {
                tokio::time::sleep(self.read_delay).await;
            }
        }

        async fn wrote(&self, k: &str) {
            if !self.write_delay.is_zero() {
                tokio::time::sleep(self.write_delay).await;
            }
            self.written
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(k.to_owned());
        }

        /// Writes so far to a key containing any of `needles`. The gate is
        /// process-global, so a test counts only its own client and account.
        pub(crate) fn writes_to(&self, needles: &[&str]) -> usize {
            self.written
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter(|k| needles.iter().any(|n| k.contains(n)))
                .count()
        }
    }

    #[async_trait::async_trait]
    impl crate::cache::Cache for CountingWrites {
        async fn get(&self, k: &str) -> Result<Option<String>, crate::cache::CacheError> {
            self.read().await;
            self.inner.get(k).await
        }
        async fn set(
            &self,
            k: &str,
            v: &str,
            ttl: Option<std::time::Duration>,
        ) -> Result<(), crate::cache::CacheError> {
            self.wrote(k).await;
            self.inner.set(k, v, ttl).await
        }
        async fn delete(&self, k: &str) -> Result<(), crate::cache::CacheError> {
            self.wrote(k).await;
            self.inner.delete(k).await
        }
        async fn exists(&self, k: &str) -> Result<bool, crate::cache::CacheError> {
            self.read().await;
            self.inner.exists(k).await
        }
        async fn clear(&self) -> Result<(), crate::cache::CacheError> {
            self.inner.clear().await
        }
        async fn incr(
            &self,
            k: &str,
            by: i64,
            ttl: Option<std::time::Duration>,
        ) -> Result<i64, crate::cache::CacheError> {
            self.wrote(k).await;
            self.inner.incr(k, by, ttl).await
        }
        async fn decr(
            &self,
            k: &str,
            by: i64,
            ttl: Option<std::time::Duration>,
        ) -> Result<i64, crate::cache::CacheError> {
            self.wrote(k).await;
            self.inner.decr(k, by, ttl).await
        }
    }
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
            LoginScope::TenantAgent("a".into()).key(),
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

    /// Many IPs spending a scope's global limit do not lock out a user who
    /// signed in from this IP before; a new IP is still limited.
    async fn known_client_passes_a_spent_global_limit(t: LoginThrottle, slug: &str, net: u8) {
        let s = tenant(slug);
        let home = ip(&format!("10.{net}.1.1"));
        t.begin(&s, &home, "alice").await.unwrap().succeeded().await;
        for n in 0..2 {
            let (from, user) = (ip(&format!("10.{net}.2.{n}")), format!("x{n}"));
            t.begin(&s, &from, &user).await.unwrap().failed().await;
        }
        let other = ip(&format!("10.{net}.3.1"));
        assert!(
            t.begin(&s, &other, "bob").await.is_err(),
            "global limit open"
        );
        assert!(t.check(&s, &home, " Alice").await.is_ok(), "check refused");
        let a = t.begin(&s, &home, "alice").await;
        a.expect("a known client was locked out").succeeded().await;
        let away = ip(&format!("10.{net}.4.1"));
        assert!(
            t.begin(&s, &away, "alice").await.is_err(),
            "new IP bypassed"
        );
    }

    #[tokio::test]
    async fn a_known_client_passes_a_spent_global_limit() {
        let limits = LoginLimits {
            global_limit: 2,
            ..LoginLimits::default()
        };
        known_client_passes_a_spent_global_limit(LoginThrottle::new(limits), "t-known", 30).await;
        let cache: BoxedCache = Arc::new(crate::cache::InMemoryCache::new());
        let t = on_cache(&cache, limits);
        known_client_passes_a_spent_global_limit(t, "t-known-c", 31).await;
    }

    /// Agent-secret failures and human logins from one IP spend separate
    /// per-IP buckets, so neither can lock the other out.
    #[tokio::test]
    async fn agent_failures_spare_human_logins_on_the_same_ip() {
        let t = LoginThrottle::new(LoginLimits {
            ip_limit: 2,
            ..LoginLimits::default()
        });
        let (agent, human) = (LoginScope::TenantAgent("t-split".into()), tenant("t-split"));
        let shared_ip = ip("10.9.40.1");
        for _ in 0..2 {
            t.begin(&agent, &shared_ip, "")
                .await
                .unwrap()
                .failed()
                .await;
        }
        assert!(t.begin(&agent, &shared_ip, "").await.is_err());
        assert!(t.check(&human, &shared_ip, "bob").await.is_ok());
        for n in 0..2 {
            let a = t.begin(&human, &shared_ip, &format!("h{n}")).await;
            a.expect("agent failures locked a human out").failed().await;
        }
        let other = ip("10.9.40.2");
        for n in 0..2 {
            let a = t.begin(&human, &other, &format!("g{n}")).await;
            a.unwrap().failed().await;
        }
        let a = t.begin(&agent, &other, "").await;
        a.expect("human failures locked an agent out")
            .succeeded()
            .await;
    }

    /// A known-client marker is written once, not on every sign-in.
    #[tokio::test]
    async fn a_known_client_marker_is_written_once() {
        let counting = Arc::new(CountingSets::default());
        let cache: BoxedCache = counting.clone();
        let t = on_cache(&cache, LoginLimits::default());
        let (s, home) = (tenant("t-mark-once"), ip("10.9.41.1"));
        for _ in 0..3 {
            t.begin(&s, &home, "carol").await.unwrap().succeeded().await;
        }
        let marks = counting
            .known_sets
            .load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(marks, 1, "the marker was rewritten");
    }

    /// An in-memory cache that counts `login-known:` writes.
    #[derive(Default)]
    struct CountingSets {
        inner: crate::cache::InMemoryCache,
        known_sets: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::cache::Cache for CountingSets {
        async fn get(&self, k: &str) -> Result<Option<String>, crate::cache::CacheError> {
            self.inner.get(k).await
        }
        async fn set(
            &self,
            k: &str,
            v: &str,
            ttl: Option<Duration>,
        ) -> Result<(), crate::cache::CacheError> {
            if k.contains("login-known:") {
                self.known_sets
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            self.inner.set(k, v, ttl).await
        }
        async fn delete(&self, k: &str) -> Result<(), crate::cache::CacheError> {
            self.inner.delete(k).await
        }
        async fn exists(&self, k: &str) -> Result<bool, crate::cache::CacheError> {
            self.inner.exists(k).await
        }
        async fn clear(&self) -> Result<(), crate::cache::CacheError> {
            self.inner.clear().await
        }
        async fn incr(
            &self,
            k: &str,
            by: i64,
            ttl: Option<Duration>,
        ) -> Result<i64, crate::cache::CacheError> {
            self.inner.incr(k, by, ttl).await
        }
        async fn decr(
            &self,
            k: &str,
            by: i64,
            ttl: Option<Duration>,
        ) -> Result<i64, crate::cache::CacheError> {
            self.inner.decr(k, by, ttl).await
        }
    }

    /// Per-request credentials keep a token only when they fail.
    #[tokio::test]
    async fn per_request_scopes_count_only_failures() {
        let t = LoginThrottle::new(LoginLimits {
            ip_limit: 2,
            ..LoginLimits::default()
        });
        let s = LoginScope::TenantApiKey("t-key".into());
        for _ in 0..5 {
            t.begin(&s, &ip("10.9.3.1"), "")
                .await
                .unwrap()
                .succeeded()
                .await;
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

    /// Start `n` attempts at once; each holds its reservation for a moment,
    /// then reports `fail` or success. Returns how many were admitted.
    async fn burst(
        t: &LoginThrottle,
        s: &LoginScope,
        ip_of: impl Fn(usize) -> ClientIp,
        user: &str,
        n: usize,
        fail: impl Fn(usize) -> bool,
    ) -> usize {
        let runs = (0..n).map(|i| {
            let (ip, fail) = (ip_of(i), fail(i));
            async move {
                let Ok(a) = t.begin(s, &ip, user).await else {
                    return 0;
                };
                tokio::time::sleep(Duration::from_millis(if fail { 1 } else { 20 })).await;
                if fail {
                    a.failed().await;
                } else {
                    a.succeeded().await;
                }
                1
            }
        });
        futures_util::future::join_all(runs).await.iter().sum()
    }

    /// #2378 — a parallel burst of wrong keys from one IP gets no more
    /// than the per-IP limit checked.
    #[tokio::test]
    async fn per_request_burst_is_capped_by_the_ip_limit() {
        let t = LoginThrottle::new(LoginLimits {
            ip_limit: 3,
            ..LoginLimits::default()
        });
        let s = LoginScope::TenantApiKey("t-burst".into());
        let n = burst(&t, &s, |_| ip("10.9.3.9"), "", 20, |_| true).await;
        assert_eq!(n, 3, "in-flight attempts must hold their token");
    }

    /// #2378 — a burst of wrong passwords at one Basic account gets
    /// `max_attempts` guesses, then the account is locked.
    #[tokio::test]
    async fn basic_burst_is_capped_by_the_account_lock() {
        let t = LoginThrottle::new(LoginLimits {
            ip_limit: 1000,
            ..LoginLimits::default()
        });
        let s = LoginScope::TenantBasic("t-burst-lock".into());
        let at = |n: usize| ip(&format!("10.9.10.{n}"));
        let n = burst(&t, &s, at, "victim", 50, |_| true).await;
        assert_eq!(
            n,
            crate::account_lockout::DEFAULT_MAX_ATTEMPTS as usize,
            "the lock check must reserve, not peek"
        );
        let r = t.begin(&s, &ip("10.9.11.1"), "victim").await;
        assert!(matches!(r, Err(LoginRefused::Throttled { .. })));
    }

    /// security-002 — parallel right credentials wait for each other;
    /// in-flight successes never refuse one.
    #[tokio::test]
    async fn parallel_successes_are_never_refused() {
        let t = LoginThrottle::new(LoginLimits {
            ip_limit: 3,
            ..LoginLimits::default()
        });
        let s = LoginScope::TenantBasic("t-par-ok".into());
        let n = burst(&t, &s, |_| ip("10.9.15.1"), "svc", 20, |_| false).await;
        assert_eq!(n, 20);
        let s = LoginScope::TenantApiKey("t-par-key".into());
        let none = |_| ClientIp::default();
        assert_eq!(
            burst(&t, &s, |_| ip("10.9.15.2"), "", 25, |_| false).await,
            25
        );
        assert_eq!(burst(&t, &s, none, "", 5, |_| false).await, 5);
    }

    /// security-002 — one wrong password among in-flight successes
    /// counts once; it does not lock the account.
    #[tokio::test]
    async fn one_failure_in_a_good_burst_does_not_lock() {
        let t = LoginThrottle::new(LoginLimits::default());
        let s = LoginScope::TenantBasic("t-one-bad".into());
        let n = burst(&t, &s, |_| ip("10.9.16.1"), "svc2", 5, |i| i == 0).await;
        assert_eq!(n, 5);
        let a = t.begin(&s, &ip("10.9.16.1"), "svc2").await;
        assert!(a.is_ok(), "one failure locked the account");
    }

    /// A wrong burst on the cache-backed limits is capped too, and the
    /// refused retries do not fill the window themselves.
    #[tokio::test]
    async fn cache_backed_burst_is_capped_and_successes_wait() {
        let cache: BoxedCache = Arc::new(crate::cache::InMemoryCache::new());
        let t = on_cache(
            &cache,
            LoginLimits {
                ip_limit: 3,
                ..LoginLimits::default()
            },
        );
        let s = LoginScope::TenantApiKey("t-cache-burst".into());
        assert_eq!(
            burst(&t, &s, |_| ip("10.9.17.1"), "", 20, |_| false).await,
            20
        );
        assert_eq!(
            burst(&t, &s, |_| ip("10.9.17.1"), "", 20, |_| true).await,
            3
        );
    }

    /// An attempt dropped unreported (busy, DB error) gives its
    /// reservations back instead of counting toward the lock.
    #[tokio::test]
    async fn an_unreported_attempt_is_given_back() {
        let t = LoginThrottle::new(LoginLimits {
            ip_limit: 1,
            ..LoginLimits::default()
        });
        let s = tenant("t-drop");
        for _ in 0..10 {
            drop(t.begin(&s, &ip("10.9.13.1"), "dropper").await.unwrap());
            // The refund runs on a spawned task.
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
        }
    }

    /// tests-001 — an attempt dropped while its runtime shuts down must
    /// not recurse through `Drop` (stack overflow).
    #[test]
    fn an_attempt_dropped_at_runtime_shutdown_does_not_recurse() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let t = LoginThrottle::new(LoginLimits::default());
            let a = t.begin(&tenant("t-shutdown"), &ip("10.9.14.1"), "u").await;
            let a = a.unwrap();
            tokio::spawn(async move {
                let _held = a;
                std::future::pending::<()>().await;
            });
            tokio::task::yield_now().await;
        });
        drop(rt);
    }

    /// Succeeding attempts give the account reservation back.
    #[tokio::test]
    async fn successes_free_the_account_reservation() {
        let t = LoginThrottle::new(LoginLimits::default());
        let s = tenant("t-lock-ok");
        for _ in 0..20 {
            t.begin(&s, &ip("10.9.12.1"), "regular")
                .await
                .unwrap()
                .succeeded()
                .await;
        }
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

    /// Two replicas on one shared cache, as a load balancer spreads them.
    fn replicas(limits: LoginLimits) -> [Arc<LoginThrottle>; 2] {
        let cache: BoxedCache = Arc::new(crate::cache::InMemoryCache::new());
        [
            Arc::new(LoginThrottle::with_cache(limits, cache.clone())),
            Arc::new(LoginThrottle::with_cache(limits, cache)),
        ]
    }

    /// Per-request guesses spread over `replicas`, each compared for 30 ms
    /// then reported wrong; how many were compared, refused or not after.
    async fn per_request_burst(
        replicas: &[Arc<LoginThrottle>; 2],
        s: &LoginScope,
        ip: &ClientIp,
        user: &str,
        n: usize,
    ) -> usize {
        let compared = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let runs: Vec<_> = (0..n)
            .map(|i| {
                let (t, s, ip, user) = (
                    Arc::clone(&replicas[i % 2]),
                    s.clone(),
                    ip.clone(),
                    user.to_owned(),
                );
                let compared = Arc::clone(&compared);
                tokio::spawn(async move {
                    let Ok(a) = t.begin_per_request(&s, &ip, &user).await else {
                        return;
                    };
                    compared.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    a.settle(false).await;
                })
            })
            .collect();
        for r in runs {
            r.await.unwrap();
        }
        compared.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// A known client's Basic guesses over two replicas: the account lock
    /// bounds every compare, including ones answered 429 after it.
    #[tokio::test]
    async fn per_request_guesses_over_replicas_are_bounded_by_the_lock() {
        let r = replicas(LoginLimits::default());
        let (s, client) = (
            LoginScope::TenantBasic("t-repl-lock".into()),
            ip("10.9.70.1"),
        );
        let a = r[0].begin(&s, &client, "bob").await.unwrap();
        a.succeeded().await;
        let compared = per_request_burst(&r, &s, &client, "bob", 30).await;
        let max = crate::account_lockout::shared().attempt_limit() as usize;
        assert!(compared <= max, "{compared} guesses were compared");
    }

    /// API-key guesses over two replicas: the per-IP limit bounds every compare.
    #[tokio::test]
    async fn per_request_keys_over_replicas_are_bounded_by_the_ip_limit() {
        // Windows align to the clock: a burst over a window edge gets 2x.
        // A decade-long window keeps the edge out of the burst.
        let r = replicas(LoginLimits {
            ip_limit: 3,
            ip_window: Duration::from_secs(10 * 365 * 24 * 3600),
            ..LoginLimits::default()
        });
        let s = LoginScope::TenantApiKey("t-repl-ip".into());
        let compared = per_request_burst(&r, &s, &ip("10.9.70.2"), "", 30).await;
        assert!(compared <= 3, "{compared} keys were compared");
    }

    /// A wrong guess counted after a quiet right one was admitted stays
    /// counted: a client polling Basic must not wipe a guesser's misses.
    #[tokio::test]
    async fn a_right_per_request_credential_keeps_a_failure_counted_meanwhile() {
        let t = LoginThrottle::new(LoginLimits::default());
        let (s, client) = (
            LoginScope::TenantBasic("t-quiet-keep".into()),
            ip("10.9.70.3"),
        );
        let a = t.begin(&s, &client, "carol").await.unwrap();
        a.succeeded().await;
        let right = t.begin_per_request(&s, &client, "carol").await.unwrap();
        let wrong = t.begin(&s, &client, "carol").await.unwrap();
        wrong.failed().await;
        right.settle(true).await;
        let key = account_key(&s.key(), "carol");
        assert_eq!(
            crate::account_lockout::shared().attempt_count(&key).await,
            1
        );
    }

    /// With a failure already counted the credential is reserved like a
    /// login; a right one still leaves the failure.
    #[tokio::test]
    async fn a_right_per_request_credential_keeps_an_earlier_failure() {
        let t = LoginThrottle::new(LoginLimits::default());
        let (s, client) = (
            LoginScope::TenantBasic("t-reserved-keep".into()),
            ip("10.9.70.5"),
        );
        t.begin(&s, &client, "erin").await.unwrap().failed().await;
        for _ in 0..3 {
            let right = t.begin_per_request(&s, &client, "erin").await.unwrap();
            right.settle(true).await;
        }
        let key = account_key(&s.key(), "erin");
        assert_eq!(
            crate::account_lockout::shared().attempt_count(&key).await,
            1
        );
    }

    /// A request cancelled while its right credential's slot is refunded
    /// still gives the slot back.
    #[tokio::test]
    async fn a_cancelled_refund_still_gives_the_slot_back() {
        let cache: BoxedCache = Arc::new(counting::CountingWrites::with_read_delay(
            Duration::from_millis(100),
        ));
        let t = on_cache(
            &cache,
            LoginLimits {
                ip_limit: 1,
                ..LoginLimits::default()
            },
        );
        let (s, client) = (LoginScope::TenantApiKey("t-cancel".into()), ip("10.9.70.6"));
        let a = t.begin_per_request(&s, &client, "").await.unwrap();
        assert!(matches!(a.held, PerRequestHeld::Quiet(_)));
        let _ = tokio::time::timeout(Duration::from_millis(30), a.settle(true)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let bucket = s.ip_bucket("10.9.70.6");
        assert!(t.0.ip.peek(&bucket).await.is_ok(), "the slot leaked");
    }

    /// A login form request cancelled while a right password's or a second-factor
    /// prompt's tokens are refunded still gives them back.
    #[tokio::test]
    async fn a_cancelled_login_refund_still_gives_the_slot_back() {
        for (n, prompt) in [(7, false), (8, true)] {
            let cache: BoxedCache = Arc::new(counting::CountingWrites::with_read_delay(
                Duration::from_millis(100),
            ));
            let t = on_cache(
                &cache,
                LoginLimits {
                    ip_limit: 1,
                    ..LoginLimits::default()
                },
            );
            let (s, addr) = (
                tenant(&format!("t-cancel-login-{n}")),
                format!("10.9.70.{n}"),
            );
            let a = t.begin(&s, &ip(&addr), "dana").await.unwrap();
            let report = async move {
                if prompt {
                    a.prompted().await;
                } else {
                    a.succeeded().await;
                }
            };
            let _ = tokio::time::timeout(Duration::from_millis(30), report).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            let bucket = s.ip_bucket(&addr);
            assert!(
                t.0.ip.peek(&bucket).await.is_ok(),
                "the slot leaked (prompted: {prompt})"
            );
        }
    }

    /// A login form request cancelled while its wrong password is counted
    /// still counts it, so cancelling cannot dodge the account lock.
    #[tokio::test]
    async fn a_cancelled_login_failure_is_still_counted() {
        let mut t = LoginThrottle::new(LoginLimits::default());
        let slow: BoxedCache = Arc::new(counting::CountingWrites::with_write_delay(
            Duration::from_millis(100),
        ));
        let lockout: &'static _ = Box::leak(Box::new(
            crate::account_lockout::Lockout::new(slow).max_attempts(1),
        ));
        Arc::get_mut(&mut t.0).expect("unshared").lockout = Some(lockout);
        let s = tenant("t-cancel-failed");
        let a = t.begin(&s, &ip("10.9.70.9"), "gil").await.unwrap();
        let _ = tokio::time::timeout(Duration::from_millis(30), a.failed()).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            lockout.is_locked(&account_key(&s.key(), "gil")).await,
            "the failure was dropped"
        );
    }

    /// Counts every call; with `fail`, every call errors.
    #[derive(Default)]
    struct CountingCalls {
        inner: crate::cache::InMemoryCache,
        calls: std::sync::atomic::AtomicUsize,
        fail: bool,
    }

    impl CountingCalls {
        fn call(&self) -> Result<(), crate::cache::CacheError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail {
                return Err(crate::cache::CacheError::Connection("down".into()));
            }
            Ok(())
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl crate::cache::Cache for CountingCalls {
        async fn get(&self, k: &str) -> Result<Option<String>, crate::cache::CacheError> {
            self.call()?;
            self.inner.get(k).await
        }
        async fn set(
            &self,
            k: &str,
            v: &str,
            ttl: Option<Duration>,
        ) -> Result<(), crate::cache::CacheError> {
            self.call()?;
            self.inner.set(k, v, ttl).await
        }
        async fn add(
            &self,
            k: &str,
            v: &str,
            ttl: Option<Duration>,
        ) -> Result<bool, crate::cache::CacheError> {
            self.call()?;
            self.inner.add(k, v, ttl).await
        }
        async fn delete(&self, k: &str) -> Result<(), crate::cache::CacheError> {
            self.call()?;
            self.inner.delete(k).await
        }
        async fn exists(&self, k: &str) -> Result<bool, crate::cache::CacheError> {
            self.call()?;
            self.inner.exists(k).await
        }
        async fn clear(&self) -> Result<(), crate::cache::CacheError> {
            self.inner.clear().await
        }
        async fn incr(
            &self,
            k: &str,
            by: i64,
            ttl: Option<Duration>,
        ) -> Result<i64, crate::cache::CacheError> {
            self.call()?;
            self.inner.incr(k, by, ttl).await
        }
        async fn decr(
            &self,
            k: &str,
            by: i64,
            ttl: Option<Duration>,
        ) -> Result<i64, crate::cache::CacheError> {
            self.call()?;
            self.inner.decr(k, by, ttl).await
        }
    }

    /// `t` counting account locks in `cache` instead of the shared lockout.
    fn with_lockout(mut t: LoginThrottle, cache: Arc<CountingCalls>) -> LoginThrottle {
        let lockout = crate::account_lockout::Lockout::new(cache);
        Arc::get_mut(&mut t.0).expect("unshared").lockout = Some(Box::leak(Box::new(lockout)));
        t
    }

    /// A lockout store that errors counts nothing, so a known client's
    /// credential takes the full reservation, per-IP slot included.
    #[tokio::test]
    async fn an_uncounted_lock_slot_is_not_quiet() {
        let down = Arc::new(CountingCalls {
            fail: true,
            ..CountingCalls::default()
        });
        let t = with_lockout(
            LoginThrottle::new(LoginLimits {
                ip_limit: 1,
                ..LoginLimits::default()
            }),
            down,
        );
        let (s, client) = (
            LoginScope::TenantBasic("t-lock-down".into()),
            ip("10.9.70.7"),
        );
        t.begin(&s, &client, "fay").await.unwrap().succeeded().await;
        let a = t.begin_per_request(&s, &client, "fay").await.unwrap();
        assert!(matches!(a.held, PerRequestHeld::Reserved(_)));
        let bucket = s.ip_bucket("10.9.70.7");
        assert!(t.0.ip.peek(&bucket).await.is_err(), "no per-IP slot taken");
        a.settle(true).await;
    }

    /// A known client's right credential: few cache calls, and the slot
    /// comes back.
    #[tokio::test]
    async fn a_right_per_request_credential_costs_six_cache_calls() {
        let calls = Arc::new(CountingCalls::default());
        let cache: BoxedCache = calls.clone();
        let t = with_lockout(on_cache(&cache, LoginLimits::default()), calls.clone());
        let (s, client) = (LoginScope::TenantBasic("t-six".into()), ip("10.9.70.8"));
        t.begin(&s, &client, "gil").await.unwrap().succeeded().await;
        let before = calls.calls();
        let a = t.begin_per_request(&s, &client, "gil").await.unwrap();
        assert!(matches!(a.held, PerRequestHeld::Quiet(_)));
        a.settle(true).await;
        let spent = calls.calls() - before;
        assert!(spent <= 6, "{spent} cache calls");
        let key = account_key(&s.key(), "gil");
        assert_eq!(t.0.lockout().attempt_count(&key).await, 0);
    }

    /// A per-request credential waits at most one `hash_wait` before a 503.
    #[tokio::test]
    async fn a_per_request_credential_waits_one_deadline() {
        let t = Arc::new(LoginThrottle::new(LoginLimits::default()));
        let (s, client) = (
            LoginScope::TenantBasic("t-one-wait".into()),
            ip("10.9.70.4"),
        );
        let a = t.begin(&s, &client, "dan").await.unwrap();
        a.succeeded().await;
        let max = crate::account_lockout::shared().attempt_limit() as usize;
        let mut held = Vec::new();
        for _ in 0..max {
            held.push(t.begin_per_request(&s, &client, "dan").await.unwrap());
        }
        let wait = crate::passwords::hash_wait();
        let started = tokio::time::Instant::now();
        let late = {
            let (t, s, client) = (Arc::clone(&t), s.clone(), client.clone());
            tokio::spawn(async move { t.begin_per_request(&s, &client, "dan").await.is_ok() })
        };
        // Plain attempts fill the lock while the late one waits.
        tokio::time::sleep(wait / 2).await;
        let mut plain = Vec::new();
        for _ in 0..max {
            let (t, s, client) = (Arc::clone(&t), s.clone(), client.clone());
            plain.push(tokio::spawn(async move {
                let a = t.begin(&s, &client, "dan").await;
                tokio::time::sleep(wait * 3).await;
                drop(a);
            }));
        }
        tokio::time::sleep(wait * 2 / 5).await;
        drop(held.pop());
        let _ = late.await.unwrap();
        let took = started.elapsed();
        assert!(
            took < wait * 3 / 2,
            "waited {took:?} for a {wait:?} deadline"
        );
        drop(held);
        for p in plain {
            p.abort();
        }
    }
}
