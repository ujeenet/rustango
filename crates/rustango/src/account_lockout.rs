//! Per-account login lockout — defends against credential stuffing /
//! brute-force attacks that bypass per-IP rate limits.
//!
//! Backed by the cache layer (in-memory or Redis). Each failed login
//! increments a counter; once it crosses the threshold, the account is
//! locked for a configurable duration. Successful logins clear the counter.
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
//! ## Why per-account, not per-IP?
//!
//! Per-IP rate limiting (`RateLimitLayer::per_ip`) catches one attacker
//! pounding one endpoint. Per-account lockout catches a botnet trying
//! the same username from thousands of IPs — the *account* is the rate
//! axis. Both belong in your stack.

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
    /// When the count reaches `max_attempts`, the account is locked
    /// for `lockout_duration`.
    pub async fn record_failure(&self, account: &str) -> u32 {
        let counter_key = self.counter_key(account);
        let current: u32 = self
            .cache
            .get(&counter_key)
            .await
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let next = current + 1;
        let _ = self
            .cache
            .set(&counter_key, &next.to_string(), Some(self.counter_ttl))
            .await;
        if next >= self.max_attempts {
            // Set the lock flag with TTL = lockout_duration
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

    fn counter_key(&self, account: &str) -> String {
        format!("{}attempts:{}", self.key_prefix, account)
    }

    fn lock_key(&self, account: &str) -> String {
        format!("{}locked:{}", self.key_prefix, account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::InMemoryCache;

    fn lockout(max: u32) -> Lockout {
        let cache: Arc<dyn Cache> = Arc::new(InMemoryCache::new());
        Lockout::new(cache).max_attempts(max).lockout_duration(Duration::from_secs(60))
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
        let l1 = Lockout::new(cache.clone()).key_prefix("login:").max_attempts(2);
        let l2 = Lockout::new(cache).key_prefix("mfa:").max_attempts(2);
        l1.record_failure("alice").await;
        l1.record_failure("alice").await;
        assert!(l1.is_locked("alice").await);
        assert!(!l2.is_locked("alice").await, "MFA namespace shouldn't be locked");
    }

    #[tokio::test]
    async fn max_attempts_floors_at_1() {
        let l = lockout(0);
        l.record_failure("alice").await;
        assert!(l.is_locked("alice").await, "max_attempts(0) should be treated as 1");
    }
}
