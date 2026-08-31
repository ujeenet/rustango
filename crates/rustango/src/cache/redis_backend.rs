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

    /// Atomic set-if-absent via `SET key value NX [EX secs]` (#1254).
    /// The default `add` is a racy `exists` + `set`; `NX` makes the
    /// server do the test-and-set in one round trip, which is what makes
    /// `DistributedLock` safe across replicas. Returns `true` when this
    /// call created the key.
    async fn add(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<bool, CacheError> {
        let mut conn = self.conn.clone();
        let mut cmd = redis::cmd("SET");
        cmd.arg(key).arg(value).arg("NX");
        if let Some(secs) = self.effective_ttl(ttl) {
            cmd.arg("EX").arg(secs);
        }
        // `SET ... NX` replies with the string "OK" on success and a nil
        // bulk on a no-op (key already existed). `Option<String>`
        // decodes that as `Some("OK")` / `None`.
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

    /// `SCAN MATCH <prefix>*` + `DEL`, in cursor batches.
    ///
    /// **This override is load-bearing.** Without it the trait default
    /// falls through to [`Self::clear`], which is `FLUSHDB` — so a
    /// single tenant's [`ScopedCache::clear`](super::ScopedCache) would
    /// wipe every other tenant's entries, every rate-limit counter, and
    /// every `lock:*` key (letting two replicas both take a
    /// "once per cluster" lock). Redis can enumerate, so the trait
    /// default's "cannot enumerate, so over-delete" bargain does not
    /// apply here (#1227).
    ///
    /// `SCAN` is non-blocking and cursor-based, unlike `KEYS`: it will
    /// not stall the server on a large keyspace. The trade-off is that
    /// it gives no snapshot guarantee — keys created *during* the sweep
    /// may be missed. That is the right trade for cache invalidation
    /// (a missed key is a stale entry that still expires on its TTL,
    /// and the alternative blocks every other client).
    ///
    /// `MATCH` takes a glob, not a literal, so `*`, `?`, `[`, `]` and
    /// `\` in the prefix are escaped — otherwise a prefix containing one
    /// would match beyond its own namespace.
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

            // A zero cursor means the iteration completed. It is only
            // valid to stop here — a non-empty batch does not imply
            // more, and an empty one does not imply done.
            if next == 0 {
                return Ok(());
            }
            cursor = next;
        }
    }

    async fn incr(&self, key: &str, by: i64, ttl: Option<Duration>) -> Result<i64, CacheError> {
        let mut conn = self.conn.clone();
        let new: i64 = redis::cmd("INCRBY")
            .arg(key)
            .arg(by)
            .query_async(&mut conn)
            .await
            .map_err(|e| CacheError::Connection(e.to_string()))?;
        // EXPIRE on first creation only — INCR-then-EXPIRE on every call
        // would reset the window each tick, breaking fixed-window rate
        // limiters. The NX flag is exactly the "set TTL only if no TTL"
        // semantic we want.
        if let Some(secs) = self.effective_ttl(ttl) {
            let _: i64 = redis::cmd("EXPIRE")
                .arg(key)
                .arg(secs)
                .arg("NX")
                .query_async(&mut conn)
                .await
                .map_err(|e| CacheError::Connection(e.to_string()))?;
        }
        Ok(new)
    }
}
