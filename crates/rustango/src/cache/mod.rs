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
//! | [`NullCache`] | `cache` | Does nothing; every read returns `None`. Good for tests. |
//! | [`InMemoryCache`] | `cache` | Per-process HashMap with TTL. No external deps. |
//! | [`FileCache`] | `cache` | On disk, one file per key. |
//! | [`DatabaseCache`](crate::cache::db_backend::DatabaseCache) | `cache` + any DB feature | A DB table. Works on all three dialects. |
//! | [`RedisCache`](crate::cache::redis_backend::RedisCache) | `cache-redis` | Redis, via an async connection manager. |
//!
//! ## Sharing a cache
//!
//! Share one instance as `Arc<dyn Cache>`. [`BoxedCache`] is the alias
//! for that type.
//!
//! [`NullCache`]: crate::cache::NullCache
//! [`InMemoryCache`]: crate::cache::InMemoryCache
//! [`FileCache`]: crate::cache::FileCache
//! [`BoxedCache`]: crate::cache::BoxedCache

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

/// Pluggable async cache.
///
/// The trait is object-safe, so you can store a backend as
/// `Arc<dyn Cache>` and pass it through axum state or `Extension`.
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

    /// Add `by` to the integer counter at `key` and return the new
    /// value. A value that is not an integer counts as 0.
    ///
    /// The default is a get-parse-set, which is not atomic. Backends
    /// with a native counter override it: `RedisCache` uses `INCRBY`,
    /// so counters stay correct across replicas.
    ///
    /// Treat `ttl` as a hint. The default applies it on every call;
    /// native counters usually set it only when the key is created.
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

    /// Store the value only if the key is absent or expired.
    /// Returns `true` when the write happened.
    ///
    /// The default is `exists` then `set`, which can race between
    /// processes. Backends with a native "set if absent" (Redis
    /// `SET NX`) should override it.
    ///
    /// Handy as a light cross-process lock:
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

    /// Replace the TTL on a key without
    /// changing its value. Returns `true` when the key was there,
    /// `false` when it was absent or expired.
    ///
    /// `ttl = None` makes the entry last forever, as `set` does.
    ///
    /// The default is a `get` then a `set`. Backends with a native
    /// `EXPIRE` should override it for a single round trip.
    async fn touch(&self, key: &str, ttl: Option<Duration>) -> Result<bool, CacheError> {
        match self.get(key).await? {
            Some(value) => {
                self.set(key, &value, ttl).await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Fetch many keys at once. Missing and expired keys are left out
    /// of the map. The order is not defined.
    ///
    /// The default runs one `get` per key. `RedisCache` overrides with
    /// `MGET` and `DatabaseCache` with a single `IN (…)` query.
    async fn get_many(&self, keys: &[&str]) -> Result<HashMap<String, String>, CacheError> {
        let mut out = HashMap::with_capacity(keys.len());
        for k in keys {
            if let Some(v) = self.get(k).await? {
                out.insert((*k).to_owned(), v);
            }
        }
        Ok(out)
    }

    /// Store many key/value pairs under one shared TTL. The default
    /// loops over `set`. Backends with a pipeline (Redis `MSET`, or a
    /// bulk insert) should override it.
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

    /// Delete every listed key. Missing keys are ignored. The default
    /// loops over `delete`; backends with `DEL k1 k2` override it.
    async fn delete_many(&self, keys: &[&str]) -> Result<(), CacheError> {
        for k in keys {
            self.delete(k).await?;
        }
        Ok(())
    }

    /// Alias for [`Self::exists`]. Never needs an override.
    async fn has_key(&self, key: &str) -> Result<bool, CacheError> {
        self.exists(key).await
    }

    /// Subtract `by` from the counter at `key` and return the new
    /// value. Forwards to [`Self::incr`] with a negated `by`, so a
    /// backend that makes `incr` atomic gets an atomic `decr` too.
    /// `ttl` is a hint, as it is for `incr`.
    async fn decr(&self, key: &str, by: i64, ttl: Option<Duration>) -> Result<i64, CacheError> {
        self.incr(key, by.saturating_neg(), ttl).await
    }

    /// Return the stored value, or `default` when the key is absent or
    /// expired.
    ///
    /// ```ignore
    /// let name = cache.get_or("username", "anonymous").await?;
    /// ```
    ///
    /// `default` is a `&str`, so a hit allocates nothing extra.
    async fn get_or(&self, key: &str, default: &str) -> Result<String, CacheError> {
        Ok(self.get(key).await?.unwrap_or_else(|| default.to_owned()))
    }

    /// Delete every key that starts with `prefix`. This is what
    /// [`ScopedCache::clear`] calls, so it must drop one namespace's
    /// entries and leave the rest alone.
    ///
    /// **The default returns an error.** Clearing the whole cache
    /// instead would let one tenant wipe every other tenant's entries,
    /// plus rate-limit counters and lock keys. An error gives the
    /// author of a new backend a loud failure rather than silent data
    /// loss in someone else's namespace.
    ///
    /// **Every backend in this crate overrides it, and yours must
    /// too**, unless it stores nothing. [`InMemoryCache`] filters its
    /// map, `DatabaseCache` runs `DELETE … WHERE cache_key LIKE
    /// 'prefix%'`, `RedisCache` runs `SCAN MATCH` then `DEL`, and
    /// [`FileCache`] scans its directory (the filename is a hash, so
    /// the key has to be read from inside each file).
    async fn delete_prefix(&self, prefix: &str) -> Result<(), CacheError> {
        Err(CacheError::Connection(format!(
            "this cache backend does not implement `delete_prefix`, so the prefix \
             `{prefix}` cannot be deleted without clearing unrelated namespaces. \
             Implement `delete_prefix` on the backend — clearing everything is not \
             a safe fallback for a scoped delete."
        )))
    }
}

/// `Arc<dyn Cache>` alias — the standard way to share a cache instance.
pub type BoxedCache = Arc<dyn Cache>;

/// Build a [`BoxedCache`] from a loaded
/// [`crate::config::CacheSettings`] section (#87 wiring, v0.29).
///
/// Backend selection from `s.backend`:
/// - `"memory"` (default) / unset → [`InMemoryCache`]
/// - `"null"` / `"none"` → [`NullCache`]
/// - `"file"` → [`FileCache`]. Needs `file_cache_dir`; without it,
///   warns and uses [`InMemoryCache`], which is colder but gives the
///   same guarantees.
/// - `"redis"`, `"db"` / `"database"` → **panics**, see below
/// - any other value → [`InMemoryCache`] with a warning (typo defense)
///
/// # Panics
/// On `backend = "redis"` or `"db"`. Both need to connect, which this
/// sync function cannot do, and quietly substituting another backend
/// would be worse: a per-process cache is not a weaker shared one.
/// Rate limits would then scale with the replica count, and
/// single-use token checks would stop failing closed. Use
/// [`from_settings_async`] for redis, and build the DB backend where
/// the `Pool` is.
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
        // `redis` and `db` need to connect, so this sync resolver
        // cannot build either. Panic rather than pretend: silently
        // handing back a per-process cache voids every protection that
        // works only because the cache is shared.
        Some("redis") => panic!(
            "cache.backend = \"redis\" cannot be built by `from_settings`, which is \
             sync — `RedisCache::new(url)` is async because it pings the server to \
             surface a bad URL at boot.\n\n\
             Use `from_settings_async(&settings.cache).await?`, or build it yourself:\n\
             \x20   let cache: BoxedCache = Arc::new(RedisCache::new(&url).await?);\n\n\
             This used to fall back to an in-memory cache, which silently voided \
             every protection that depends on the cache being shared across replicas."
        ),
        Some("null" | "none") => Arc::new(NullCache),
        Some("file") => file_from_settings_or_warn(s),
        // `DatabaseCache` needs a runtime `Pool` and an async
        // `ensure_table()`. Settings alone cannot describe it, so even
        // the async resolver cannot build this one.
        Some("db" | "database") => panic!(
            "cache.backend = \"db\" cannot be built from settings: `DatabaseCache` \
             needs a runtime `&Pool`, which `[cache]` does not carry, plus an async \
             `ensure_table()` call.\n\n\
             Build it where the pool exists:\n\
             \x20   let cache = DatabaseCache::new(pool.clone(), \"rustango_cache\");\n\
             \x20   cache.ensure_table().await?;\n\
             \x20   let boxed: BoxedCache = Arc::new(cache);\n\n\
             This used to fall back to an in-memory cache, which silently voided \
             every protection that depends on the cache being shared across replicas."
        ),
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

/// [`from_settings`], but it can also build backends that connect —
/// today that is only `redis`. Use this one wherever you can `.await`.
///
/// `db` still cannot come from settings and returns an error saying
/// so: `DatabaseCache` needs a runtime `&Pool`, which `[cache]` does
/// not carry.
///
/// ```no_run
/// # async fn f() -> Result<(), rustango::cache::CacheError> {
/// let cfg = rustango::config::Settings::load_from_env().unwrap();
/// let cache: rustango::cache::BoxedCache =
///     rustango::cache::from_settings_async(&cfg.cache).await?;
/// # Ok(()) }
/// ```
///
/// # Errors
/// Returns [`CacheError`] when the configured backend cannot be built:
/// an unreachable Redis, a missing `redis_url`, or `db`.
#[cfg(feature = "config")]
pub async fn from_settings_async(
    s: &crate::config::CacheSettings,
) -> Result<BoxedCache, CacheError> {
    match s.backend.as_deref() {
        Some("redis") => {
            #[cfg(feature = "cache-redis")]
            {
                let url = s
                    .redis_url
                    .as_deref()
                    .filter(|u| !u.is_empty())
                    .ok_or_else(|| {
                        CacheError::Connection(
                            "cache.backend = \"redis\" but [cache].redis_url is unset".into(),
                        )
                    })?;
                Ok(Arc::new(redis_backend::RedisCache::new(url).await?))
            }
            #[cfg(not(feature = "cache-redis"))]
            {
                Err(CacheError::Connection(
                    "cache.backend = \"redis\" but the `cache-redis` feature is not \
                     compiled in — enable it, or change the backend"
                        .into(),
                ))
            }
        }
        Some("db" | "database") => Err(CacheError::Connection(
            "cache.backend = \"db\" cannot be built from settings: `DatabaseCache` needs \
             a runtime `&Pool`. Build it where the pool exists and pass the Arc directly."
                .into(),
        )),
        // Everything else builds synchronously and behaves the same.
        _ => Ok(from_settings(s)),
    }
}

/// Build a [`FileCache`] from `[cache].file_cache_dir`. If that is
/// unset, warn and fall back to `InMemoryCache` so the app still boots.
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

/// A cache that stores nothing and returns `None` for every read.
/// Use it in tests, or to turn caching off without touching call
/// sites.
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

    /// Nothing is stored, so there is nothing to delete. Must not fall
    /// through to the trait default, which errors.
    async fn delete_prefix(&self, _prefix: &str) -> Result<(), CacheError> {
        Ok(())
    }
}

// ------------------------------------------------------------------ InMemoryCache

struct CacheEntry {
    value: String,
    expires_at: Option<Instant>,
    /// Access tick for approximate LRU, bumped on every read and
    /// write. Atomic so a read, which holds only the read lock, can
    /// still update it.
    last_used: AtomicU64,
    /// `key.len() + value.len()`: this entry's share of the byte budget.
    size: usize,
}

impl CacheEntry {
    fn is_expired(&self) -> bool {
        self.expires_at.map_or(false, |t| Instant::now() > t)
    }
}

/// The map and its running byte total, behind one lock so they cannot
/// drift apart.
struct Store {
    map: HashMap<String, CacheEntry>,
    used_bytes: usize,
}

/// Default byte budget for [`InMemoryCache::new`]: 256 MiB. Large
/// enough that ordinary page and fragment caching never evicts, small
/// enough that a flood of unique keys (say `?cb=<random>`) cannot run
/// the process out of memory. Change or disable it with
/// [`InMemoryCache::with_max_bytes`].
pub const DEFAULT_MAX_BYTES: usize = 256 * 1024 * 1024;

/// Default entry-count cap for [`InMemoryCache::new`]. Bounds memory
/// and the eviction scan even when every entry is tiny. `0` turns it
/// off; see [`InMemoryCache::with_max_entries`].
pub const DEFAULT_MAX_ENTRIES: usize = 100_000;

/// Per-process cache over a `tokio::sync::RwLock<HashMap>`. Thread
/// safe, async friendly, no external dependencies.
///
/// TTLs are checked on read; there is no background reaper.
///
/// Size is bounded by [`DEFAULT_MAX_BYTES`] and
/// [`DEFAULT_MAX_ENTRIES`], so a flood of unique keys cannot grow the
/// process without limit. Eviction drops expired entries first, then
/// the least recently used, until both budgets are met. Change the
/// budgets with [`InMemoryCache::with_max_bytes`] and
/// [`InMemoryCache::with_max_entries`]; `0` means unbounded.
///
/// Build with [`InMemoryCache::with_default_ttl`] to give every
/// `set(_, _, None)` call a TTL.
pub struct InMemoryCache {
    inner: tokio::sync::RwLock<Store>,
    default_ttl: Option<Duration>,
    /// Byte budget; `0` means unbounded.
    max_bytes: usize,
    /// Entry-count budget; `0` means unbounded.
    max_entries: usize,
    /// Counter feeding each entry's `last_used`.
    tick: AtomicU64,
}

impl InMemoryCache {
    /// Cache with no default TTL and the default size budgets.
    #[must_use]
    pub fn new() -> Self {
        Self::build(None, DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES)
    }

    /// Cache where `set(key, value, None)` uses `default_ttl` instead
    /// of never expiring. Keeps the default budgets.
    #[must_use]
    pub fn with_default_ttl(default_ttl: Duration) -> Self {
        Self::build(Some(default_ttl), DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES)
    }

    /// Set the byte budget; `0` disables it. Chainable:
    /// `InMemoryCache::new().with_max_bytes(64 << 20)`.
    #[must_use]
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// Set the entry-count budget; `0` disables it. Chainable.
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

    /// Evict until both budgets are met: expired entries first, then
    /// the least recently used. The caller holds the write lock. One
    /// entry always survives, so a value bigger than the whole budget
    /// still caches.
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
                // LRU bump; the field is atomic, so a read lock is enough.
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

    /// Atomic increment. Holds the write lock across the whole
    /// read-modify-write, so two callers cannot lose an update. The
    /// counters built on this — account lockout, distributed lock,
    /// rate limiting — need that. Across replicas you still need
    /// `RedisCache`; a per-process cache cannot help there.
    async fn incr(&self, key: &str, by: i64, ttl: Option<Duration>) -> Result<i64, CacheError> {
        let tick = self.next_tick();
        let mut store = self.inner.write().await;
        let current = store
            .map
            .get(key)
            .filter(|e| !e.is_expired())
            .and_then(|e| e.value.parse::<i64>().ok())
            .unwrap_or(0);
        let new = current.saturating_add(by);
        let value = new.to_string();
        let size = key.len() + value.len();
        // Keep the existing expiry when the key is live and no TTL is
        // passed. That is the fixed-window behaviour counters need. A
        // supplied TTL, or a new or expired key, resets it.
        let expires_at = match store.map.get(key) {
            Some(e) if !e.is_expired() && ttl.is_none() => e.expires_at,
            _ => self.resolve_ttl(ttl),
        };
        if let Some(old) = store.map.remove(key) {
            store.used_bytes = store.used_bytes.saturating_sub(old.size);
        }
        store.used_bytes += size;
        store.map.insert(
            key.to_owned(),
            CacheEntry {
                value,
                expires_at,
                last_used: AtomicU64::new(tick),
                size,
            },
        );
        self.evict_locked(&mut store);
        Ok(new)
    }

    /// Atomic set-if-absent. Holds the write lock across the check and
    /// the insert, so it is a real test-and-set. `DistributedLock`
    /// builds its acquire on this.
    async fn add(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<bool, CacheError> {
        let tick = self.next_tick();
        let mut store = self.inner.write().await;
        if store.map.get(key).is_some_and(|e| !e.is_expired()) {
            return Ok(false);
        }
        let size = key.len() + value.len();
        if let Some(old) = store.map.remove(key) {
            store.used_bytes = store.used_bytes.saturating_sub(old.size);
        }
        store.used_bytes += size;
        store.map.insert(
            key.to_owned(),
            CacheEntry {
                value: value.to_owned(),
                expires_at: self.resolve_ttl(ttl),
                last_used: AtomicU64::new(tick),
                size,
            },
        );
        self.evict_locked(&mut store);
        Ok(true)
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

    /// Exact prefix delete: the map is right here, so it can be
    /// filtered. `used_bytes` drops by exactly what was removed, so
    /// the budget stays honest.
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

/// Cache on disk, one file per key.
///
/// Use it when you want a cache that survives a restart without
/// running Redis, and the working set fits on local disk. Keys are
/// SHA-256 hashed into filenames, so no key can produce a path
/// separator, an over-long name, or a case clash on macOS. The
/// directory is created on the first `set`.
///
/// ## File format
///
/// `[magic "RCF1"][expires_at: i64 BE epoch-millis][key_len: u32 BE]
/// [key bytes][value bytes]`. `expires_at = 0` means no TTL. Expired
/// entries are removed on the next `get` or `exists`; there is no
/// background reaper.
///
/// ## Limits
///
/// **Writes are not atomic, and a torn read does not always fail.**
/// `std::fs::write` truncates and then writes, so a reader can see a
/// partial file. A tear inside the header fails `decode`, and `get`
/// then drops the entry. But the value carries no length, so a tear
/// after the key returns `Some(_)` with a silently short value.
///
/// There is no per-entry lock file and no entry cap with a cull
/// strategy. Write-to-temp plus
/// rename, a value length, and file locking are tracked in #1530.
pub struct FileCache {
    dir: std::path::PathBuf,
}

impl FileCache {
    /// File-format marker. Bump the digit when the layout changes.
    /// An unknown magic makes the entry undecodable, and an
    /// undecodable entry is dropped on next access, so a bump migrates
    /// a cache directory on its own.
    const MAGIC: &'static [u8] = b"RCF1";

    /// Store entries under `dir`, which is created on the first `set`.
    #[must_use]
    pub fn new(dir: impl Into<std::path::PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory entries are stored under.
    #[must_use]
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// Hash the key into a stable, filesystem-safe filename: SHA-256
    /// in hex, so no separators and no length surprises.
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

    /// Epoch milliseconds. Seconds are too coarse: an entry stamped at
    /// second granularity can be born already expired.
    fn now_unix_millis() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| i64::try_from(d.as_millis()).ok())
            .unwrap_or(0)
    }

    /// Write one entry in the format described on [`FileCache`].
    ///
    /// The key itself is stored because the filename is only its
    /// SHA-256, so nothing about the key can be read back from disk.
    /// Without the key, `delete_prefix` would be impossible.
    ///
    /// Timestamps are in milliseconds. At second granularity a `set`
    /// at `T.999` stamps `T + 1` and expires a millisecond later, and
    /// any sub-second TTL rounds to zero and is born expired.
    fn encode(key: &str, value: &str, ttl: Option<Duration>) -> Vec<u8> {
        let expires_at = ttl
            .and_then(|d| i64::try_from(d.as_millis()).ok())
            .map(|ms| Self::now_unix_millis().saturating_add(ms))
            .unwrap_or(0);
        let key_len = u32::try_from(key.len()).unwrap_or(u32::MAX);
        let mut out = Vec::with_capacity(Self::MAGIC.len() + 12 + key.len() + value.len());
        out.extend_from_slice(Self::MAGIC);
        out.extend_from_slice(&expires_at.to_be_bytes());
        out.extend_from_slice(&key_len.to_be_bytes());
        out.extend_from_slice(key.as_bytes());
        out.extend_from_slice(value.as_bytes());
        out
    }

    /// Decode a file body into `(key, value, expired)`.
    ///
    /// Returns `None` for anything unreadable: a truncated write, an
    /// older format, a bad length. The caller then removes the file,
    /// which is how a format change migrates itself.
    ///
    /// Expiry uses `>`, not `>=`, so an entry stays live for the whole
    /// duration it was given.
    fn decode(buf: &[u8]) -> Option<(String, String, bool /* expired */)> {
        let rest = buf.strip_prefix(Self::MAGIC)?;
        if rest.len() < 12 {
            return None;
        }
        let mut ts = [0u8; 8];
        ts.copy_from_slice(&rest[..8]);
        let expires_at = i64::from_be_bytes(ts);

        let mut kl = [0u8; 4];
        kl.copy_from_slice(&rest[8..12]);
        let key_len = usize::try_from(u32::from_be_bytes(kl)).ok()?;

        let body = rest.get(12..)?;
        let key = std::str::from_utf8(body.get(..key_len)?).ok()?.to_owned();
        let value = std::str::from_utf8(body.get(key_len..)?).ok()?.to_owned();

        let expired = expires_at != 0 && Self::now_unix_millis() > expires_at;
        Some((key, value, expired))
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
            Some((_, _, true)) => {
                let _ = std::fs::remove_file(&path);
                Ok(None)
            }
            Some((_, v, false)) => Ok(Some(v)),
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
        std::fs::write(&path, Self::encode(key, value, ttl))
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

    /// Delete only the entries whose key starts with `prefix`.
    ///
    /// Filenames are hashes, so the key has to be read from inside
    /// each file. That makes this a full directory scan with one read
    /// per file. A prefix delete is a rare admin action, so the cost
    /// is fine.
    ///
    /// Expired and undecodable entries are removed along the way, as
    /// the scan has already paid for the read.
    async fn delete_prefix(&self, prefix: &str) -> Result<(), CacheError> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(CacheError::Connection(format!("read_dir: {e}"))),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("cache") {
                continue;
            }
            let Ok(buf) = std::fs::read(&path) else {
                continue;
            };
            match Self::decode(&buf) {
                // Matching entry, or one that is dead anyway.
                Some((key, _, expired)) if expired || key.starts_with(prefix) => {
                    let _ = std::fs::remove_file(&path);
                }
                // Another namespace's live entry — leave it alone.
                Some(_) => {}
                // Unreadable, or written by an older format: drop it.
                None => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "config"))]
mod settings_tests {
    use super::*;

    /// An unset backend gives an InMemoryCache. The concrete type is
    /// hidden, so check it by writing and reading back.
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

    /// `"null"` and `"none"` give a NullCache, so reads return None.
    #[tokio::test]
    async fn null_backend_drops_writes() {
        let mut s = crate::config::CacheSettings::default();
        s.backend = Some("null".into());
        let cache = from_settings(&s);
        cache.set("k", "v", None).await.unwrap();
        assert!(cache.get("k").await.unwrap().is_none());
    }

    /// An unknown name falls back to InMemoryCache, where writes land
    /// — unlike the null backend.
    #[tokio::test]
    async fn unknown_backend_falls_back_to_inmemory() {
        let mut s = crate::config::CacheSettings::default();
        s.backend = Some("typo".into());
        let cache = from_settings(&s);
        cache.set("k", "v", None).await.unwrap();
        assert_eq!(cache.get("k").await.unwrap().as_deref(), Some("v"));
    }

    /// The sync resolver must refuse `redis`, not hand back an
    /// in-memory cache. A working cache of the wrong kind is exactly
    /// what the caller cannot detect.
    #[test]
    #[should_panic(expected = "cannot be built by `from_settings`")]
    fn redis_backend_refuses_the_sync_resolver() {
        let mut s = crate::config::CacheSettings::default();
        s.backend = Some("redis".into());
        let _ = from_settings(&s);
    }

    /// Same for `db`, which settings cannot describe at all:
    /// `DatabaseCache` needs a runtime `&Pool`.
    #[test]
    #[should_panic(expected = "cannot be built from settings")]
    fn db_backend_refuses_the_sync_resolver() {
        let mut s = crate::config::CacheSettings::default();
        s.backend = Some("db".into());
        let _ = from_settings(&s);
    }

    /// The async resolver errors on a missing url instead of quietly
    /// swapping in another backend.
    #[tokio::test]
    async fn async_resolver_errors_on_redis_without_url() {
        let mut s = crate::config::CacheSettings::default();
        s.backend = Some("redis".into());
        // `expect_err` needs `BoxedCache: Debug`, which it is not.
        // Match instead of widening the trait just for a test.
        let Err(err) = from_settings_async(&s).await else {
            panic!("missing redis_url must be an error, not a fallback");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("redis"),
            "the error must name the backend it could not build: {msg}"
        );
    }

    /// The sync-buildable backends still work through it, so one
    /// resolver covers every case.
    #[tokio::test]
    async fn async_resolver_still_builds_the_sync_backends() {
        let mut s = crate::config::CacheSettings::default();
        s.backend = Some("memory".into());
        let cache = from_settings_async(&s).await.expect("memory builds");
        cache.set("k", "v", None).await.unwrap();
        assert_eq!(cache.get("k").await.unwrap().as_deref(), Some("v"));
    }
}

#[cfg(test)]
mod bound_tests {
    use super::*;

    fn val(n: usize) -> String {
        "x".repeat(n)
    }

    /// A flood of unique keys never grows the cache past `max_bytes`.
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

    /// The entry-count budget works on its own, without the byte one.
    #[tokio::test]
    async fn entry_budget_caps_count() {
        let cache = InMemoryCache::new().with_max_bytes(0).with_max_entries(5);
        for i in 0..50 {
            cache.set(&format!("k{i}"), "v", None).await.unwrap();
        }
        assert!(cache.inner.read().await.map.len() <= 5);
    }

    /// A key kept warm by reads survives a flood that evicts colder
    /// keys.
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

    /// A budget of `0` means no limit.
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
