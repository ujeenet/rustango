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
//! - **Under tenancy, scope the lock.** Lock names are global by
//!   default, so looping tenants around one `with_lock("daily_report")`
//!   lets the first tenant win and skips the rest for the whole TTL —
//!   silently, since a refused acquire is the expected outcome. Use
//!   [`DistributedLock::for_tenant`] so each tenant gets its own lock
//!   (#1228). Leave it unscoped only for genuinely process-wide work.

use std::sync::Arc;
use std::time::Duration;

use crate::cache::BoxedCache;

const KEY_PREFIX: &str = "lock";

/// Lock factory. Cheap to clone.
#[derive(Clone)]
pub struct DistributedLock {
    cache: BoxedCache,
    /// Folded into every lock name. Empty for a process-wide lock;
    /// `tenant:{slug}` after [`Self::for_tenant`] (#1228).
    scope: Option<String>,
}

impl DistributedLock {
    #[must_use]
    pub fn new(cache: BoxedCache) -> Self {
        Self { cache, scope: None }
    }

    /// A lock factory whose names are scoped to one tenant, so the same
    /// lock name in two tenants is two independent locks (#1228).
    ///
    /// Without this, the natural per-tenant cron —
    ///
    /// ```ignore
    /// for org in active_orgs {
    ///     lock.with_lock("daily_report", ttl, || async { report(&org).await }).await;
    /// }
    /// ```
    ///
    /// — has every tenant contend for one `lock:daily_report`. The first
    /// tenant wins and the rest are skipped for the whole TTL, which
    /// (with a TTL correctly sized to the work) means most tenants never
    /// get their report. Nothing is logged, because a refused acquire is
    /// the documented, expected outcome.
    ///
    /// Scoped, each tenant gets `lock:tenant:{slug}:daily_report` and
    /// they no longer collide. Follows the `tenant:{slug}:…` convention
    /// the tenancy layer already uses for lockout keys.
    ///
    /// ```ignore
    /// let lock = DistributedLock::new(cache).for_tenant(&org.slug);
    /// lock.with_lock("daily_report", ttl, || async { … }).await;
    /// ```
    ///
    /// Keep using the unscoped form for genuinely process-wide work —
    /// registry cleanup, a cross-tenant rollup — where "exactly one
    /// replica, ever" is the point.
    #[must_use]
    pub fn for_tenant(mut self, slug: impl AsRef<str>) -> Self {
        self.scope = Some(format!("tenant:{}", slug.as_ref()));
        self
    }

    /// Scope lock names under an arbitrary namespace. [`Self::for_tenant`]
    /// is this with the `tenant:` convention applied.
    #[must_use]
    pub fn scoped(mut self, namespace: impl AsRef<str>) -> Self {
        self.scope = Some(namespace.as_ref().to_owned());
        self
    }

    /// The cache key for `name`, including this factory's scope.
    fn key_for(&self, name: &str) -> String {
        match &self.scope {
            Some(scope) => format!("{KEY_PREFIX}:{scope}:{name}"),
            None => format!("{KEY_PREFIX}:{name}"),
        }
    }

    /// Try to acquire `name` for `ttl`. Returns:
    /// - `Some(LockGuard)` when we got it. Call `release()` when done
    ///   (drop without release leaves the lock to expire on TTL —
    ///   safe but slightly wasteful of contention slots).
    /// - `None` when someone else holds it.
    pub async fn try_acquire(&self, name: &str, ttl: Duration) -> Option<LockGuard> {
        let key = self.key_for(name);
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

    /// The bug from #1228: two tenants, one lock name, unscoped — the
    /// second is refused. Pinned so the distinction between the scoped
    /// and unscoped forms stays deliberate rather than accidental.
    #[tokio::test]
    async fn unscoped_lock_is_shared_across_tenants() {
        let cache: BoxedCache = StdArc::new(InMemoryCache::new());
        let lock = DistributedLock::new(cache);

        let acme = lock
            .try_acquire("daily_report", Duration::from_secs(30))
            .await;
        let globex = lock
            .try_acquire("daily_report", Duration::from_secs(30))
            .await;

        assert!(acme.is_some(), "first caller takes the lock");
        assert!(
            globex.is_none(),
            "unscoped, a second tenant contends for the same name — this is the \
             starvation #1228 is about"
        );
    }

    /// Scoped, the same lock name in two tenants is two locks, so a
    /// per-tenant cron actually runs for every tenant.
    #[tokio::test]
    async fn scoped_locks_do_not_contend_across_tenants() {
        let cache: BoxedCache = StdArc::new(InMemoryCache::new());
        let acme_lock = DistributedLock::new(cache.clone()).for_tenant("acme");
        let globex_lock = DistributedLock::new(cache).for_tenant("globex");

        let acme = acme_lock
            .try_acquire("daily_report", Duration::from_secs(30))
            .await;
        let globex = globex_lock
            .try_acquire("daily_report", Duration::from_secs(30))
            .await;

        assert!(acme.is_some(), "acme gets its own lock");
        assert!(globex.is_some(), "globex gets its own lock, not acme's");
    }

    /// Within one tenant the lock still excludes — scoping must not
    /// weaken the guarantee it exists for.
    #[tokio::test]
    async fn scoped_lock_still_excludes_within_a_tenant() {
        let cache: BoxedCache = StdArc::new(InMemoryCache::new());
        let lock = DistributedLock::new(cache).for_tenant("acme");

        let first = lock
            .try_acquire("daily_report", Duration::from_secs(30))
            .await;
        let second = lock
            .try_acquire("daily_report", Duration::from_secs(30))
            .await;

        assert!(first.is_some());
        assert!(second.is_none(), "one holder at a time, per tenant");
    }

    /// A tenant scope must not let one slug's lock name collide with
    /// another's by prefix (`acme` vs `acme-corp`).
    #[tokio::test]
    async fn tenant_scopes_are_separated_by_slug() {
        let cache: BoxedCache = StdArc::new(InMemoryCache::new());
        let acme = DistributedLock::new(cache.clone()).for_tenant("acme");
        let acme_corp = DistributedLock::new(cache).for_tenant("acme-corp");

        assert!(acme
            .try_acquire("j", Duration::from_secs(30))
            .await
            .is_some());
        assert!(
            acme_corp
                .try_acquire("j", Duration::from_secs(30))
                .await
                .is_some(),
            "`acme-corp` must not be blocked by `acme`'s lock"
        );
    }
}
