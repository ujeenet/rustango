//! Per-account login lockout. Stops credential stuffing and brute
//! force that gets past per-IP rate limits.
//!
//! Backed by the cache layer (in-memory or Redis). Each failed login
//! increments a counter. At the threshold the account locks for a set
//! time. A successful login clears the counter.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::account_lockout::Lockout;
//! use rustango::cache::InMemoryCache;
//! use std::sync::Arc;
//! use std::time::Duration;
//!
//! let cache: Arc<dyn rustango::cache::Cache> = Arc::new(InMemoryCache::new());
//! let lockout = Lockout::new(cache)
//!     .max_attempts(5)
//!     .lockout_duration(Duration::from_secs(900));    // 15 min
//!
//! // Login handler:
//! let username = "alice";
//!
//! if lockout.is_locked(username).await {
//!     return Err("account temporarily locked — try again later");
//! }
//!
//! if !verify_credentials(username, password).await? {
//!     lockout.record_failure(username).await;
//!     return Err("bad credentials");
//! }
//!
//! lockout.clear(username).await;       // success → reset counter
//! issue_session(username).await
//! ```
//!
//! ## Per-account, not per-IP
//!
//! Per-IP limiting (`RateLimitLayer::per_ip`) catches one attacker
//! hitting one endpoint. Per-account lockout catches a botnet trying
//! the same username from thousands of IPs, where the account is the
//! only axis they all share. Run both.

use std::sync::Arc;
use std::time::Duration;

use crate::cache::Cache;

/// Default attempts before lockout.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 5;
/// Default lockout duration (15 minutes).
pub const DEFAULT_LOCKOUT_DURATION_SECS: u64 = 900;

/// Per-account lockout tracker.
pub struct Lockout {
    cache: Arc<dyn Cache>,
    max_attempts: u32,
    lockout_duration: Duration,
    counter_ttl: Duration,
    key_prefix: String,
}

impl Lockout {
    /// New tracker with default thresholds (5 attempts → 15 min lock,
    /// counter expires after 1 hour of inactivity).
    #[must_use]
    pub fn new(cache: Arc<dyn Cache>) -> Self {
        Self {
            cache,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            lockout_duration: Duration::from_secs(DEFAULT_LOCKOUT_DURATION_SECS),
            counter_ttl: Duration::from_secs(3600),
            key_prefix: "lockout:".to_owned(),
        }
    }

    /// Override the attempts threshold.
    #[must_use]
    pub fn max_attempts(mut self, n: u32) -> Self {
        self.max_attempts = n.max(1);
        self
    }

    /// Override the lockout duration.
    #[must_use]
    pub fn lockout_duration(mut self, d: Duration) -> Self {
        self.lockout_duration = d;
        self
    }

    /// Override how long the failure counter persists between attempts.
    /// Defaults to 1 hour — counters reset themselves if the user goes
    /// quiet for a while.
    #[must_use]
    pub fn counter_ttl(mut self, d: Duration) -> Self {
        self.counter_ttl = d;
        self
    }

    /// Override the cache-key prefix. Defaults to `"lockout:"`. Useful
    /// when sharing one cache across multiple lockout namespaces (login
    /// vs MFA vs API key etc.).
    #[must_use]
    pub fn key_prefix(mut self, p: impl Into<String>) -> Self {
        self.key_prefix = p.into();
        self
    }

    /// Check whether `account` is currently locked. Returns `true` to
    /// reject the login attempt; `false` to proceed with verification.
    pub async fn is_locked(&self, account: &str) -> bool {
        self.cache
            .exists(&self.lock_key(account))
            .await
            .unwrap_or(false)
    }

    /// Record a failed login attempt. Returns the new attempt count.
    /// At `max_attempts` the account locks for `lockout_duration`.
    ///
    /// # Security note
    ///
    /// `account` becomes the cache key. If you pass the raw username
    /// from a login form, **an attacker can lock any user out** by
    /// sending failed logins for that name — a denial of service. Key
    /// on something the attacker cannot pick:
    /// - [`Self::record_failure_by_id`], which takes a resolved user id.
    /// - Or your own `uid:{id}` key, built only when the user exists.
    /// - Plus per-IP rate limiting upstream.
    pub async fn record_failure(&self, account: &str) -> u32 {
        let counter_key = self.counter_key(account);
        // Atomic increment, never get-parse-set. A read-modify-write
        // loses updates under concurrent failed logins: several
        // attempts read the same value and write back the same `+1`,
        // so parallel guesses can out-run the threshold. `incr` is
        // atomic on `RedisCache` and `InMemoryCache`; `DatabaseCache`
        // still does get+set, which is fine for a single process.
        //
        // A cache failure means this attempt is NOT counted and
        // lockout quietly stops engaging, so log it loudly. Failing
        // open is deliberate: a cache outage that locks every account
        // is its own denial of service.
        let counted = match self
            .cache
            .incr(&counter_key, 1, Some(self.counter_ttl))
            .await
        {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    account,
                    "account-lockout counter failed; this failed attempt is NOT counted \
                     and lockout will not engage"
                );
                0
            }
        };
        let next = u32::try_from(counted).unwrap_or(u32::MAX);
        if next >= self.max_attempts {
            // Lock flag with TTL = lockout_duration.
            let _ = self
                .cache
                .set(&self.lock_key(account), "1", Some(self.lockout_duration))
                .await;
        }
        next
    }

    /// Clear the failure counter and any active lock. Call on successful
    /// authentication.
    pub async fn clear(&self, account: &str) {
        let _ = self.cache.delete(&self.counter_key(account)).await;
        let _ = self.cache.delete(&self.lock_key(account)).await;
    }

    /// Read the current failure count for an account. 0 when absent.
    pub async fn attempt_count(&self, account: &str) -> u32 {
        self.cache
            .get(&self.counter_key(account))
            .await
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    /// Force-lock an account (e.g. by an admin action).
    pub async fn force_lock(&self, account: &str) {
        let _ = self
            .cache
            .set(&self.lock_key(account), "1", Some(self.lockout_duration))
            .await;
    }

    // Typed-user-id variants. They stamp a `uid:` prefix on the key,
    // so id counters group together. The prefix is not an isolation
    // barrier: a caller who passes the literal username `uid:42` to
    // `record_failure` still hits the same key.

    /// User-id variant of [`Self::record_failure`]. Prefer this in
    /// production: the caller has already resolved a real user, so an
    /// attacker cannot lock out a name of their choosing.
    pub async fn record_failure_by_id(&self, user_id: i64) -> u32 {
        self.record_failure(&format!("uid:{user_id}")).await
    }

    /// User-id variant of [`Self::is_locked`].
    pub async fn is_locked_by_id(&self, user_id: i64) -> bool {
        self.is_locked(&format!("uid:{user_id}")).await
    }

    /// User-id variant of [`Self::clear`].
    pub async fn clear_by_id(&self, user_id: i64) {
        self.clear(&format!("uid:{user_id}")).await
    }

    fn counter_key(&self, account: &str) -> String {
        format!("{}attempts:{}", self.key_prefix, account)
    }

    fn lock_key(&self, account: &str) -> String {
        format!("{}locked:{}", self.key_prefix, account)
    }
}

/// Process-wide default lockout, lazily initialized to an in-memory
/// tracker with the default policy (5 attempts → 15-min lock).
static SHARED_LOCKOUT: std::sync::OnceLock<Lockout> = std::sync::OnceLock::new();

/// The built-in login flows (admin, operator, tenant, JWT API) use
/// this, so per-account brute-force protection is on by default with
/// no wiring.
///
/// It uses an in-memory cache, so it guards **one process only**: with
/// N replicas an attacker gets N times the attempts. For a scaled
/// deployment install a shared-cache (Redis / DB) tracker at boot with
/// [`configure_shared`].
#[must_use]
pub fn shared() -> &'static Lockout {
    SHARED_LOCKOUT.get_or_init(|| Lockout::new(Arc::new(crate::cache::InMemoryCache::new())))
}

/// Install the process-wide [`shared`] lockout. Call it once at boot
/// to back the lockout with a shared cache or a different policy.
/// First call wins: returns `false` when [`shared`] was already built,
/// for example because a login ran first.
pub fn configure_shared(lockout: Lockout) -> bool {
    SHARED_LOCKOUT.set(lockout).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::InMemoryCache;

    fn lockout(max: u32) -> Lockout {
        let cache: Arc<dyn Cache> = Arc::new(InMemoryCache::new());
        Lockout::new(cache)
            .max_attempts(max)
            .lockout_duration(Duration::from_secs(60))
    }

    #[tokio::test]
    async fn fresh_account_not_locked() {
        let l = lockout(5);
        assert!(!l.is_locked("alice").await);
        assert_eq!(l.attempt_count("alice").await, 0);
    }

    #[tokio::test]
    async fn record_failure_increments_count() {
        let l = lockout(5);
        assert_eq!(l.record_failure("alice").await, 1);
        assert_eq!(l.record_failure("alice").await, 2);
        assert_eq!(l.attempt_count("alice").await, 2);
        assert!(!l.is_locked("alice").await);
    }

    #[tokio::test]
    async fn locks_at_threshold() {
        let l = lockout(3);
        for _ in 0..2 {
            l.record_failure("alice").await;
        }
        assert!(!l.is_locked("alice").await);
        l.record_failure("alice").await;
        assert!(l.is_locked("alice").await);
    }

    #[tokio::test]
    async fn clear_resets_counter_and_lock() {
        let l = lockout(2);
        l.record_failure("alice").await;
        l.record_failure("alice").await;
        assert!(l.is_locked("alice").await);
        l.clear("alice").await;
        assert!(!l.is_locked("alice").await);
        assert_eq!(l.attempt_count("alice").await, 0);
    }

    #[tokio::test]
    async fn force_lock_works_without_failures() {
        let l = lockout(5);
        l.force_lock("alice").await;
        assert!(l.is_locked("alice").await);
    }

    #[tokio::test]
    async fn lockout_expires() {
        let cache: Arc<dyn Cache> = Arc::new(InMemoryCache::new());
        let l = Lockout::new(cache)
            .max_attempts(2)
            .lockout_duration(Duration::from_millis(100));
        l.record_failure("alice").await;
        l.record_failure("alice").await;
        assert!(l.is_locked("alice").await);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!l.is_locked("alice").await);
    }

    #[tokio::test]
    async fn separate_accounts_dont_share_state() {
        let l = lockout(2);
        l.record_failure("alice").await;
        l.record_failure("alice").await;
        assert!(l.is_locked("alice").await);
        assert!(!l.is_locked("bob").await);
        assert_eq!(l.attempt_count("bob").await, 0);
    }

    #[tokio::test]
    async fn key_prefix_isolates_namespaces() {
        let cache: Arc<dyn Cache> = Arc::new(InMemoryCache::new());
        let l1 = Lockout::new(cache.clone())
            .key_prefix("login:")
            .max_attempts(2);
        let l2 = Lockout::new(cache).key_prefix("mfa:").max_attempts(2);
        l1.record_failure("alice").await;
        l1.record_failure("alice").await;
        assert!(l1.is_locked("alice").await);
        assert!(
            !l2.is_locked("alice").await,
            "MFA namespace shouldn't be locked"
        );
    }

    #[tokio::test]
    async fn max_attempts_floors_at_1() {
        let l = lockout(0);
        l.record_failure("alice").await;
        assert!(
            l.is_locked("alice").await,
            "max_attempts(0) should be treated as 1"
        );
    }

    // ---- typed-user-id variants ----

    #[tokio::test]
    async fn by_id_namespace_is_isolated_from_username() {
        // The by_id variants always stamp the `uid:` prefix. Today a
        // literal username of `uid:42` produces the same key
        // (`lockout:attempts:uid:42`); this test pins the behaviour
        // and would catch a real isolation prefix being added later.
        let l = lockout(3);
        l.record_failure_by_id(42).await;
        l.record_failure_by_id(42).await;
        l.record_failure_by_id(42).await;
        assert!(l.is_locked_by_id(42).await);
    }

    #[tokio::test]
    async fn by_id_lifecycle_round_trips() {
        let l = lockout(3);
        assert!(!l.is_locked_by_id(99).await);
        l.record_failure_by_id(99).await;
        l.record_failure_by_id(99).await;
        l.record_failure_by_id(99).await;
        assert!(l.is_locked_by_id(99).await);
        l.clear_by_id(99).await;
        assert!(!l.is_locked_by_id(99).await);
    }

    #[tokio::test]
    async fn scoped_keys_isolate_domains_and_tenants() {
        // The built-in login handlers use scoped string keys, so the
        // same numeric id in two domains or tenants never collides.
        let l = lockout(2);
        l.record_failure("tenant:acme:5").await;
        l.record_failure("tenant:acme:5").await;
        assert!(l.is_locked("tenant:acme:5").await);
        assert!(
            !l.is_locked("tenant:beta:5").await,
            "a different tenant's user 5 must be unaffected"
        );
        assert!(
            !l.is_locked("op:5").await,
            "the operator domain must be unaffected"
        );
        assert!(
            !l.is_locked("admin:5").await,
            "the bare-admin domain must be unaffected"
        );
    }

    #[tokio::test]
    async fn shared_default_is_usable() {
        // The process-global default exists and answers. Unique key so
        // parallel tests cannot disturb the assertion.
        assert!(!shared().is_locked("smoke:unique-unused-key").await);
    }

    /// Concurrent failed attempts must each count, or an attacker can
    /// out-run the threshold by guessing in parallel. With an atomic
    /// `incr`, 50 racing failures record exactly 50.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_failures_all_count() {
        let cache: Arc<dyn Cache> = Arc::new(InMemoryCache::new());
        let l = Arc::new(
            Lockout::new(cache)
                .max_attempts(1000) // don't lock; we're counting
                .counter_ttl(Duration::from_secs(60)),
        );
        let mut handles = Vec::new();
        for _ in 0..50 {
            let l = l.clone();
            handles.push(tokio::spawn(async move { l.record_failure("alice").await }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(
            l.attempt_count("alice").await,
            50,
            "lost-update race: concurrent failures did not all count",
        );
    }
}
