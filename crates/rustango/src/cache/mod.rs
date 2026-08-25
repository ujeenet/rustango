//! Pluggable caching layer.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::cache::{Cache, InMemoryCache, get_json, set_json, get_or_set};
//! use std::sync::Arc;
//! use std::time::Duration;
//!
//! // Build a shared cache (put it in axum Extension or your own state)
//! let cache: Arc<dyn Cache> = Arc::new(InMemoryCache::new());
//!
//! // Raw string values
//! cache.set("greeting", "hello", Some(Duration::from_secs(60))).await?;
//! let val: Option<String> = cache.get("greeting").await?;
//!
//! // Typed JSON helpers
//! set_json(&*cache, "user:1", &my_struct, Some(Duration::from_secs(300))).await?;
//! let user: Option<MyStruct> = get_json(&*cache, "user:1").await?;
//!
//! // Fetch-or-compute pattern
//! let posts: Vec<Post> = get_or_set(
//!     &*cache,
//!     "posts:recent",
//!     || async { Post::objects().order_by("-created_at").fetch(&pool).await.unwrap() },
//!     Some(Duration::from_secs(60)),
//! ).await?;
//! ```
//!
//! ## Backends
//!
//! | Type | Feature | Description |
//! |------|---------|-------------|
//! | [`NullCache`] | `cache` | No-op; all reads return `None`. Good for tests. |
//! | [`InMemoryCache`] | `cache` | Per-process HashMap with TTL. Zero external deps. |
//! | [`FileCache`] | `cache` | File-system, one file per key (#408). |
//! | [`DatabaseCache`](db_backend::DatabaseCache) | `cache` + any DB feature | DB table, tri-dialect upsert (#409). |
//! | [`RedisCache`](redis_backend::RedisCache) | `cache-redis` | Redis-backed via async connection manager. |
//!
//! ## Shared cache type
//!
//! `Arc<dyn Cache>` is the recommended way to share a cache across handlers.
//! Use [`BoxedCache`] as a convenient alias.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;

#[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
pub mod db_backend;
#[cfg(feature = "cache-redis")]
pub mod redis_backend;

#[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
pub use db_backend::DatabaseCache;

pub mod scoped;
pub use scoped::{ScopedCache, TENANT_PREFIX};

// ------------------------------------------------------------------ CacheError

/// Errors returned by cache operations.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("cache connection error: {0}")]
    Connection(String),
    #[error("cache serialization error: {0}")]
    Serialization(String),
}

// ------------------------------------------------------------------ Cache trait

/// Pluggable async cache. All methods are async and return `Result`.
///
/// # Object safety
///
/// Implementations are object-safe — store as `Arc<dyn Cache>` to pass
/// the backend through axum state or `Extension`.
#[async_trait]
pub trait Cache: Send + Sync + 'static {
    /// Retrieve the value for `key`, or `None` if absent or expired.
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError>;

    /// Store `value` under `key` with an optional TTL.
    ///
    /// `ttl = None` means "no expiry" (store indefinitely).
    async fn set(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<(), CacheError>;

    /// Remove `key` from the cache. No-op if absent.
    async fn delete(&self, key: &str) -> Result<(), CacheError>;

    /// Return `true` when `key` is present and not expired.
    async fn exists(&self, key: &str) -> Result<bool, CacheError>;

    /// Remove all entries from the cache.
    async fn clear(&self) -> Result<(), CacheError>;

    /// Atomically increment the integer counter at `key` by `by` and
    /// return the new value. The default implementation is a non-atomic
    /// get + parse + set — fine for single-process use. `RedisCache`
    /// overrides with `INCRBY` so multi-replica rate limiters can rely
    /// on it across processes.
    ///
    /// `ttl` is applied on every call by the default impl; backends with
    /// native counters typically only set TTL on first creation. Treat
    /// `ttl` as a hint, not a guarantee.
    ///
    /// Non-integer existing values are treated as 0 — the counter is
    /// overwritten with `by` and the new value is `by` itself.
    async fn incr(&self, key: &str, by: i64, ttl: Option<Duration>) -> Result<i64, CacheError> {
        let cur = self
            .get(key)
            .await?
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        let new = cur.saturating_add(by);
        self.set(key, &new.to_string(), ttl).await?;
        Ok(new)
    }

    /// Django-parity `cache.add(key, value, timeout)` — set the value
    /// ONLY if the key is currently absent (or expired). Returns `true`
    /// when the value was inserted, `false` when an existing entry
    /// blocked the write.
    ///
    /// The default implementation is a non-atomic `exists` + `set`
    /// pair, which races between processes; backends with a native
    /// "set if absent" primitive (Redis `SET NX`) should override
    /// for atomicity. For single-process locks, the default is fine.
    ///
    /// Useful as a lightweight inter-process lock primitive:
    ///
    /// ```ignore
    /// if cache.add("import-running", "1", Some(Duration::from_secs(60))).await? {
    ///     // We won the race — run the import.
    /// }
    /// ```
    async fn add(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<bool, CacheError> {
        if self.exists(key).await? {
            return Ok(false);
        }
        self.set(key, value, ttl).await?;
        Ok(true)
    }

    /// Django-parity `cache.touch(key, timeout)` — extend (or replace)
    /// the TTL on an existing key without changing the value. Returns
    /// `true` when the key existed and the TTL was reset, `false`
    /// when the key was absent or already expired (no-op).
    ///
    /// The default implementation is a non-atomic `get` + `set` round-
    /// trip. Backends with a native `EXPIRE` / `PEXPIRE` primitive
    /// should override for an O(1) single-RTT path.
    ///
    /// `ttl = None` makes the entry persist indefinitely (matching
    /// `set(_, _, None)`).
    async fn touch(&self, key: &str, ttl: Option<Duration>) -> Result<bool, CacheError> {
        match self.get(key).await? {
            Some(value) => {
                self.set(key, &value, ttl).await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Django-parity `cache.get_many(keys)` — bulk fetch for a key
    /// set, returning a map of present-and-not-expired entries.
    /// Missing keys are omitted (Django's shape). Order of the
    /// returned map is unspecified.
    ///
    /// Default implementation issues one `get` per key in sequence.
    /// Backends with native batch primitives should override:
    /// * `RedisCache` → `MGET` (one RTT)
    /// * `DatabaseCache` → `SELECT … WHERE cache_key IN (…)` (one query)
    async fn get_many(&self, keys: &[&str]) -> Result<HashMap<String, String>, CacheError> {
        let mut out = HashMap::with_capacity(keys.len());
        for k in keys {
            if let Some(v) = self.get(k).await? {
                out.insert((*k).to_owned(), v);
            }
        }
        Ok(out)
    }

    /// Django-parity `cache.set_many(mapping, timeout)` — bulk-set
    /// many key/value pairs with one shared TTL. Equivalent to
    /// looping `set` per entry; backends with native pipelines
    /// (Redis `MSET` + `EXPIRE`, or executor-side `bulk_insert`)
    /// should override.
    async fn set_many(
        &self,
        entries: &[(&str, &str)],
        ttl: Option<Duration>,
    ) -> Result<(), CacheError> {
        for (k, v) in entries {
            self.set(k, v, ttl).await?;
        }
        Ok(())
    }

    /// Django-parity `cache.delete_many(keys)` — bulk-delete every
    /// listed key. Missing keys are silently ignored. Default loops
    /// `delete`; backends with native primitives (`DEL key1 key2`)
    /// override.
    async fn delete_many(&self, keys: &[&str]) -> Result<(), CacheError> {
        for k in keys {
            self.delete(k).await?;
        }
        Ok(())
    }

    /// Django-parity `cache.has_key(key)` — direct alias for
    /// [`Self::exists`]. Django spells the membership check as
    /// `has_key`; rustango shipped `exists` first (Rust convention)
    /// but the Django method name is the one most users reach for
    /// when translating from a Django codebase.
    ///
    /// Default implementation delegates to `exists`; backends never
    /// need to override.
    async fn has_key(&self, key: &str) -> Result<bool, CacheError> {
        self.exists(key).await
    }

    /// Django-parity `cache.decr(key, delta=1)` — atomically decrement
    /// the integer counter at `key` by `by` and return the new value.
    ///
    /// Equivalent to [`Self::incr`] with a negated `by` — the default
    /// implementation simply forwards to `incr(-by)`, so backends that
    /// override `incr` for atomicity (Redis `INCRBY -N`) get the
    /// matching atomic `decr` for free.
    ///
    /// `ttl` semantics mirror `incr` — treat as a hint; backends with
    /// native counters typically only set TTL on first creation.
    async fn decr(&self, key: &str, by: i64, ttl: Option<Duration>) -> Result<i64, CacheError> {
        self.incr(key, by.saturating_neg(), ttl).await
    }

    /// Django-parity `cache.get(key, default)` — returns the stored
    /// value, or `default` (cloned) when the key is absent or expired.
    ///
    /// Direct translation of Django's two-arg form:
    ///
    /// ```python
    /// # Django
    /// name = cache.get('username', default='anonymous')
    /// ```
    ///
    /// ```ignore
    /// // rustango
    /// let name = cache.get_or("username", "anonymous").await?;
    /// ```
    ///
    /// `default` is taken by `&str` so callers can pass either string
    /// literals or borrowed `String`s without an unnecessary allocation
    /// on the hit path — the allocation only happens on miss.
    async fn get_or(&self, key: &str, default: &str) -> Result<String, CacheError> {
        Ok(self.get(key).await?.unwrap_or_else(|| default.to_owned()))
    }

    /// Delete every key starting with `prefix`. Powers
    /// [`ScopedCache::clear`], which must drop one namespace's entries
    /// without touching anyone else's (#1227).
    ///
    /// **The default over-deletes: it clears the whole cache** and logs
    /// a warning. That is deliberate. A backend that cannot enumerate
    /// its keys has two options, and only one of them is safe:
    /// under-deleting leaves stale entries that a *different* namespace
    /// can then read, which is a correctness bug; over-deleting costs
    /// other namespaces a cache miss. [`FileCache`] is the concrete
    /// case — it hashes keys into paths, so the prefix is not
    /// recoverable from the filename.
    ///
    /// Backends that can enumerate override this:
    /// [`InMemoryCache`] filters its map, [`DatabaseCache`] issues a
    /// `DELETE … WHERE cache_key LIKE 'prefix%'`, and [`NullCache`]
    /// has nothing to delete.
    async fn delete_prefix(&self, prefix: &str) -> Result<(), CacheError> {
        tracing::warn!(
            target: "rustango::cache",
            prefix = %prefix,
            "this cache backend cannot enumerate keys, so a prefix delete clears \
             the WHOLE cache — other namespaces will see a cold cache. Use a \
             backend that overrides `delete_prefix` (memory / database) if that \
             matters",
        );
        self.clear().await
    }
}

/// `Arc<dyn Cache>` alias — the standard way to share a cache instance.
pub type BoxedCache = Arc<dyn Cache>;

/// Build a [`BoxedCache`] from a loaded
/// [`crate::config::CacheSettings`] section (#87 wiring, v0.29).
///
/// Backend selection from `s.backend`:
/// - `"memory"` (default) → [`InMemoryCache`]
/// - `"redis"` → [`redis_backend::RedisCache`] (requires
///   `cache-redis` feature; falls back to `InMemoryCache` with a
///   warning when the feature isn't compiled in)
/// - `"null"` / `"none"` → [`NullCache`]
/// - any other / unset → [`InMemoryCache`] with a warning if the
///   value was non-empty (typo defense)
///
/// `redis_url` is required when `backend = "redis"` — without it
/// the resolver falls back to `InMemoryCache` with a warning so
/// startup doesn't block on a misconfig.
///
/// ```ignore
/// let cfg = rustango::config::Settings::load_from_env()?;
/// let cache: rustango::cache::BoxedCache =
///     rustango::cache::from_settings(&cfg.cache);
/// ```
#[cfg(feature = "config")]
#[must_use]
pub fn from_settings(s: &crate::config::CacheSettings) -> BoxedCache {
    match s.backend.as_deref() {
        Some("redis") => {
            #[cfg(feature = "cache-redis")]
            {
                if s.redis_url.as_deref().is_some_and(|u| !u.is_empty()) {
                    // `RedisCache::new` is async (it pings the
                    // server eagerly to surface bad URLs at boot)
                    // but `from_settings` is sync — we can't .await
                    // here without changing the public API. Users
                    // who want redis must construct it explicitly:
                    //
                    //     let cache = RedisCache::new(&url).await?;
                    //     let boxed: BoxedCache = Arc::new(cache);
                    //
                    // We fall back to InMemoryCache + warn rather
                    // than silently returning the wrong backend.
                    tracing::warn!(
                        target: "rustango::cache",
                        "cache.backend = \"redis\" requires async construction; \
                         build `RedisCache::new(url).await?` and pass the Arc \
                         directly. Falling back to InMemoryCache."
                    );
                } else {
                    tracing::warn!(
                        target: "rustango::cache",
                        "cache.backend = \"redis\" but redis_url is unset; falling back to InMemoryCache",
                    );
                }
            }
            #[cfg(not(feature = "cache-redis"))]
            {
                tracing::warn!(
                    target: "rustango::cache",
                    "cache.backend = \"redis\" but the `cache-redis` feature isn't compiled in; falling back to InMemoryCache",
                );
            }
            Arc::new(InMemoryCache::new())
        }
        Some("null" | "none") => Arc::new(NullCache),
        Some("file") => file_from_settings_or_warn(s),
        Some("db" | "database") => {
            // #409 — DatabaseCache needs a runtime Pool and an async
            // `ensure_table()` step that this sync resolver can't
            // perform. Apps that want the DB backend must build it
            // explicitly:
            //
            //     let cache = DatabaseCache::new(pool.clone(), "rustango_cache");
            //     cache.ensure_table().await?;
            //     let boxed: BoxedCache = Arc::new(cache);
            //
            // We fall back to InMemoryCache + warn rather than
            // silently producing a different backend.
            tracing::warn!(
                target: "rustango::cache",
                "cache.backend = \"db\" requires async construction with a `&Pool`; \
                 build `DatabaseCache::new(pool, table)` and call `ensure_table().await` \
                 then pass the Arc directly. Falling back to InMemoryCache."
            );
            Arc::new(InMemoryCache::new())
        }
        Some("memory") | None => Arc::new(InMemoryCache::new()),
        Some(other) => {
            tracing::warn!(
                target: "rustango::cache",
                backend = %other,
                "unknown cache.backend value; falling back to InMemoryCache",
            );
            Arc::new(InMemoryCache::new())
        }
    }
}

/// File-backend resolver — needs `[cache].file_cache_dir` set,
/// otherwise warns and falls back to `InMemoryCache` so the app still
/// boots on misconfig. Issue #408.
#[cfg(feature = "config")]
fn file_from_settings_or_warn(s: &crate::config::CacheSettings) -> BoxedCache {
    match s.file_cache_dir.as_deref() {
        Some(dir) => Arc::new(FileCache::new(dir)),
        None => {
            tracing::warn!(
                target: "rustango::cache",
                "cache.backend = \"file\" but [cache].file_cache_dir is unset; \
                 falling back to InMemoryCache.",
            );
            Arc::new(InMemoryCache::new())
        }
    }
}

// ------------------------------------------------------------------ Typed helpers

/// Retrieve a JSON-deserializable value from the cache.
///
/// Returns `None` when the key is absent, expired, or the stored string
/// isn't valid JSON for `T`.
///
/// # Errors
/// [`CacheError::Connection`] on backend failures.
/// [`CacheError::Serialization`] when the stored value can't be decoded.
pub async fn get_json<T: serde::de::DeserializeOwned>(
    cache: &dyn Cache,
    key: &str,
) -> Result<Option<T>, CacheError> {
    let Some(s) = cache.get(key).await? else {
        return Ok(None);
    };
    serde_json::from_str(&s)
        .map(Some)
        .map_err(|e| CacheError::Serialization(e.to_string()))
}

/// Serialize `value` to JSON and store it under `key` with an optional TTL.
///
/// # Errors
/// [`CacheError::Serialization`] when `value` can't be encoded.
/// [`CacheError::Connection`] on backend failures.
pub async fn set_json<T: serde::Serialize>(
    cache: &dyn Cache,
    key: &str,
    value: &T,
    ttl: Option<Duration>,
) -> Result<(), CacheError> {
    let s = serde_json::to_string(value).map_err(|e| CacheError::Serialization(e.to_string()))?;
    cache.set(key, &s, ttl).await
}

/// Return the cached value for `key`, or compute it with `factory`, cache
/// it, and return it.
///
/// The factory is only called on a cache miss. The computed value is stored
/// with `ttl`.
///
/// # Errors
/// [`CacheError::Serialization`] when encoding/decoding fails.
/// [`CacheError::Connection`] on backend failures.
pub async fn get_or_set<T, F, Fut>(
    cache: &dyn Cache,
    key: &str,
    factory: F,
    ttl: Option<Duration>,
) -> Result<T, CacheError>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
    F: FnOnce() -> Fut + Send,
    Fut: std::future::Future<Output = T> + Send,
{
    if let Some(cached) = get_json::<T>(cache, key).await? {
        return Ok(cached);
    }
    let value = factory().await;
    set_json(cache, key, &value, ttl).await?;
    Ok(value)
}

// ------------------------------------------------------------------ NullCache

/// A no-op cache that stores nothing and returns `None` for every read.
///
/// Useful in tests and for disabling caching without changing call sites.
///
/// ```ignore
/// let cache: Arc<dyn Cache> = Arc::new(NullCache);
/// assert!(cache.get("any").await?.is_none());
/// ```
pub struct NullCache;

#[async_trait]
impl Cache for NullCache {
    async fn get(&self, _key: &str) -> Result<Option<String>, CacheError> {
        Ok(None)
    }

    async fn set(
        &self,
        _key: &str,
        _value: &str,
        _ttl: Option<Duration>,
    ) -> Result<(), CacheError> {
        Ok(())
    }

    async fn delete(&self, _key: &str) -> Result<(), CacheError> {
        Ok(())
    }

    async fn exists(&self, _key: &str) -> Result<bool, CacheError> {
        Ok(false)
    }

    async fn clear(&self) -> Result<(), CacheError> {
        Ok(())
    }

    /// Nothing is stored, so nothing needs deleting — and in particular
    /// this must not fall through to the warning on the trait default.
    async fn delete_prefix(&self, _prefix: &str) -> Result<(), CacheError> {
        Ok(())
    }
}

// ------------------------------------------------------------------ InMemoryCache

struct CacheEntry {
    value: String,
    expires_at: Option<Instant>,
    /// Monotonic access tick for approximate-LRU eviction; bumped on
    /// every read/write from [`InMemoryCache::tick`]. `AtomicU64` so
    /// reads (which hold only the read lock) can update it.
    last_used: AtomicU64,
    /// `key.len() + value.len()` — this entry's charge against the
    /// cache's byte budget.
    size: usize,
}

impl CacheEntry {
    fn is_expired(&self) -> bool {
        self.expires_at.map_or(false, |t| Instant::now() > t)
    }
}

/// Map plus its running byte total, guarded together so the two can't
/// drift under concurrent mutation.
struct Store {
    map: HashMap<String, CacheEntry>,
    used_bytes: usize,
}

/// Default byte budget for [`InMemoryCache::new`] — 256 MiB. Big enough
/// that normal page / fragment caching never evicts, small enough that
/// an unauthenticated flood of unique keys (e.g. `?cb=<random>`, which
/// each create a distinct cache entry) can't drive the process to OOM.
/// Override — including disabling (`0`) — via
/// [`InMemoryCache::with_max_bytes`].
pub const DEFAULT_MAX_BYTES: usize = 256 * 1024 * 1024;

/// Default entry-count cap for [`InMemoryCache::new`] — bounds the
/// eviction scan (and memory) even when individual entries are tiny.
/// `0` disables it; see [`InMemoryCache::with_max_entries`].
pub const DEFAULT_MAX_ENTRIES: usize = 100_000;

/// A per-process in-memory cache backed by a `tokio::sync::RwLock<HashMap>`.
///
/// - Thread-safe, async-friendly, zero external dependencies.
/// - TTL is enforced lazily on reads (no background eviction thread).
/// - **Size-bounded** (since #_cache_bound): capped at
///   [`DEFAULT_MAX_BYTES`] / [`DEFAULT_MAX_ENTRIES`] with approximate-LRU
///   eviction, so a flood of unique keys can't grow the process without
///   limit. Eviction drops expired entries first, then the
///   least-recently-used, until both budgets are met. Override or
///   disable the budgets with [`InMemoryCache::with_max_bytes`] /
///   [`InMemoryCache::with_max_entries`] (`0` = unbounded — the
///   pre-#_cache_bound behavior).
/// - `clear()` removes all entries.
///
/// # Optional default TTL
///
/// Build with [`InMemoryCache::with_default_ttl`] to apply a TTL to every
/// `set` call that passes `ttl = None`.
pub struct InMemoryCache {
    inner: tokio::sync::RwLock<Store>,
    default_ttl: Option<Duration>,
    /// Byte budget; `0` = unbounded (opt-out).
    max_bytes: usize,
    /// Entry-count budget; `0` = unbounded.
    max_entries: usize,
    /// Monotonic clock feeding each entry's `last_used` (approx-LRU).
    tick: AtomicU64,
}

impl InMemoryCache {
    /// Create a cache with no default TTL and the default size budgets
    /// ([`DEFAULT_MAX_BYTES`] / [`DEFAULT_MAX_ENTRIES`], LRU eviction).
    #[must_use]
    pub fn new() -> Self {
        Self::build(None, DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES)
    }

    /// Create a cache where every `set(key, value, None)` call uses
    /// `default_ttl` instead of "no expiry". Keeps the default budgets.
    #[must_use]
    pub fn with_default_ttl(default_ttl: Duration) -> Self {
        Self::build(Some(default_ttl), DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES)
    }

    /// Override the byte budget. `0` disables it (unbounded — the old
    /// behavior). Chainable: `InMemoryCache::new().with_max_bytes(64 << 20)`.
    #[must_use]
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// Override the entry-count budget. `0` disables it. Chainable.
    #[must_use]
    pub fn with_max_entries(mut self, max_entries: usize) -> Self {
        self.max_entries = max_entries;
        self
    }

    fn build(default_ttl: Option<Duration>, max_bytes: usize, max_entries: usize) -> Self {
        Self {
            inner: tokio::sync::RwLock::new(Store {
                map: HashMap::new(),
                used_bytes: 0,
            }),
            default_ttl,
            max_bytes,
            max_entries,
            tick: AtomicU64::new(0),
        }
    }

    fn resolve_ttl(&self, ttl: Option<Duration>) -> Option<Instant> {
        let effective = ttl.or(self.default_ttl)?;
        Some(Instant::now() + effective)
    }

    fn next_tick(&self) -> u64 {
        self.tick.fetch_add(1, Ordering::Relaxed)
    }

    fn over_budget(&self, s: &Store) -> bool {
        (self.max_bytes > 0 && s.used_bytes > self.max_bytes)
            || (self.max_entries > 0 && s.map.len() > self.max_entries)
    }

    /// Evict until both budgets are satisfied — expired entries first
    /// (free + always correct), then least-recently-used. Caller holds
    /// the write lock. Always keeps at least one entry, so a single
    /// value larger than the whole budget still caches.
    fn evict_locked(&self, store: &mut Store) {
        if !self.over_budget(store) {
            return;
        }
        let expired: Vec<String> = store
            .map
            .iter()
            .filter(|(_, e)| e.is_expired())
            .map(|(k, _)| k.clone())
            .collect();
        for k in expired {
            if let Some(e) = store.map.remove(&k) {
                store.used_bytes = store.used_bytes.saturating_sub(e.size);
            }
        }
        while self.over_budget(store) && store.map.len() > 1 {
            let victim = store
                .map
                .iter()
                .min_by_key(|(_, e)| e.last_used.load(Ordering::Relaxed))
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => {
                    if let Some(e) = store.map.remove(&k) {
                        store.used_bytes = store.used_bytes.saturating_sub(e.size);
                    }
                }
                None => break,
            }
        }
    }
}

impl Default for InMemoryCache {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Cache for InMemoryCache {
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError> {
        let store = self.inner.read().await;
        Ok(store.map.get(key).and_then(|e| {
            if e.is_expired() {
                None
            } else {
                // Approx-LRU bump — read lock is enough (atomic field).
                e.last_used.store(self.next_tick(), Ordering::Relaxed);
                Some(e.value.clone())
            }
        }))
    }

    async fn set(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<(), CacheError> {
        let expires_at = self.resolve_ttl(ttl);
        let size = key.len() + value.len();
        let tick = self.next_tick();
        let mut store = self.inner.write().await;
        if let Some(old) = store.map.remove(key) {
            store.used_bytes = store.used_bytes.saturating_sub(old.size);
        }
        store.used_bytes += size;
        store.map.insert(
            key.to_owned(),
            CacheEntry {
                value: value.to_owned(),
                expires_at,
                last_used: AtomicU64::new(tick),
                size,
            },
        );
        self.evict_locked(&mut store);
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), CacheError> {
        let mut store = self.inner.write().await;
        if let Some(e) = store.map.remove(key) {
            store.used_bytes = store.used_bytes.saturating_sub(e.size);
        }
        Ok(())
    }

    async fn exists(&self, key: &str) -> Result<bool, CacheError> {
        let store = self.inner.read().await;
        Ok(store.map.get(key).map_or(false, |e| !e.is_expired()))
    }

    async fn clear(&self) -> Result<(), CacheError> {
        let mut store = self.inner.write().await;
        store.map.clear();
        store.used_bytes = 0;
        Ok(())
    }

    /// Exact prefix delete — the map is right here, so there is no need
    /// to fall back to the trait default's whole-cache clear (#1227).
    /// `used_bytes` is decremented by what actually left, keeping the
    /// LRU budget honest.
    async fn delete_prefix(&self, prefix: &str) -> Result<(), CacheError> {
        let mut store = self.inner.write().await;
        let mut freed = 0usize;
        store.map.retain(|k, e| {
            if k.starts_with(prefix) {
                freed += k.len() + e.value.len();
                false
            } else {
                true
            }
        });
        store.used_bytes = store.used_bytes.saturating_sub(freed);
        Ok(())
    }
}

// ------------------------------------------------------------------ FileCache

/// File-system cache — one file per key, mirroring Django's
/// `django.core.cache.backends.filebased.FileBasedCache` (issue #408).
///
/// Useful when you want process-restart-durable caching without
/// running Redis, and when the working set fits the local disk.
/// Keys are SHA-256-hashed to produce filenames that are safe across
/// platforms (no path-separator surprises, no length limits, no case
/// folding on macOS). The directory is auto-created on the first
/// `set`.
///
/// ## File format
///
/// Each entry is a small binary blob:
///   `[expires_at_unix_secs: i64 big-endian][value bytes]`
///
/// `expires_at_unix_secs` is `0` when the entry has no TTL. Expired
/// entries are pruned lazily on the next `get` / `exists` call —
/// there is no background reaper.
///
/// ## Limitations vs Django
///
/// Django's FBC takes a `_lock` file for atomic multi-process writes
/// + supports MAX_ENTRIES with a cull strategy. This implementation
/// is the minimal Django-shape primitive: same on-disk semantics,
/// per-process atomicity via `std::fs::write` (atomic per-call on
/// most filesystems). Add file locking when a project actually
/// shares the directory across processes.
pub struct FileCache {
    dir: std::path::PathBuf,
}

impl FileCache {
    /// Build a cache that stores entries under `dir`. The directory
    /// is auto-created on the first `set` call.
    #[must_use]
    pub fn new(dir: impl Into<std::path::PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory entries are stored under.
    #[must_use]
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// Hash the key into a stable, filesystem-safe filename. Uses
    /// SHA-256 (already a workspace dep via `passwords` / `signed_url`)
    /// hexlified; no separators, no length surprises.
    fn key_path(&self, key: &str) -> std::path::PathBuf {
        use sha2::{Digest, Sha256};
        let hash = Sha256::digest(key.as_bytes());
        let mut name = String::with_capacity(64 + 6);
        for b in hash {
            use std::fmt::Write as _;
            let _ = write!(&mut name, "{b:02x}");
        }
        name.push_str(".cache");
        self.dir.join(name)
    }

    fn now_unix_secs() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// Encode `[expires_at: i64 BE][value bytes]`. expires_at = 0
    /// means no TTL.
    fn encode(value: &str, ttl: Option<Duration>) -> Vec<u8> {
        let expires_at = ttl
            .map(|d| Self::now_unix_secs().saturating_add(d.as_secs() as i64))
            .unwrap_or(0);
        let mut out = Vec::with_capacity(8 + value.len());
        out.extend_from_slice(&expires_at.to_be_bytes());
        out.extend_from_slice(value.as_bytes());
        out
    }

    /// Decode the file body. Returns `Some(value)` if present + not
    /// expired, else `None`. Caller is responsible for deleting the
    /// file when this returns `None` due to expiry.
    fn decode(buf: &[u8]) -> Option<(String, bool /* expired */)> {
        if buf.len() < 8 {
            return None;
        }
        let mut ts = [0u8; 8];
        ts.copy_from_slice(&buf[..8]);
        let expires_at = i64::from_be_bytes(ts);
        let value = std::str::from_utf8(&buf[8..]).ok()?.to_owned();
        let expired = expires_at != 0 && Self::now_unix_secs() >= expires_at;
        Some((value, expired))
    }
}

#[async_trait]
impl Cache for FileCache {
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError> {
        let path = self.key_path(key);
        let buf = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(CacheError::Connection(format!("read: {e}"))),
        };
        match Self::decode(&buf) {
            Some((_, true)) => {
                let _ = std::fs::remove_file(&path);
                Ok(None)
            }
            Some((v, false)) => Ok(Some(v)),
            None => {
                let _ = std::fs::remove_file(&path);
                Ok(None)
            }
        }
    }

    async fn set(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<(), CacheError> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| CacheError::Connection(format!("create_dir_all: {e}")))?;
        let path = self.key_path(key);
        std::fs::write(&path, Self::encode(value, ttl))
            .map_err(|e| CacheError::Connection(format!("write: {e}")))?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), CacheError> {
        let path = self.key_path(key);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(CacheError::Connection(format!("remove_file: {e}"))),
        }
    }

    async fn exists(&self, key: &str) -> Result<bool, CacheError> {
        Ok(self.get(key).await?.is_some())
    }

    async fn clear(&self) -> Result<(), CacheError> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(CacheError::Connection(format!("read_dir: {e}"))),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("cache") {
                let _ = std::fs::remove_file(&path);
            }
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "config"))]
mod settings_tests {
    use super::*;

    /// Unset backend → InMemoryCache. The cache is non-trait-named,
    /// but we can confirm by writing then reading.
    #[tokio::test]
    async fn unset_backend_returns_inmemory() {
        let s = crate::config::CacheSettings::default();
        let cache = from_settings(&s);
        cache.set("k", "v", None).await.unwrap();
        assert_eq!(cache.get("k").await.unwrap().as_deref(), Some("v"));
    }

    /// Explicit `"memory"` matches the unset behavior.
    #[tokio::test]
    async fn memory_backend_works() {
        let mut s = crate::config::CacheSettings::default();
        s.backend = Some("memory".into());
        let cache = from_settings(&s);
        cache.set("k", "v", None).await.unwrap();
        assert_eq!(cache.get("k").await.unwrap().as_deref(), Some("v"));
    }

    /// `"null"` / `"none"` map to NullCache — every read returns None.
    #[tokio::test]
    async fn null_backend_drops_writes() {
        let mut s = crate::config::CacheSettings::default();
        s.backend = Some("null".into());
        let cache = from_settings(&s);
        cache.set("k", "v", None).await.unwrap();
        assert!(cache.get("k").await.unwrap().is_none());
    }

    /// Unknown backend names fall back to InMemoryCache (the writes
    /// land — different from the null backend).
    #[tokio::test]
    async fn unknown_backend_falls_back_to_inmemory() {
        let mut s = crate::config::CacheSettings::default();
        s.backend = Some("typo".into());
        let cache = from_settings(&s);
        cache.set("k", "v", None).await.unwrap();
        assert_eq!(cache.get("k").await.unwrap().as_deref(), Some("v"));
    }

    /// `"redis"` without `cache-redis` feature falls back to
    /// InMemoryCache (don't block startup on a misconfig).
    /// Whether the redis arm runs depends on the feature; both paths
    /// must yield a working cache.
    #[tokio::test]
    async fn redis_without_url_falls_back_to_inmemory() {
        let mut s = crate::config::CacheSettings::default();
        s.backend = Some("redis".into());
        // No redis_url — the fallback path should still produce a
        // usable cache.
        let cache = from_settings(&s);
        // Round-trip works only on the in-memory fallback. This
        // test serves as both the "missing url" and "no feature"
        // regression: in either case, the resulting cache is
        // InMemoryCache.
        #[cfg(not(feature = "cache-redis"))]
        {
            cache.set("k", "v", None).await.unwrap();
            assert_eq!(cache.get("k").await.unwrap().as_deref(), Some("v"));
        }
        #[cfg(feature = "cache-redis")]
        {
            // With the feature on, missing url still falls back to
            // in-memory.
            cache.set("k", "v", None).await.unwrap();
            assert_eq!(cache.get("k").await.unwrap().as_deref(), Some("v"));
        }
    }
}

#[cfg(test)]
mod bound_tests {
    use super::*;

    fn val(n: usize) -> String {
        "x".repeat(n)
    }

    /// Byte budget is enforced: flooding unique keys never grows the
    /// cache past `max_bytes`. This is the unique-key memory-DoS guard.
    #[tokio::test]
    async fn byte_budget_caps_unique_key_flood() {
        let cache = InMemoryCache::new()
            .with_max_bytes(10 * 1024)
            .with_max_entries(0);
        for i in 0..1000 {
            cache.set(&format!("k{i}"), &val(1024), None).await.unwrap();
        }
        let store = cache.inner.read().await;
        assert!(
            store.used_bytes <= 10 * 1024,
            "used_bytes {} exceeded byte budget",
            store.used_bytes
        );
        assert!(
            store.map.len() <= 12,
            "entry count {} too high",
            store.map.len()
        );
    }

    /// Entry-count budget is enforced independently of the byte budget.
    #[tokio::test]
    async fn entry_budget_caps_count() {
        let cache = InMemoryCache::new().with_max_bytes(0).with_max_entries(5);
        for i in 0..50 {
            cache.set(&format!("k{i}"), "v", None).await.unwrap();
        }
        assert!(cache.inner.read().await.map.len() <= 5);
    }

    /// Eviction is least-recently-used: a key kept warm by reads
    /// survives a flood that evicts colder keys.
    #[tokio::test]
    async fn lru_keeps_recently_used() {
        let cache = InMemoryCache::new().with_max_bytes(0).with_max_entries(3);
        cache.set("hot", "v", None).await.unwrap();
        cache.set("a", "v", None).await.unwrap();
        cache.set("b", "v", None).await.unwrap();
        let _ = cache.get("hot").await.unwrap(); // touch -> most-recently-used
        cache.set("c", "v", None).await.unwrap(); // evicts LRU (a)
        cache.set("d", "v", None).await.unwrap(); // evicts LRU (b)
        assert_eq!(cache.get("hot").await.unwrap().as_deref(), Some("v"));
    }

    /// `0` budgets restore the pre-fix unbounded behavior (opt-out).
    #[tokio::test]
    async fn zero_budget_is_unbounded() {
        let cache = InMemoryCache::new().with_max_bytes(0).with_max_entries(0);
        for i in 0..1000 {
            cache.set(&format!("k{i}"), "v", None).await.unwrap();
        }
        assert_eq!(cache.inner.read().await.map.len(), 1000);
    }

    /// Deleting an entry returns its bytes to the budget.
    #[tokio::test]
    async fn delete_frees_bytes() {
        let cache = InMemoryCache::new();
        cache.set("k", &val(4096), None).await.unwrap();
        assert!(cache.inner.read().await.used_bytes >= 4096);
        cache.delete("k").await.unwrap();
        assert_eq!(cache.inner.read().await.used_bytes, 0);
    }
}
