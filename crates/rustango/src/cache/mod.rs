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

#[cfg(feature = "cache-redis")]
pub mod redis_backend;

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
}

/// `Arc<dyn Cache>` alias — the standard way to share a cache instance.
pub type BoxedCache = Arc<dyn Cache>;

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
