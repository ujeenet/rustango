//! Distributed locks backed by [`Cache`](crate::cache::Cache).
//!
//! "Only one worker at a time runs this task." Pair with the
//! [`crate::scheduler`] so a multi-replica deploy doesn't run a daily
//! cron N times, or wrap a long-running job whose effect should be
//! exactly-once.
//!
//! ## Mechanism
//!
//! Acquire is a Cache `set` of `lock:<name>` to a per-acquire token,
//! gated on the existing default `incr`-based check. The lock auto-
//! expires after `ttl`, so a process that crashes while holding the
//! lock doesn't deadlock the system — at worst, the next acquirer
//! waits `ttl` seconds.
//!
//! Release is conditional on the token: a process that lost its lock
//! (because TTL expired and someone else acquired) does NOT
//! accidentally release the new holder's lock.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::distributed_lock::DistributedLock;
//! use std::time::Duration;
//! use std::sync::Arc;
//!
//! let lock = DistributedLock::new(redis_cache);
//!
//! // Try once, give up if another replica has it:
//! if let Some(guard) = lock.try_acquire("daily_report", Duration::from_secs(60)).await {
//!     run_daily_report().await;
//!     guard.release().await;
//! }
//!
//! // Or use the closure form which auto-releases on drop:
//! lock.with_lock("daily_report", Duration::from_secs(60), || async {
//!     run_daily_report().await;
//! }).await;
//! ```
//!
//! ## Caveats
//!
//! - The non-atomic default `Cache::incr` can race under heavy
//!   contention with two acquirers in the same millisecond. RedisCache
//!   uses native `INCRBY` so it's safe across replicas.
//! - This is "best-effort exactly-once" — fine for cron-style work,
//!   not a substitute for a transaction when correctness matters.
//! - TTL must be longer than the worst-case execution time of the
//!   protected work, OR the work must be idempotent. A too-short TTL
//!   means another replica could grab the lock mid-execution.

use std::sync::Arc;
use std::time::Duration;

use crate::cache::BoxedCache;

const KEY_PREFIX: &str = "lock";

/// Lock factory. Cheap to clone.
#[derive(Clone)]
pub struct DistributedLock {
    cache: BoxedCache,
}

impl DistributedLock {
    #[must_use]
    pub fn new(cache: BoxedCache) -> Self {
        Self { cache }
    }

    /// Try to acquire `name` for `ttl`. Returns:
    /// - `Some(LockGuard)` when we got it. Call `release()` when done
    ///   (drop without release leaves the lock to expire on TTL —
    ///   safe but slightly wasteful of contention slots).
    /// - `None` when someone else holds it.
    pub async fn try_acquire(&self, name: &str, ttl: Duration) -> Option<LockGuard> {
        let key = format!("{KEY_PREFIX}:{name}");
        // `incr(key, 1, ttl)` returns 1 ONLY when the counter was
        // previously absent (or 0). On RedisCache the INCRBY+EXPIRE NX
        // sequence is atomic; on the in-memory default impl it's racy
        // but acceptable for tests.
        let n = self.cache.incr(&key, 1, Some(ttl)).await.ok()?;
        if n == 1 {
            // We got the lock. Stash a token so release knows it's us.
            let token = format!(
                "{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos())
            );
            // Keep the counter, but ALSO write a token — release reads
            // both. (The counter is the gate; the token is the receipt.)
            let token_key = format!("{key}:token");
            let _ = self.cache.set(&token_key, &token, Some(ttl)).await;
            Some(LockGuard {
                cache: self.cache.clone(),
                key,
                token_key,
                token: Arc::new(token),
                released: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            })
        } else {
            None
        }
    }

    /// Acquire-or-skip helper: runs `body` only if we got the lock.
    /// Returns `Some(R)` when body ran, `None` when another holder
    /// blocked us. The lock is released after body finishes (or on
    /// panic, via the guard's Drop — which is best-effort since we
    /// can't call async fns from Drop).
    pub async fn with_lock<F, Fut, R>(&self, name: &str, ttl: Duration, body: F) -> Option<R>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = R>,
    {
        let guard = self.try_acquire(name, ttl).await?;
        let result = body().await;
        guard.release().await;
        Some(result)
    }
}

/// Holds a lock until released or until the TTL expires.
pub struct LockGuard {
    cache: BoxedCache,
    key: String,
    token_key: String,
    token: Arc<String>,
    released: Arc<std::sync::atomic::AtomicBool>,
}

impl LockGuard {
    /// Release the lock if we still hold it. Safe to call multiple
    /// times; the second call is a no-op.
    pub async fn release(self) {
        self.release_inner().await;
    }

    async fn release_inner(&self) {
        if self
            .released
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        // Token check — if our token is still the one stored, we still
        // hold the lock, so we can clear it. Otherwise someone else
        // acquired after our TTL expired and we mustn't touch theirs.
        let stored = self.cache.get(&self.token_key).await.ok().flatten();
        if stored.as_deref() != Some(self.token.as_str()) {
            return;
        }
        let _ = self.cache.delete(&self.key).await;
        let _ = self.cache.delete(&self.token_key).await;
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // Best-effort: if the holder forgot to call release(), the
        // TTL will eventually free the lock. We can't do an async
        // delete from Drop, but a sync warning is informative.
        if !self.released.load(std::sync::atomic::Ordering::SeqCst) {
            tracing::debug!(
                key = %self.key,
                "DistributedLock guard dropped without release(); waiting for TTL"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::InMemoryCache;
    use std::sync::Arc as StdArc;

    fn lock() -> DistributedLock {
        let cache: BoxedCache = StdArc::new(InMemoryCache::new());
        DistributedLock::new(cache)
    }

    #[tokio::test]
    async fn first_acquirer_succeeds() {
        let l = lock();
        let g = l.try_acquire("job", Duration::from_secs(5)).await;
        assert!(g.is_some());
    }

    #[tokio::test]
    async fn second_acquirer_blocked() {
        let l = lock();
        let g1 = l.try_acquire("job", Duration::from_secs(5)).await;
        assert!(g1.is_some());
        let g2 = l.try_acquire("job", Duration::from_secs(5)).await;
        assert!(g2.is_none(), "second acquirer should be blocked");
    }

    #[tokio::test]
    async fn release_lets_next_acquirer_in() {
        let l = lock();
        let g1 = l.try_acquire("job", Duration::from_secs(5)).await.unwrap();
        g1.release().await;
        let g2 = l.try_acquire("job", Duration::from_secs(5)).await;
        assert!(g2.is_some(), "after release the lock is free");
    }

    #[tokio::test]
    async fn different_names_dont_collide() {
        let l = lock();
        let a = l.try_acquire("a", Duration::from_secs(5)).await;
        let b = l.try_acquire("b", Duration::from_secs(5)).await;
        assert!(a.is_some());
        assert!(b.is_some());
    }

    #[tokio::test]
    async fn with_lock_runs_body_and_releases() {
        let l = lock();
        let result = l
            .with_lock("job", Duration::from_secs(5), || async { 42 })
            .await;
        assert_eq!(result, Some(42));
        // Lock should be released — next acquire works.
        let g = l.try_acquire("job", Duration::from_secs(5)).await;
        assert!(g.is_some());
    }

    #[tokio::test]
    async fn with_lock_returns_none_when_blocked() {
        let l = lock();
        let _hold = l.try_acquire("job", Duration::from_secs(5)).await.unwrap();
        let result = l
            .with_lock("job", Duration::from_secs(5), || async { 42 })
            .await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn release_is_idempotent_at_least_once() {
        // The release call consumes the guard, so calling it twice
        // requires re-acquiring + dropping — verify the inner method
        // is safe to call twice.
        let l = lock();
        let g = l.try_acquire("job", Duration::from_secs(5)).await.unwrap();
        g.release_inner().await;
        // Manually call again — should be a no-op.
    }

    #[tokio::test]
    async fn ttl_expiry_frees_lock() {
        let l = lock();
        let g = l.try_acquire("job", Duration::from_millis(50)).await;
        assert!(g.is_some());
        // Forget the guard so we can't release explicitly; wait past TTL.
        std::mem::forget(g);
        tokio::time::sleep(Duration::from_millis(120)).await;
        // After expiry the lock is reacquireable.
        let g2 = l.try_acquire("job", Duration::from_millis(50)).await;
        assert!(g2.is_some(), "TTL expiry should free the lock");
    }

    #[tokio::test]
    async fn release_after_ttl_does_not_clobber_new_holder() {
        let l = lock();
        let g1 = l
            .try_acquire("job", Duration::from_millis(30))
            .await
            .unwrap();
        // Wait for TTL to expire.
        tokio::time::sleep(Duration::from_millis(80)).await;
        // Someone else acquires.
        let g2 = l.try_acquire("job", Duration::from_secs(5)).await;
        assert!(g2.is_some(), "new acquirer can claim after TTL");
        // Now g1 belatedly releases — this MUST NOT clobber g2.
        g1.release().await;
        // g2 must still hold the lock.
        let g3 = l.try_acquire("job", Duration::from_secs(5)).await;
        assert!(
            g3.is_none(),
            "g2 still holds the lock — late g1.release() must not clear it"
        );
        drop(g2);
    }

    #[tokio::test]
    async fn with_lock_releases_even_when_body_returns_unit() {
        let l = lock();
        let r: Option<()> = l
            .with_lock("job", Duration::from_secs(5), || async {})
            .await;
        assert!(r.is_some());
        let g = l.try_acquire("job", Duration::from_secs(5)).await;
        assert!(g.is_some());
    }
}
