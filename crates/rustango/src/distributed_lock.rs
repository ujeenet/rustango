//! Distributed locks backed by [`Cache`](crate::cache::Cache).
//!
//! "Only one worker at a time runs this task." Pair with the
//! [`crate::scheduler`] so a multi-replica deploy doesn't run a daily
//! cron N times, or wrap a long-running job whose effect should be
//! exactly-once.
//!
//! ## How it works
//!
//! Acquiring writes `lock:<name>` with a token, using one atomic
//! set-if-absent (`Cache::add`). The key expires after `ttl`, so a
//! crash while holding the lock cannot deadlock the system: the next
//! caller waits at most `ttl`.
//!
//! Releasing first checks the token, so a process whose TTL ran out
//! does not free the lock the next holder now owns.
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
//! // Or use the closure form, which releases when the body ends:
//! lock.with_lock("rebuild_index", Duration::from_secs(60), || async {
//!     rebuild_index().await;
//! }).await;
//!
//! // From a scheduler: once per UTC day across every pod. Tick often;
//! // the first tick of each day runs it. `every(day)` alone can miss a
//! // day across a deploy, and `with_lock` runs it once per pod.
//! let day = Duration::from_secs(86_400);
//! scheduler.every("daily_report", Duration::from_secs(60), move || {
//!     let lock = lock.clone();
//!     async move {
//!         // `run_daily_report` returns a `Result`; an `Err` retries next tick.
//!         lock.once_per_period("daily_report", day, || run_daily_report()).await;
//!     }
//! });
//! ```
//!
//! ## Warnings
//!
//! - **The lock is advisory and best-effort.** It suits cron-style
//!   work. It is not a replacement for a transaction where
//!   correctness matters. One race remains: release reads the key and
//!   then deletes it, so a lock whose TTL ran out during release could
//!   be freed just as the next holder takes it. A compare-and-delete
//!   script would close that window.
//! - **Across replicas use Redis or the database.** `RedisCache` does
//!   `SET NX` and `DatabaseCache::add` is one conditional write, so
//!   either picks a single winner between machines. `InMemoryCache`
//!   holds its own lock, so it is only safe inside one process.
//! - **Set `ttl` above the worst-case run time of the guarded work**,
//!   or make that work idempotent. With a short TTL another replica
//!   can take the lock while the first is still running.
//! - **Under tenancy, scope the lock.** Names are global by default,
//!   so looping over tenants with one `with_lock("daily_report")`
//!   lets the first tenant win and skips the rest for a whole TTL,
//!   and nothing is logged, because a refused acquire is normal. Use
//!   [`DistributedLock::for_tenant`] to give each tenant its own
//!   lock. Stay unscoped only for process-wide work.
//!
//! [`DistributedLock::for_tenant`]: crate::distributed_lock::DistributedLock::for_tenant

use std::sync::Arc;
use std::time::Duration;

use crate::cache::BoxedCache;

const KEY_PREFIX: &str = "lock";

/// Lock factory. Cheap to clone.
#[derive(Clone)]
pub struct DistributedLock {
    cache: BoxedCache,
    /// Prefix on every lock name. `None` is process-wide;
    /// [`Self::for_tenant`] sets `tenant:{slug}`.
    scope: Option<String>,
}

impl DistributedLock {
    #[must_use]
    pub fn new(cache: BoxedCache) -> Self {
        Self { cache, scope: None }
    }

    /// Scope every lock name to one tenant, so the same name in two
    /// tenants gives two separate locks.
    ///
    /// Use this for any per-tenant job. Unscoped, a loop over tenants
    /// makes them all contend for one `lock:daily_report`: the first
    /// wins and the rest are skipped for a whole TTL, without a log
    /// line. Scoped, each gets `lock:tenant:{slug}:daily_report`.
    ///
    /// ```ignore
    /// let lock = DistributedLock::new(cache).for_tenant(&org.slug);
    /// lock.with_lock("daily_report", ttl, || async { … }).await;
    /// ```
    ///
    /// Stay unscoped only when "one replica, ever" is the point, such
    /// as registry cleanup or a cross-tenant rollup.
    #[must_use]
    pub fn for_tenant(mut self, slug: impl AsRef<str>) -> Self {
        self.scope = Some(format!("tenant:{}", slug.as_ref()));
        self
    }

    /// Scope lock names under any namespace. [`Self::for_tenant`] is
    /// this with a `tenant:` prefix.
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

    /// Try to take `name` for `ttl`. Gives `None` when someone else
    /// holds it, or when the cache is unreachable.
    ///
    /// Call [`LockGuard::release`] when the work is done. Dropping
    /// the guard instead is safe, but the lock then stays taken until
    /// the TTL runs out.
    pub async fn try_acquire(&self, name: &str, ttl: Duration) -> Option<LockGuard> {
        let key = self.key_for(name);
        // The whole acquire is one atomic set-if-absent: `add` writes
        // the key only when it is absent and says whether it did. The
        // key's value IS the token, so there is no second write that
        // could fall out of step with it.
        let token = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        );
        match self.cache.add(&key, &token, Some(ttl)).await {
            Ok(true) => Some(LockGuard {
                cache: self.cache.clone(),
                key,
                token: Arc::new(token),
                released: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }),
            // `Ok(false)`: another holder. `Err`: the cache is down.
            // Both fail closed — skip the work rather than run it
            // unguarded.
            _ => None,
        }
    }

    /// Run `body` only if we get the lock, then release it. Gives
    /// `Some(R)` when the body ran and `None` when another holder
    /// blocked it. If the body panics, the lock is left to its TTL,
    /// because `Drop` cannot await the delete.
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

    /// Run `body` at most once per `period` across every caller sharing
    /// the cache. Use this, not [`Self::with_lock`], for a scheduled job,
    /// and tick well inside the period (say every 60s).
    ///
    /// Windows count from the Unix epoch, so a daily period starts at
    /// 00:00 UTC and pods booted apart agree on them. A finished run keeps
    /// the window's lock, so later ticks in that window skip. A body that
    /// returns `Err` or panics frees it, so a later tick retries.
    ///
    /// While `body` runs it also holds the plain `name` lock (TTL one
    /// period), so a run that outlasts its window is not overlapped by the
    /// next one, provided `body` ends within a period. A run dropped
    /// mid-way, such as an aborted task, frees both.
    pub async fn once_per_period<F, Fut, T, E>(
        &self,
        name: &str,
        period: Duration,
        body: F,
    ) -> Option<Result<T, E>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
    {
        let period = period.max(Duration::from_millis(1));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let window = now.as_millis() / period.as_millis();
        self.once_in_window(name, period, window, body).await
    }

    /// [`Self::once_per_period`] for a window the caller already computed.
    pub(crate) async fn once_in_window<F, Fut, T, E>(
        &self,
        name: &str,
        period: Duration,
        window: u128,
        body: F,
    ) -> Option<Result<T, E>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
    {
        // Twice the period, so a pod whose clock lags still sees the key.
        let guard = self
            .try_acquire(&format!("{name}@{window}"), period.saturating_mul(2))
            .await?;
        // The key two windows back is past any skew; drop it so a database
        // cache does not keep one row per window forever.
        if let Some(old) = window.checked_sub(2) {
            let _ = self
                .cache
                .delete(&self.key_for(&format!("{name}@{old}")))
                .await;
        }
        // A run from the last window may still be going on another pod.
        // Free this window so a later tick retries once it ends.
        let Some(running) = self.try_acquire(name, period).await else {
            guard.release().await;
            return None;
        };
        // Frees both locks if this future is dropped mid-run (an aborted task).
        let mut held = ReleaseOnDrop(Some((guard, running)));
        let result = crate::panic_guard::catch_unwind(async move { body().await }).await;
        let (guard, running) = held.0.take().expect("held until here");
        running.release().await;
        match result {
            Ok(Ok(v)) => {
                guard.keep_until_ttl();
                Some(Ok(v))
            }
            Ok(Err(e)) => {
                guard.release().await;
                Some(Err(e))
            }
            Err(panic) => {
                guard.release().await;
                std::panic::resume_unwind(panic)
            }
        }
    }
}

/// Releases the window and run locks when a run is dropped before it ends.
struct ReleaseOnDrop(Option<(LockGuard, LockGuard)>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        // Best effort: without a runtime the TTLs free them.
        if let (Some((window, running)), Ok(rt)) =
            (self.0.take(), tokio::runtime::Handle::try_current())
        {
            rt.spawn(async move {
                running.release().await;
                window.release().await;
            });
        }
    }
}

/// Holds a lock until released or until the TTL expires.
pub struct LockGuard {
    cache: BoxedCache,
    key: String,
    token: Arc<String>,
    released: Arc<std::sync::atomic::AtomicBool>,
}

impl LockGuard {
    /// Release the lock, if we still hold it. Calling it again does
    /// nothing.
    pub async fn release(self) {
        self.release_inner().await;
    }

    /// Drop the guard without releasing: the TTL frees the lock.
    fn keep_until_ttl(self) {
        self.released
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    async fn release_inner(&self) {
        if self
            .released
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        // Delete only if the stored value is still our token. If our
        // TTL ran out and someone else took the lock, the value is
        // theirs and we must not free it. Get-then-delete leaves a
        // small window; a compare-and-delete script would close it.
        let stored = self.cache.get(&self.key).await.ok().flatten();
        if stored.as_deref() == Some(self.token.as_str()) {
            let _ = self.cache.delete(&self.key).await;
        }
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // Drop cannot await the delete, so the TTL frees the lock.
        // Log it, since the caller probably meant to release.
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

    /// Under contention exactly one of many racing callers may win.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn only_one_of_many_racing_acquirers_wins() {
        let cache: BoxedCache = StdArc::new(InMemoryCache::new());
        let l = StdArc::new(DistributedLock::new(cache));
        let mut handles = Vec::new();
        for _ in 0..50 {
            let l = l.clone();
            handles.push(tokio::spawn(async move {
                l.try_acquire("hot", Duration::from_secs(30))
                    .await
                    .map(|g| {
                        // Never release, so a second winner would be
                        // a real double-acquire.
                        std::mem::forget(g);
                    })
                    .is_some()
            }));
        }
        let mut winners = 0;
        for h in handles {
            if h.await.unwrap() {
                winners += 1;
            }
        }
        assert_eq!(winners, 1, "exactly one acquirer must win; got {winners}");
    }

    /// A released lock can be taken again right away, every time.
    #[tokio::test]
    async fn lock_is_reacquirable_after_release() {
        let l = lock();
        for _ in 0..5 {
            let g = l.try_acquire("cycle", Duration::from_secs(5)).await;
            assert!(g.is_some(), "should re-acquire after each release");
            g.unwrap().release().await;
        }
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
        // `release` consumes the guard, so test the inner method,
        // which is the one that can be called twice.
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
        // Forget the guard so only the TTL can free the lock.
        std::mem::forget(g);
        tokio::time::sleep(Duration::from_millis(120)).await;
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
        // g1 releases late; it must not clear g2's lock.
        g1.release().await;
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

    /// A finished period run keeps its lock, so a caller later in the
    /// same window skips (#2330).
    #[tokio::test]
    async fn once_per_period_holds_the_window_after_the_body() {
        let l = lock();
        let day = Duration::from_secs(86_400);
        let ok = |n: i32| async move { Ok::<_, ()>(n) };
        assert_eq!(l.once_per_period("j", day, || ok(1)).await, Some(Ok(1)));
        assert_eq!(l.once_per_period("j", day, || ok(2)).await, None);
    }

    fn window_now(period: Duration) -> u128 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        now.as_millis() / period.as_millis()
    }

    /// An `Err` or a panic frees the window, so a later tick retries.
    #[tokio::test]
    async fn once_per_period_retries_after_err_or_panic() {
        let l = lock();
        let day = Duration::from_secs(86_400);
        let failed = l.once_per_period("j", day, || async { Err::<(), _>("x") });
        assert_eq!(failed.await, Some(Err("x")));
        let l2 = l.clone();
        let panicked = tokio::spawn(async move {
            l2.once_per_period("j", day, || async {
                if day > Duration::ZERO {
                    panic!("boom");
                }
                Ok::<(), ()>(())
            })
            .await
        });
        assert!(panicked.await.unwrap_err().is_panic());
        let ok = l.once_per_period("j", day, || async { Ok::<_, ()>(1) });
        assert_eq!(ok.await, Some(Ok(1)), "the window stayed taken");
        // The plain name lock is free again too.
        assert!(l.try_acquire("j", day).await.is_some());
    }

    /// The window key outlives the period (clock skew) and the key two
    /// windows back is deleted.
    #[tokio::test]
    async fn once_per_period_keys_ttl_and_cleanup() {
        let cache: BoxedCache = StdArc::new(InMemoryCache::new());
        let l = DistributedLock::new(cache.clone());
        let period = Duration::from_millis(100);
        let w = loop {
            let w = window_now(period);
            for old in [w - 2, w - 1] {
                cache
                    .set(&format!("lock:t@{old}"), "old", None)
                    .await
                    .unwrap();
            }
            let r = l.once_per_period("t", period, || async { Ok::<_, ()>(()) });
            assert!(r.await.is_some());
            if window_now(period) == w {
                break w;
            }
        };
        assert!(!cache.exists(&format!("lock:t@{}", w - 2)).await.unwrap());
        assert!(
            cache.exists(&format!("lock:t@{}", w - 1)).await.unwrap(),
            "the last window's key is kept for skew"
        );
        tokio::time::sleep(Duration::from_millis(140)).await;
        assert!(
            cache.exists(&format!("lock:t@{w}")).await.unwrap(),
            "TTL shorter than twice the period"
        );
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(
            !cache.exists(&format!("lock:t@{w}")).await.unwrap(),
            "TTL longer than twice the period"
        );
    }

    /// A zero period is clamped, not a division by zero.
    #[tokio::test]
    async fn once_per_period_clamps_a_zero_period() {
        let l = lock();
        let r = l.once_per_period("z", Duration::ZERO, || async { Ok::<_, ()>(()) });
        assert_eq!(r.await, Some(Ok(())));
    }

    /// Sleep until `offset` into the next `period` window.
    async fn sleep_to_offset(period: Duration, offset: Duration) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let into = (now % period.as_millis()) as u64;
        let wait = period.as_millis() as u64 - into + offset.as_millis() as u64;
        tokio::time::sleep(Duration::from_millis(wait)).await;
    }

    /// A run that crosses into the next window is not overlapped by that
    /// window's run; the next window runs once it ends.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn once_per_period_does_not_overlap_a_long_run() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let period = Duration::from_millis(200);
        let cache: BoxedCache = StdArc::new(InMemoryCache::new());
        let (a, b) = (
            DistributedLock::new(cache.clone()),
            DistributedLock::new(cache),
        );
        let running = StdArc::new(AtomicBool::new(false));
        let (overlaps, b_runs) = (
            StdArc::new(AtomicUsize::new(0)),
            StdArc::new(AtomicUsize::new(0)),
        );
        sleep_to_offset(period, Duration::from_millis(100)).await;
        let r = running.clone();
        let pod_a = tokio::spawn(async move {
            a.once_per_period("long", period, || async {
                r.store(true, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(150)).await;
                r.store(false, Ordering::SeqCst);
                Ok::<_, ()>(())
            })
            .await
        });
        let end = std::time::Instant::now() + Duration::from_millis(240);
        while std::time::Instant::now() < end {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let (r, o, n) = (running.clone(), overlaps.clone(), b_runs.clone());
            b.once_per_period("long", period, || async move {
                if r.load(Ordering::SeqCst) {
                    o.fetch_add(1, Ordering::SeqCst);
                }
                n.fetch_add(1, Ordering::SeqCst);
                Ok::<_, ()>(())
            })
            .await;
        }
        assert!(pod_a.await.unwrap().is_some(), "pod A never ran");
        assert_eq!(overlaps.load(Ordering::SeqCst), 0, "two runs overlapped");
        assert_eq!(
            b_runs.load(Ordering::SeqCst),
            1,
            "the next window ran {} times",
            b_runs.load(Ordering::SeqCst)
        );
    }

    /// While another pod's run holds the name lock, a tick frees the window
    /// it took, so a later tick in the same window runs.
    #[tokio::test]
    async fn a_busy_run_lock_leaves_the_window_for_a_later_tick() {
        let l = lock();
        let day = Duration::from_secs(86_400);
        let other = l.try_acquire("j", day).await.expect("another pod's run");
        let blocked = l.once_per_period("j", day, || async { Ok::<_, ()>(1) });
        assert_eq!(blocked.await, None);
        other.release().await;
        let later = l.once_per_period("j", day, || async { Ok::<_, ()>(2) });
        assert_eq!(later.await, Some(Ok(2)), "the window stayed taken");
    }

    /// An aborted run (scheduler shutdown) frees both locks, so a later tick
    /// in the same window runs.
    #[tokio::test]
    async fn an_aborted_run_frees_its_window() {
        let l = lock();
        let day = Duration::from_secs(86_400);
        let (started_tx, started) = tokio::sync::oneshot::channel();
        let l2 = l.clone();
        let task = tokio::spawn(async move {
            l2.once_per_period("j", day, || async move {
                let _ = started_tx.send(());
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok::<_, ()>(())
            })
            .await
        });
        started.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        // The release runs on a spawned task.
        let mut again = None;
        for _ in 0..50 {
            again = l
                .once_per_period("j", day, || async { Ok::<_, ()>(()) })
                .await;
            if again.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(again, Some(Ok(())), "the aborted run kept the window");
    }

    /// A body that panics before its first await still frees the window.
    #[tokio::test]
    async fn a_body_panicking_on_call_frees_the_window() {
        let l = lock();
        let day = Duration::from_secs(86_400);
        let l2 = l.clone();
        let r = tokio::spawn(async move {
            l2.once_per_period("j", day, || -> std::future::Ready<Result<(), ()>> {
                panic!("before the future")
            })
            .await
        });
        assert!(r.await.unwrap_err().is_panic());
        let again = l.once_per_period("j", day, || async { Ok::<_, ()>(()) });
        assert_eq!(again.await, Some(Ok(())));
    }

    /// Unscoped, two tenants share one lock name and the second is
    /// refused. Pinned so the scoped/unscoped split stays on purpose.
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
            "unscoped, a second tenant contends for the same name and starves"
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

    /// Scoping must not weaken the guarantee inside a tenant.
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

    /// One slug must not collide with another by prefix, such as
    /// `acme` against `acme-corp`.
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

    /// A tenant cache clear must not free a lock a running job holds.
    #[tokio::test]
    async fn tenant_cache_clear_keeps_tenant_locks() {
        use crate::cache::{Cache as _, ScopedCache};
        let cache: BoxedCache = StdArc::new(InMemoryCache::new());
        let acme = DistributedLock::new(cache.clone()).for_tenant("acme");
        let ttl = Duration::from_secs(30);
        let _held = acme.try_acquire("j", ttl).await.expect("acme");

        ScopedCache::for_tenant(cache, "acme")
            .clear()
            .await
            .unwrap();

        assert!(acme.try_acquire("j", ttl).await.is_none(), "lock freed");
    }

    /// `scoped` names its own keyspace, apart from unscoped and other scopes.
    #[tokio::test]
    async fn scoped_locks_use_their_namespace() {
        let cache: BoxedCache = StdArc::new(InMemoryCache::new());
        let ttl = Duration::from_secs(30);
        let ns = DistributedLock::new(cache.clone()).scoped("reports");
        let _held = ns.try_acquire("j", ttl).await.expect("scoped");

        assert!(cache.exists("lock:reports:j").await.unwrap());
        assert!(
            ns.try_acquire("j", ttl).await.is_none(),
            "same scope contends"
        );
        let other = DistributedLock::new(cache.clone()).scoped("billing");
        assert!(other.try_acquire("j", ttl).await.is_some());
        let unscoped = DistributedLock::new(cache);
        assert!(unscoped.try_acquire("j", ttl).await.is_some());
    }
}
