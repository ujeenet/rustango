//! The cache-and-poll apparatus tenant resolution runs on, once.
//!
//! Three shapes each existed twice in `resolver.rs`:
//!
//! * a bounded, negatively-caching TTL map keyed on a client-supplied
//!   `Host` — once for base hosts, once for extra hostnames;
//! * a throttled fingerprint poll that drops that map when *another*
//!   process writes — once for `rustango_orgs`, once for
//!   `rustango_org_hosts`;
//! * a failure breaker that stops a broken dependency from costing one
//!   doomed query per request — once for the registry, once for the
//!   host table.
//!
//! The copies drifted almost immediately: one cache expired at `>= ttl`
//! and the other at `> ttl`, and a documented collision caveat existed
//! on one and not the other. That is the argument for this module —
//! every fix here would otherwise have to be found and applied in both
//! places, and the second one is easy to miss.
//!
//! Every type is `const`-constructible so it can live in a `static`
//! without a lazy wrapper.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

/// A latch that lets one message through for the life of the process.
///
/// Both the polls and the breakers need to say "this is broken" without
/// saying it once per request — or, just as bad, once per poll interval
/// for as long as the outage lasts. A dead mechanism that logs nothing is
/// invisible until it produces a symptom nobody can correlate; a dead
/// mechanism that logs every request buries the rest of the log.
pub(super) struct WarnOnce(AtomicBool);

impl WarnOnce {
    pub(super) const fn new() -> Self {
        Self(AtomicBool::new(false))
    }

    /// Run `emit` the first time this is called, and never again.
    pub(super) fn once(&self, emit: impl FnOnce()) {
        if !self.0.swap(true, Ordering::Relaxed) {
            emit();
        }
    }
}

/// What a cache lookup found.
///
/// Named rather than `Option<Option<V>>` because "not cached" and
/// "cached as a miss" lead to opposite actions — one queries, the other
/// must not.
pub(super) enum Cached<V> {
    /// Nothing usable; go to the registry.
    Absent,
    /// Known not to resolve to anything.
    Miss,
    /// Carries the value. Deliberately not boxed: this is the hot path
    /// the cache exists to make free, and a box would add an allocation
    /// to every hit. The enum is a short-lived local, destructured
    /// immediately, so its size costs a stack move, not a heap trip.
    Hit(V),
}

/// A bounded, TTL'd, negatively-caching map from hostname to `V`.
///
/// ## Why two bounded maps, with LRU eviction
///
/// The keys are `Host` header values, so an attacker picks them. Misses
/// live in their own, smaller map, so random hosts (wildcard DNS) can
/// never push a resolved tenant out. Each map evicts its least recently
/// read entry at the cap, never the whole map. [`Self::single_flight`]
/// lets one caller per host query on a cold or expired entry.
///
/// Callers must pass an already-normalised key. Hostnames are
/// case-insensitive (RFC 4343), so `ACME.example` and `acme.example`
/// must not be two entries for one tenant.
pub(super) struct HostTtlCache<V> {
    /// `Option` only because `HashMap::new` is not `const` — the maps are
    /// built on first write so the whole cache can live in a `static`.
    hits: RwLock<Option<HashMap<String, Entry<V>>>>,
    misses: RwLock<Option<HashMap<String, Entry<()>>>>,
    ttl: Duration,
    cap: usize,
    /// Logical clock for LRU: bumped on every read hit.
    tick: AtomicU64,
    /// Per-host lock held while one caller queries.
    flights: Mutex<Option<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

/// A cached value, when it was stored, and when it was last read.
struct Entry<V> {
    value: V,
    at: Instant,
    used: AtomicU64,
}

/// Held by the one caller querying for a host; see [`HostTtlCache::single_flight`].
pub(super) struct Flight<'a, V> {
    cache: &'a HostTtlCache<V>,
    host: String,
    lock: Arc<tokio::sync::Mutex<()>>,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl<V> Drop for Flight<'_, V> {
    fn drop(&mut self) {
        // The map, `lock` and the guard: 3 means nobody else waits.
        let Ok(mut g) = self.cache.flights.lock() else {
            return;
        };
        if let Some(map) = g.as_mut() {
            if Arc::strong_count(&self.lock) <= 3 {
                map.remove(&self.host);
            }
        }
    }
}

impl<V: Clone> HostTtlCache<V> {
    pub(super) const fn new(ttl: Duration, cap: usize) -> Self {
        Self {
            hits: RwLock::new(None),
            misses: RwLock::new(None),
            ttl,
            cap,
            tick: AtomicU64::new(0),
            flights: Mutex::new(None),
        }
    }

    pub(super) fn get(&self, host: &str) -> Cached<V> {
        if let Some(v) = self.lookup(&self.hits, host) {
            return Cached::Hit(v);
        }
        match self.lookup(&self.misses, host) {
            Some(()) => Cached::Miss,
            None => Cached::Absent,
        }
    }

    fn lookup<T: Clone>(&self, map: &RwLock<Option<HashMap<String, Entry<T>>>>, host: &str) -> Option<T> {
        let guard = map.read().ok()?;
        let e = guard.as_ref()?.get(host)?;
        // `>=`, not `>`: an entry exactly at its TTL is expired.
        if e.at.elapsed() >= self.ttl {
            return None;
        }
        e.used
            .store(self.tick.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
        Some(e.value.clone())
    }

    pub(super) fn put(&self, host: &str, value: Option<V>) {
        match value {
            Some(v) => {
                remove(&self.misses, host);
                self.insert(&self.hits, self.cap, host, v);
            }
            None => {
                remove(&self.hits, host);
                self.insert(&self.misses, (self.cap / 4).max(1), host, ());
            }
        }
    }

    fn insert<T>(&self, map: &RwLock<Option<HashMap<String, Entry<T>>>>, cap: usize, host: &str, value: T) {
        let Ok(mut guard) = map.write() else {
            return;
        };
        let map = guard.get_or_insert_with(HashMap::default);
        if map.len() >= cap && !map.contains_key(host) {
            map.retain(|_, e| e.at.elapsed() < self.ttl);
            if map.len() >= cap {
                let victim = map
                    .iter()
                    .min_by_key(|(_, e)| e.used.load(Ordering::Relaxed))
                    .map(|(k, _)| k.clone());
                if let Some(victim) = victim {
                    map.remove(&victim);
                }
            }
        }
        let used = AtomicU64::new(self.tick.fetch_add(1, Ordering::Relaxed));
        map.insert(host.to_owned(), Entry { value, at: Instant::now(), used });
    }

    /// Wait until no other caller is querying `host`. Re-check [`Self::get`]
    /// after this returns: the caller ahead may have filled it.
    pub(super) async fn single_flight(&self, host: &str) -> Flight<'_, V> {
        let lock = {
            let mut g = self.flights.lock().unwrap_or_else(PoisonError::into_inner);
            g.get_or_insert_with(HashMap::default)
                .entry(host.to_owned())
                .or_default()
                .clone()
        };
        let guard = lock.clone().lock_owned().await;
        Flight {
            cache: self,
            host: host.to_owned(),
            lock,
            _guard: guard,
        }
    }

    pub(super) fn clear(&self) {
        clear(&self.hits);
        clear(&self.misses);
    }
}

fn remove<T>(map: &RwLock<Option<HashMap<String, Entry<T>>>>, host: &str) {
    if let Ok(mut g) = map.write() {
        if let Some(m) = g.as_mut() {
            m.remove(host);
        }
    }
}

fn clear<T>(map: &RwLock<Option<HashMap<String, Entry<T>>>>) {
    if let Ok(mut g) = map.write() {
        if let Some(m) = g.as_mut() {
            m.clear();
        }
    }
}

/// Periodic read of a table fingerprint, so one process notices another
/// process's writes.
///
/// A local `invalidate` only clears the pod that called it. Behind a
/// load balancer that is half a solution: every other pod keeps
/// answering from a cache that is now wrong. Polling a cheap fingerprint
/// closes that without a shared cache, a message bus, or making Redis a
/// dependency of tenant resolution — the one path that runs before
/// everything else and must not acquire new ways to fail.
pub(super) struct GenerationPoll<G> {
    /// `None` = never looked. `Some((gen, at))` carries the last
    /// fingerprint seen and the time of the last *attempt* — successful
    /// or not, so a failing probe is still throttled.
    ///
    /// `gen` is itself `Option`: a claimed-but-not-yet-completed check
    /// writes the attempt time with the previous fingerprint, so a probe
    /// that fails leaves the last known-good value in place rather than
    /// resetting it.
    state: RwLock<Option<(Option<G>, Instant)>>,
    /// Logged once per process rather than once per interval.
    warned: WarnOnce,
}

/// The outcome of trying to claim a poll slot.
///
/// Named rather than `Option<Option<G>>`: the outer layer is "did I get
/// the slot", the inner is "have I ever completed a read", and confusing
/// the two is how a poll starts firing on every request.
enum Claim<G> {
    /// The interval has not elapsed; someone else has this round.
    NotDue,
    /// The slot is yours, carrying the last fingerprint seen (`None` if
    /// no read has ever completed).
    Taken(Option<G>),
}

impl<G: Copy + PartialEq> GenerationPoll<G> {
    pub(super) const fn new() -> Self {
        Self {
            state: RwLock::new(None),
            warned: WarnOnce::new(),
        }
    }

    /// Claim this interval's check, if one is due.
    ///
    /// Claiming **before** the caller awaits is the whole point. Stamping
    /// the time only on success turns a failing probe into a per-request
    /// query storm, and without the claim every request arriving during
    /// an in-flight probe fires its own. Claiming does both: the first
    /// caller through takes the slot, everyone else sees a fresh
    /// timestamp and returns immediately.
    ///
    /// The lock is dropped before the caller awaits — holding a std
    /// `RwLock` across an await parks it on whatever task resumes and can
    /// deadlock the next reader on the same thread.
    fn claim(&self, interval: Duration) -> Claim<G> {
        match self.state.write() {
            Ok(mut g) => {
                let due = g.as_ref().is_none_or(|(_, at)| at.elapsed() >= interval);
                if !due {
                    return Claim::NotDue;
                }
                let previous = g.as_ref().and_then(|(seen, _)| *seen);
                *g = Some((previous, Instant::now()));
                Claim::Taken(previous)
            }
            // A poisoned lock means another thread panicked mid-update.
            // Skip this round rather than acting on a torn value.
            Err(_) => Claim::NotDue,
        }
    }

    /// Record a completed read; returns whether the fingerprint moved.
    fn record(&self, previous: Option<G>, current: G) -> bool {
        match self.state.write() {
            Ok(mut g) => {
                *g = Some((Some(current), Instant::now()));
                previous.is_some_and(|seen| seen != current)
            }
            Err(_) => false,
        }
    }

    /// Warn at most once for the life of the process.
    pub(super) fn warn_once(&self, emit: impl FnOnce()) {
        self.warned.once(emit);
    }

    /// Forget everything (test hook). Process-global state would
    /// otherwise leak a fingerprint from one test's registry into the
    /// next test's brand-new one.
    #[cfg(any(test, feature = "testkit"))]
    pub(super) fn reset(&self) {
        if let Ok(mut g) = self.state.write() {
            *g = None;
        }
    }

    /// Back-date the last attempt by `by`, so the next poll is due
    /// immediately instead of sleeping out the interval (test hook).
    ///
    /// Unconditional by design. Guarding on an existing `Some` made this
    /// a silent no-op in precisely the case a test reaches for it —
    /// right after a reset — so the test read as though it forced a
    /// re-check while actually doing nothing.
    ///
    /// `floor` is the fallback for a monotonic clock younger than `by`,
    /// reachable on a freshly booted host. Falling back to `now()` there
    /// would *unexpire* the entry and invert this function's purpose.
    #[cfg(any(test, feature = "testkit"))]
    pub(super) fn expire(&self, by: Duration, floor: Instant) {
        if let Ok(mut g) = self.state.write() {
            let previous = g.as_ref().and_then(|(seen, _)| *seen);
            let long_ago = Instant::now().checked_sub(by).unwrap_or(floor);
            *g = Some((previous, long_ago));
        }
    }

    /// Run one throttled poll.
    ///
    /// `read` fetches the fingerprint; `on_change` runs only when it
    /// moved. `interval` of `None` disables polling entirely. Errors are
    /// handed to `on_error` — never propagated, because a cache refresh
    /// must not fail a request.
    ///
    /// `read` may return `Ok(None)` to decline this round without it
    /// counting as a failure — what a caller wants when its own breaker
    /// is already open and going to the database would only re-pay the
    /// timeout. The claim has already been recorded either way, so a
    /// decline backs off for a full interval rather than re-deciding on
    /// every request.
    pub(super) async fn sync<F, Fut, E>(
        &self,
        interval: Option<Duration>,
        read: F,
        on_error: impl FnOnce(&E),
        on_change: impl FnOnce(),
    ) where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Option<G>, E>>,
    {
        let Some(interval) = interval else {
            return;
        };
        let Claim::Taken(previous) = self.claim(interval) else {
            return;
        };
        let current = match read().await {
            Ok(Some(current)) => current,
            Ok(None) => return,
            Err(e) => {
                on_error(&e);
                return;
            }
        };
        if self.record(previous, current) {
            on_change();
        }
    }
}

/// A "this dependency is failing, stop asking" latch.
///
/// Tenant resolution runs before everything else, so a broken dependency
/// is paid **per request** — each one waiting out the pool's acquire
/// timeout before erroring. That pins a worker for the whole timeout, so
/// the server saturates and every tenant goes down, including tenants
/// whose own databases are perfectly healthy. Measured against a renamed
/// host table: 25,987 failing `SELECT`s for 25,958 requests, all aimed
/// at a registry that was by definition already unhealthy.
///
/// Recording the failure lets the requests behind the first one fail
/// immediately instead of queueing for the same doomed connection.
/// Deliberately *not* a behaviour change for the caller: it still gets
/// the same `Err` and still renders whatever it rendered before — just
/// in microseconds rather than seconds.
///
/// The retry window is passed per call rather than stored, because the
/// two users want different ones and one of them reads its window from
/// the environment.
pub(super) struct Breaker {
    /// When the dependency last failed, if it is currently failing.
    at: RwLock<Option<Instant>>,
    warned: WarnOnce,
}

impl Breaker {
    pub(super) const fn new() -> Self {
        Self {
            at: RwLock::new(None),
            warned: WarnOnce::new(),
        }
    }

    /// True when a recent attempt failed and `window` has not elapsed.
    /// Read-locked, so the healthy path pays one uncontended read.
    pub(super) fn is_open(&self, window: Duration) -> bool {
        match self.at.read() {
            Ok(g) => g.is_some_and(|at| at.elapsed() < window),
            Err(_) => false,
        }
    }

    /// Open the breaker after a failed attempt.
    pub(super) fn open(&self) {
        if let Ok(mut g) = self.at.write() {
            *g = Some(Instant::now());
        }
    }

    /// Close it after a successful one — checked under a read lock first
    /// so the common case, never having failed, takes no write lock.
    pub(super) fn close(&self) {
        if self.at.read().is_ok_and(|g| g.is_some()) {
            if let Ok(mut g) = self.at.write() {
                *g = None;
            }
        }
    }

    /// Warn at most once for the life of the process.
    pub(super) fn warn_once(&self, emit: impl FnOnce()) {
        self.warned.once(emit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    const TTL: Duration = Duration::from_secs(60);

    /// Junk hosts fill only the miss side; a resolved tenant stays cached.
    #[test]
    fn misses_never_evict_a_resolved_host() {
        let cache = HostTtlCache::<u32>::new(TTL, 2);
        cache.put("acme.example", Some(1));
        for i in 0..64 {
            cache.put(&format!("junk{i}.example"), None);
        }
        assert!(matches!(cache.get("acme.example"), Cached::Hit(1)));
        assert!(matches!(cache.get("junk63.example"), Cached::Miss));
    }

    /// At the cap the least recently read entry goes.
    #[test]
    fn a_full_cache_evicts_the_least_recently_used() {
        for _ in 0..16 {
            let cache = HostTtlCache::<u32>::new(TTL, 2);
            cache.put("a", Some(1));
            cache.put("b", Some(2));
            let _ = cache.get("a");
            cache.put("c", Some(3));
            assert!(matches!(cache.get("a"), Cached::Hit(1)), "a was just read");
            assert!(matches!(cache.get("b"), Cached::Absent), "b was idle");
        }
    }

    /// Concurrent cold lookups of one host run the query once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_cold_lookups_query_once() {
        let cache = Arc::new(HostTtlCache::<u32>::new(TTL, 8));
        let queries = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let (cache, queries) = (cache.clone(), queries.clone());
                tokio::spawn(async move {
                    let _flight = cache.single_flight("acme.example").await;
                    if matches!(cache.get("acme.example"), Cached::Absent) {
                        queries.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        cache.put("acme.example", Some(1));
                    }
                })
            })
            .collect();
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(queries.load(Ordering::SeqCst), 1);
    }
}
