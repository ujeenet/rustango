//! Redis cache backend — [`RedisCache`].
//!
//! Backed by `redis::aio::ConnectionManager` which maintains a single
//! multiplexed async connection and transparently reconnects on failure.
//!
//! ## Usage
//!
//! ```ignore
//! use rustango::cache::redis_backend::RedisCache;
//! use rustango::cache::{Cache, BoxedCache};
//! use std::sync::Arc;
//! use std::time::Duration;
//!
//! let cache: BoxedCache = Arc::new(
//!     RedisCache::new("redis://127.0.0.1/").await?
//! );
//! cache.set("key", "value", Some(Duration::from_secs(300))).await?;
//! ```

use std::time::Duration;

use async_trait::async_trait;
use redis::AsyncCommands;

use super::{Cache, CacheError};

/// Redis-backed async cache using a multiplexed connection manager.
///
/// Stores all values as UTF-8 strings (raw or JSON-encoded via [`super::set_json`]).
/// TTL maps directly to Redis `SETEX` / `SET EX`.
pub struct RedisCache {
    conn: redis::aio::ConnectionManager,
    default_ttl: Option<Duration>,
}

impl RedisCache {
    /// Connect to Redis at `url` (e.g. `"redis://127.0.0.1/"`) with no
    /// default TTL.
    ///
    /// # Errors
    /// [`CacheError::Connection`] when the initial connection fails.
    pub async fn new(url: &str) -> Result<Self, CacheError> {
        Self::with_default_ttl(url, None).await
    }

    /// Connect to Redis with a default TTL applied to every `set` call
    /// that passes `ttl = None`.
    ///
    /// # Errors
    /// [`CacheError::Connection`] when the initial connection fails.
    pub async fn with_default_ttl(url: &str, default_ttl: Option<Duration>) -> Result<Self, CacheError> {
        let client = redis::Client::open(url)
            .map_err(|e| CacheError::Connection(e.to_string()))?;
        let conn = redis::aio::ConnectionManager::new(client)
            .await
            .map_err(|e| CacheError::Connection(e.to_string()))?;
        Ok(Self { conn, default_ttl })
    }

    fn effective_ttl(&self, ttl: Option<Duration>) -> Option<u64> {
        ttl.or(self.default_ttl).map(|d| d.as_secs().max(1))
    }
}

#[async_trait]
impl Cache for RedisCache {
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError> {
        let mut conn = self.conn.clone();
        conn.get::<_, Option<String>>(key)
            .await
            .map_err(|e| CacheError::Connection(e.to_string()))
    }

    async fn set(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<(), CacheError> {
        let mut conn = self.conn.clone();
        match self.effective_ttl(ttl) {
            Some(secs) => {
                conn.set_ex::<_, _, ()>(key, value, secs)
                    .await
                    .map_err(|e| CacheError::Connection(e.to_string()))
            }
            None => {
                conn.set::<_, _, ()>(key, value)
                    .await
                    .map_err(|e| CacheError::Connection(e.to_string()))
            }
        }
    }

    async fn delete(&self, key: &str) -> Result<(), CacheError> {
        let mut conn = self.conn.clone();
        conn.del::<_, ()>(key)
            .await
            .map_err(|e| CacheError::Connection(e.to_string()))
    }

    async fn exists(&self, key: &str) -> Result<bool, CacheError> {
        let mut conn = self.conn.clone();
        conn.exists::<_, bool>(key)
            .await
            .map_err(|e| CacheError::Connection(e.to_string()))
    }

    async fn clear(&self) -> Result<(), CacheError> {
        let mut conn = self.conn.clone();
        redis::cmd("FLUSHDB")
            .query_async::<()>(&mut conn)
            .await
            .map_err(|e| CacheError::Connection(e.to_string()))
    }
}
