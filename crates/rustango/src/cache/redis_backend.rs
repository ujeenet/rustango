//! Redis cache backend: [`RedisCache`].
//!
//! Built on `redis::aio::ConnectionManager`, which keeps one
//! multiplexed async connection and reconnects on its own.
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

/// Async cache backed by Redis.
///
/// Values are UTF-8 strings, either raw or JSON from
/// [`super::set_json`]. A TTL becomes `SET EX`.
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
    pub async fn with_default_ttl(
        url: &str,
        default_ttl: Option<Duration>,
    ) -> Result<Self, CacheError> {
        let client = redis::Client::open(url).map_err(|e| CacheError::Connection(e.to_string()))?;
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
            Some(secs) => conn
                .set_ex::<_, _, ()>(key, value, secs)
                .await
                .map_err(|e| CacheError::Connection(e.to_string())),
            None => conn
                .set::<_, _, ()>(key, value)
                .await
                .map_err(|e| CacheError::Connection(e.to_string())),
        }
    }

    /// Atomic set-if-absent via `SET key value NX [EX secs]`. `NX`
    /// makes the server do the test-and-set in one round trip, which
    /// is what makes `DistributedLock` safe across replicas. Returns
    /// `true` when this call created the key.
    async fn add(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<bool, CacheError> {
        let mut conn = self.conn.clone();
        let mut cmd = redis::cmd("SET");
        cmd.arg(key).arg(value).arg("NX");
        if let Some(secs) = self.effective_ttl(ttl) {
            cmd.arg("EX").arg(secs);
        }
        // `SET … NX` replies "OK" on success and nil when the key
        // already existed, which decodes as `Some`/`None`.
        let reply: Option<String> = cmd
            .query_async(&mut conn)
            .await
            .map_err(|e| CacheError::Connection(e.to_string()))?;
        Ok(reply.is_some())
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

    /// `SCAN MATCH <prefix>*` then `DEL`, batch by batch.
    ///
    /// `SCAN` is cursor-based and does not block the server on a large
    /// keyspace, unlike `KEYS`. In exchange it gives no snapshot: a
    /// key created during the sweep may be missed. That is the right
    /// trade for cache invalidation, since a missed key still expires
    /// on its own TTL.
    ///
    /// `MATCH` takes a glob, so `*`, `?`, `[`, `]` and `\` in the
    /// prefix are escaped. Otherwise a prefix holding one of them
    /// would match outside its namespace.
    async fn delete_prefix(&self, prefix: &str) -> Result<(), CacheError> {
        let mut conn = self.conn.clone();
        let mut pattern = String::with_capacity(prefix.len() + 1);
        for ch in prefix.chars() {
            if matches!(ch, '*' | '?' | '[' | ']' | '\\') {
                pattern.push('\\');
            }
            pattern.push(ch);
        }
        pattern.push('*');

        let mut cursor: u64 = 0;
        loop {
            let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(512)
                .query_async(&mut conn)
                .await
                .map_err(|e| CacheError::Connection(format!("scan: {e}")))?;

            if !keys.is_empty() {
                redis::cmd("DEL")
                    .arg(&keys)
                    .query_async::<()>(&mut conn)
                    .await
                    .map_err(|e| CacheError::Connection(format!("del: {e}")))?;
            }

            // Only a zero cursor means the scan is finished. An empty
            // batch does not, and a full one does not mean more.
            if next == 0 {
                return Ok(());
            }
            cursor = next;
        }
    }

    async fn incr(&self, key: &str, by: i64, ttl: Option<Duration>) -> Result<i64, CacheError> {
        let mut conn = self.conn.clone();
        // Increment, and set the TTL only if the key has none, in one
        // atomic server-side step.
        //
        // Setting the TTL only when the key is created is what makes a
        // fixed-window rate limiter work. An EXPIRE on every tick
        // would slide the window forward and the limit would never be
        // reached.
        //
        // Lua rather than `EXPIRE … NX`, which needs Redis 7.0. `EVAL`
        // works from 2.6, so this also runs on ElastiCache and other
        // Redis-compatible servers. After the INCRBY the key always
        // exists, so a `TTL` below 0 means no expiry is set.
        let script = redis::Script::new(
            r"local n = redis.call('INCRBY', KEYS[1], ARGV[1])
              if tonumber(ARGV[2]) > 0 and redis.call('TTL', KEYS[1]) < 0 then
                redis.call('EXPIRE', KEYS[1], ARGV[2])
              end
              return n",
        );
        script
            .key(key)
            .arg(by)
            .arg(self.effective_ttl(ttl).unwrap_or(0))
            .invoke_async(&mut conn)
            .await
            .map_err(|e| CacheError::Connection(e.to_string()))
    }
}
