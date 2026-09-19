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
    /// **Every shipped backend that can enumerate overrides this**, and
    /// a new backend that can MUST: [`InMemoryCache`] filters its map,
    /// [`DatabaseCache`] issues a `DELETE … WHERE cache_key LIKE
    /// 'prefix%'`, `RedisCache` runs `SCAN MATCH` + `DEL`, and
    /// [`NullCache`] has nothing to delete. Redis is the cautionary
    /// one: its `clear()` is `FLUSHDB`, so inheriting this default
    /// would let one tenant's invalidation wipe every other tenant,
    /// every rate-limit counter and every lock key.
    async fn delete_prefix(&self, prefix: &str) -> Result<(), CacheError> {
        // Fails rather than clearing everything.
        //
        // This used to call `self.clear()` behind a `tracing::warn!`,
        // which meant a backend that simply had not implemented this
        // would delete *other namespaces'* data on a scoped clear —
        // `ScopedCache::clear()` routes here, so one tenant wiped the
        // rest while `docs/caching.md` said it wiped only its own. A
        // warning is not consent, and the caller is not the one who
        // chose the backend.
        //
        // Every backend in-tree overrides this, so reaching it means a
        // third-party `impl Cache` that has not considered the
        // question. Erring gives that implementor a loud, local failure
        // instead of silent data loss in someone else's namespace.
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
/// - `"file"` → [`FileCache`] (needs `file_cache_dir`; warns and uses
///   [`InMemoryCache`] without it — a cache that is merely colder, not
///   a different guarantee)
/// - `"redis"`, `"db"` / `"database"` → **panics**. Both need async
///   construction, so this resolver cannot build them, and returning
///   something else is what #1400 was.
/// - any other value → [`InMemoryCache`] with a warning (typo defense)
///
/// # Panics
/// On `backend = "redis"` or `"db"`. Use
/// [`from_settings_async`] for redis, and build the DB backend where
/// the `Pool` is. The message says which.
///
/// It used to warn and hand back an [`InMemoryCache`] for those two.
/// The caller got a working cache with no way to tell which backend it
/// held — and a per-process cache is not a degraded shared one, it is a
/// different one. `CacheRateLimitLayer` then multiplies its limit by the
/// replica count, and `verify_single_use` stops failing closed.
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
        // `redis` and `db` both need async construction, so this sync
        // resolver cannot build either — and must not pretend (#1400).
        //
        // It used to warn and hand back an `InMemoryCache`. The caller
        // got a working `BoxedCache` with no way to tell which backend
        // it held, which matters because a per-process cache is not a
        // degraded shared one: `CacheRateLimitLayer` multiplies the
        // limit by the replica count, and `verify_single_use` stops
        // failing closed — the same reset link works once per replica.
        // Both of those are documented as working *because* the cache
        // is shared. A warning at boot does not reach the person
        // debugging that days later.
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
        // #409 — `DatabaseCache` needs a runtime `Pool` and an async
        // `ensure_table()`. Unlike redis, settings alone cannot describe
        // it, so there is no async resolver for this one either.
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

/// [`from_settings`], but able to build the backends that need to
/// connect — today that means `redis` (#1400).
///
/// Reach for this one wherever you can `.await`. It is the only way to
/// get the backend `[cache] backend = "redis"` actually asks for; the
/// sync resolver panics rather than hand back something else.
///
/// `db` is still not buildable from settings and returns an error
/// saying so: `DatabaseCache` needs a runtime `&Pool`, which `[cache]`
/// does not carry. That is a fact about the backend, not a limitation
/// of this function.
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
/// Returns [`CacheError`] when the backend is configured but cannot be
/// reached or built — an unreachable Redis, a missing `redis_url`, or
/// `db`, which needs a pool.
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
        // Everything else is buildable synchronously and behaves identically.
        _ => Ok(from_settings(s)),
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

    /// Atomic increment (#1253). The trait default is get-parse-set,
    /// which races two concurrent callers into a lost update — fatal
    /// for the counters built on it (account lockout, distributed lock,
    /// rate limiting). This holds the single write lock across the whole
    /// read-modify-write, so an in-process increment is atomic. (Across
    /// replicas you still need `RedisCache`, whose `INCRBY` is atomic on
    /// the server; a per-process cache cannot help there.)
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
        // Preserve the existing expiry when the key is live and no new
        // TTL is given, matching the fixed-window semantics counters
        // rely on; a supplied TTL (or a fresh/expired key) resets it.
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

    /// Atomic set-if-absent (#1254). The trait default is a racy
    /// `exists` then `set`; this holds the single write lock across the
    /// check and the insert, so it is a genuine test-and-set — the
    /// primitive `DistributedLock` acquire is built on.
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
///   `[expires_at_unix_millis: i64 big-endian][value bytes]`
///
/// `expires_at_unix_millis` is `0` when the entry has no TTL. Expired
/// entries are pruned lazily on the next `get` / `exists` call —
/// there is no background reaper.
///
/// The header held **seconds** before #1233, which made sub-second TTLs
/// unrepresentable and let a 1-second entry expire immediately. Entries
/// written by an older build decode as long-past and are treated as
/// expired, so upgrading costs one cold read per stale key — the safe
/// direction for a cache.
///
/// ## Limitations vs Django
///
/// Django's FBC takes a `_lock` file for atomic multi-process writes
/// + supports MAX_ENTRIES with a cull strategy. This implementation
/// is the minimal Django-shape primitive with the same on-disk
/// semantics.
///
/// **Writes are not atomic, and a torn read does not always fail.**
/// `std::fs::write` is `O_TRUNC` followed by a write, on every common
/// filesystem, so a concurrent reader can see a partial file. What
/// happens next depends on where the tear lands: the header is
/// length-checked, so a tear inside it makes `decode` return `None`
/// and `get` delete the entry — but `encode` writes no length for the
/// **value**, and `decode` takes `body[key_len..]` verbatim. A tear
/// after the key therefore yields `Some(_)` with a silently truncated
/// value, which is worse than the miss, because nothing signals it.
///
/// This originally claimed "per-process atomicity via `std::fs::write`
/// (atomic per-call on most filesystems)", sitting directly under a
/// correct note about Django's lock file, which made it read as
/// considered rather than assumed (#1543). The first correction then
/// said a torn read "fails decode", which is only half true (#1606
/// review). Write-to-temp plus rename, a value length, and file
/// locking for a shared directory are #1530.
pub struct FileCache {
    dir: std::path::PathBuf,
}

impl FileCache {
    /// File-format marker. Bump the digit if the layout changes again:
    /// an unrecognised magic makes the entry undecodable, and an
    /// undecodable entry is discarded on next access, so a version bump
    /// migrates a cache directory by itself.
    const MAGIC: &'static [u8] = b"RCF1";

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

    /// Epoch **milliseconds**. Seconds were too coarse: an entry whose
    /// TTL was stamped at second granularity could be born already
    /// expired (#1233).
    fn now_unix_millis() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| i64::try_from(d.as_millis()).ok())
            .unwrap_or(0)
    }

    /// Encode `[magic "RCF1"][expires_at: i64 BE epoch-millis][key_len:
    /// u32 BE][key bytes][value bytes]`. `expires_at = 0` means no TTL.
    ///
    /// Milliseconds, not seconds. At second granularity a `set` landing
    /// at wall-clock `T.999` stamped `expires_at = T + 1`, and the read
    /// a millisecond later was already at `T+1` — so a 1-second TTL
    /// could expire in one millisecond, and any sub-second TTL rounded
    /// to `as_secs() == 0` and was born expired (#1233).
    ///
    /// The key is stored because the filename cannot carry it: it is a
    /// SHA-256 of the key, so nothing about the original is recoverable
    /// from disk. Without it `delete_prefix` is not merely unimplemented
    /// but *impossible*, and the trait's default — clear everything —
    /// meant one tenant's `ScopedCache::clear()` wiped every other
    /// tenant's entries.
    ///
    /// The `RCF1` magic makes pre-#1400 files fail to decode rather than
    /// be misread as `[key_len][key]`. They are then treated like any
    /// unreadable entry and removed on next access, so an existing cache
    /// directory self-heals with no migration step — which is safe
    /// precisely because this is a cache.
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

    /// Decode the file body into `(key, value, expired)`.
    ///
    /// Returns `None` for anything unreadable — a truncated write, a
    /// pre-`RCF1` file, a bad length. The caller removes the file in
    /// that case, which is how the format change migrates itself.
    ///
    /// Expiry is `>`, not `>=`: an entry is live for the full duration
    /// it was promised, rather than dying on the boundary tick.
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
    /// This is what makes `ScopedCache::clear()` honest on a file cache.
    /// Without the override it fell through to the trait default, which
    /// calls `clear()` — so one tenant's clear removed every tenant's
    /// entries while the docs said it removed only its own.
    ///
    /// Filenames are SHA-256 of the key and carry nothing recoverable,
    /// so the match has to come from inside each file. That makes this
    /// O(entries): a full directory scan, one read per file. Acceptable
    /// because a prefix delete is a rare administrative act, and the
    /// alternative was deleting other namespaces' data.
    ///
    /// Expired and undecodable entries are removed as they are passed —
    /// the scan is already paying for the read.
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
                // Unreadable or written by an older format: evict.
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

    /// #1400 — the sync resolver must refuse `redis` rather than hand
    /// back an in-memory cache.
    ///
    /// This test replaced one asserting the opposite. That test was
    /// pinning the bug: it required a *working* cache back, which the
    /// fallback provided, and a working cache is exactly what made the
    /// wrong backend undetectable to the caller.
    #[test]
    #[should_panic(expected = "cannot be built by `from_settings`")]
    fn redis_backend_refuses_the_sync_resolver() {
        let mut s = crate::config::CacheSettings::default();
        s.backend = Some("redis".into());
        let _ = from_settings(&s);
    }

    /// Same for `db`, which additionally cannot be described by
    /// settings at all — `DatabaseCache` needs a runtime `&Pool`.
    #[test]
    #[should_panic(expected = "cannot be built from settings")]
    fn db_backend_refuses_the_sync_resolver() {
        let mut s = crate::config::CacheSettings::default();
        s.backend = Some("db".into());
        let _ = from_settings(&s);
    }

    /// The async resolver reports a missing url as an error rather than
    /// substituting a backend — the whole point of #1400.
    #[tokio::test]
    async fn async_resolver_errors_on_redis_without_url() {
        let mut s = crate::config::CacheSettings::default();
        s.backend = Some("redis".into());
        // `expect_err` would need `BoxedCache: Debug`, and `Arc<dyn Cache>`
        // is not — match instead of loosening the trait for a test.
        let Err(err) = from_settings_async(&s).await else {
            panic!("missing redis_url must be an error, not a fallback");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("redis"),
            "the error must name the backend it could not build: {msg}"
        );
    }

    /// And the backends it *can* build synchronously still work through
    /// it, so callers can use one resolver everywhere.
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
