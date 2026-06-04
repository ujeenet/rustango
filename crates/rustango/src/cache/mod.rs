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
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;

#[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
pub mod db_backend;
#[cfg(feature = "cache-redis")]
pub mod redis_backend;

#[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
pub use db_backend::DatabaseCache;

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
    /// Returns 0 if the existing value isn't a valid integer (the entry
    /// is overwritten with `by` in that case).
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
}

// ------------------------------------------------------------------ InMemoryCache

struct CacheEntry {
    value: String,
    expires_at: Option<Instant>,
}

impl CacheEntry {
    fn is_expired(&self) -> bool {
        self.expires_at.map_or(false, |t| Instant::now() > t)
    }
}

/// A per-process in-memory cache backed by a `tokio::sync::RwLock<HashMap>`.
///
/// - Thread-safe, async-friendly, zero external dependencies.
/// - TTL is enforced lazily on reads (no background eviction thread).
/// - `clear()` removes all entries; expired entries accumulate until the
///   key is read or cleared. For long-running processes with many unique
///   keys, call `clear()` periodically or use the Redis backend.
///
/// # Optional default TTL
///
/// Build with [`InMemoryCache::with_default_ttl`] to apply a TTL to every
/// `set` call that passes `ttl = None`.
pub struct InMemoryCache {
    inner: tokio::sync::RwLock<HashMap<String, CacheEntry>>,
    default_ttl: Option<Duration>,
}

impl InMemoryCache {
    /// Create a cache with no default TTL (entries live forever unless
    /// explicitly given a TTL or removed).
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: tokio::sync::RwLock::new(HashMap::new()),
            default_ttl: None,
        }
    }

    /// Create a cache where every `set(key, value, None)` call uses
    /// `default_ttl` instead of "no expiry".
    #[must_use]
    pub fn with_default_ttl(default_ttl: Duration) -> Self {
        Self {
            inner: tokio::sync::RwLock::new(HashMap::new()),
            default_ttl: Some(default_ttl),
        }
    }

    fn resolve_ttl(&self, ttl: Option<Duration>) -> Option<Instant> {
        let effective = ttl.or(self.default_ttl)?;
        Some(Instant::now() + effective)
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
        let map = self.inner.read().await;
        Ok(map.get(key).and_then(|e| {
            if e.is_expired() {
                None
            } else {
                Some(e.value.clone())
            }
        }))
    }

    async fn set(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<(), CacheError> {
        let expires_at = self.resolve_ttl(ttl);
        let mut map = self.inner.write().await;
        map.insert(
            key.to_owned(),
            CacheEntry {
                value: value.to_owned(),
                expires_at,
            },
        );
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), CacheError> {
        self.inner.write().await.remove(key);
        Ok(())
    }

    async fn exists(&self, key: &str) -> Result<bool, CacheError> {
        let map = self.inner.read().await;
        Ok(map.get(key).map_or(false, |e| !e.is_expired()))
    }

    async fn clear(&self) -> Result<(), CacheError> {
        self.inner.write().await.clear();
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
