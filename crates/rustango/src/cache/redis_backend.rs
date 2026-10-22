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

/// Retries after a failed connect attempt, each at most a ~1 s (jittered) wait.
const CONNECT_RETRIES: usize = 2;
const RETRY_MAX_DELAY_MS: u64 = 1_000;

/// How long one connect attempt and one command may take. Never unbounded:
/// a stalled Redis would hang every cache call for minutes of TCP retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RedisTimeouts {
    /// One connect attempt, handshake included. Up to three attempts are made.
    pub connect: Duration,
    /// One command, from send to reply.
    pub response: Duration,
}

impl RedisTimeouts {
    /// Connect within `connect`, answer within `response`.
    #[must_use]
    pub const fn new(connect: Duration, response: Duration) -> Self {
        Self { connect, response }
    }
}

impl Default for RedisTimeouts {
    /// 5 s to connect, 2 s per command.
    fn default() -> Self {
        Self::new(Duration::from_secs(5), Duration::from_secs(2))
    }
}

/// redis 0.27 otherwise backs off up to 60 s between 6 retries, and a
/// call waits on the reconnect: minutes of stall per outage.
fn manager_config(timeouts: RedisTimeouts) -> redis::aio::ConnectionManagerConfig {
    redis::aio::ConnectionManagerConfig::new()
        .set_connection_timeout(timeouts.connect)
        .set_response_timeout(timeouts.response)
        .set_number_of_retries(CONNECT_RETRIES)
        .set_max_delay(RETRY_MAX_DELAY_MS)
}

/// Async cache backed by Redis.
///
/// Values are UTF-8 strings, or raw bytes through `set_bytes`.
/// A TTL becomes `SET PX`.
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
        Self::with_timeouts(url, default_ttl, RedisTimeouts::default()).await
    }

    /// [`Self::with_default_ttl`] with explicit connect and response timeouts.
    ///
    /// # Errors
    /// [`CacheError::Connection`] when the initial connection fails or times out.
    pub async fn with_timeouts(
        url: &str,
        default_ttl: Option<Duration>,
        timeouts: RedisTimeouts,
    ) -> Result<Self, CacheError> {
        let client = redis::Client::open(url).map_err(|e| CacheError::Connection(e.to_string()))?;
        let conn = redis::aio::ConnectionManager::new_with_config(client, manager_config(timeouts))
            .await
            .map_err(|e| CacheError::Connection(e.to_string()))?;
        Ok(Self { conn, default_ttl })
    }

    /// In milliseconds, as `PX`/`PEXPIRE` take it: whole seconds cut a
    /// 1500 ms TTL to 1 s (#1677).
    fn effective_ttl_ms(&self, ttl: Option<Duration>) -> Option<u64> {
        ttl.or(self.default_ttl).map(ttl_ms)
    }
}

/// Redis adds now to the TTL and rejects a sum past `i64::MAX`, so cap well below it.
const MAX_TTL_MS: u64 = i64::MAX as u64 / 2;

fn ttl_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis())
        .unwrap_or(u64::MAX)
        .clamp(1, MAX_TTL_MS)
}

#[async_trait]
impl Cache for RedisCache {
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError> {
        let mut conn = self.conn.clone();
        conn.get::<_, Option<String>>(key)
            .await
            .map_err(|e| CacheError::Connection(e.to_string()))
    }

    async fn get_bytes(&self, key: &str) -> Result<Option<bytes::Bytes>, CacheError> {
        let mut conn = self.conn.clone();
        conn.get::<_, Option<Vec<u8>>>(key)
            .await
            .map(|v| v.map(Into::into))
            .map_err(|e| CacheError::Connection(e.to_string()))
    }

    async fn set(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<(), CacheError> {
        self.set_bytes(key, value.as_bytes(), ttl).await
    }

    /// Raw bytes: Redis strings are binary-safe.
    async fn set_bytes(
        &self,
        key: &str,
        value: &[u8],
        ttl: Option<Duration>,
    ) -> Result<(), CacheError> {
        let mut conn = self.conn.clone();
        match self.effective_ttl_ms(ttl) {
            Some(ms) => conn
                .pset_ex::<_, _, ()>(key, value, ms)
                .await
                .map_err(|e| CacheError::Connection(e.to_string())),
            None => conn
                .set::<_, _, ()>(key, value)
                .await
                .map_err(|e| CacheError::Connection(e.to_string())),
        }
    }

    async fn set_forever(&self, key: &str, value: &str) -> Result<(), CacheError> {
        let mut conn = self.conn.clone();
        conn.set::<_, _, ()>(key, value)
            .await
            .map_err(|e| CacheError::Connection(e.to_string()))
    }

    /// Atomic set-if-absent via `SET key value NX [PX ms]`. `NX`
    /// makes the server do the test-and-set in one round trip, which
    /// is what makes `DistributedLock` safe across replicas. Returns
    /// `true` when this call created the key.
    async fn add(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<bool, CacheError> {
        let mut conn = self.conn.clone();
        let mut cmd = redis::cmd("SET");
        cmd.arg(key).arg(value).arg("NX");
        if let Some(ms) = self.effective_ttl_ms(ttl) {
            cmd.arg("PX").arg(ms);
        }
        // `SET … NX` replies "OK" on success and nil when the key
        // already existed, which decodes as `Some`/`None`.
        let reply: Option<String> = cmd
            .query_async(&mut conn)
            .await
            .map_err(|e| CacheError::Connection(e.to_string()))?;
        Ok(reply.is_some())
    }

    /// `PEXPIRE` / `PERSIST` never create a key, so a racing `delete` stays deleted (#2300).
    async fn touch(&self, key: &str, ttl: Option<Duration>) -> Result<bool, CacheError> {
        let mut conn = self.conn.clone();
        let err = |e: redis::RedisError| CacheError::Connection(e.to_string());
        match self.effective_ttl_ms(ttl) {
            Some(ms) => redis::cmd("PEXPIRE")
                .arg(key)
                .arg(ms)
                .query_async::<bool>(&mut conn)
                .await
                .map_err(err),
            // PERSIST answers 0 for a key without a TTL too, so ask EXISTS.
            None => {
                redis::cmd("PERSIST")
                    .arg(key)
                    .query_async::<()>(&mut conn)
                    .await
                    .map_err(err)?;
                conn.exists::<_, bool>(key).await.map_err(err)
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

    /// `ASYNC` replies at once; a sync flush of a big DB outlasts the command timeout.
    async fn clear(&self) -> Result<(), CacheError> {
        let mut conn = self.conn.clone();
        redis::cmd("FLUSHDB")
            .arg("ASYNC")
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
        // exists, so a `PTTL` below 0 means no expiry is set.
        let script = redis::Script::new(
            r"local n = redis.call('INCRBY', KEYS[1], ARGV[1])
              if tonumber(ARGV[2]) > 0 and redis.call('PTTL', KEYS[1]) < 0 then
                redis.call('PEXPIRE', KEYS[1], ARGV[2])
              end
              return n",
        );
        script
            .key(key)
            .arg(by)
            .arg(self.effective_ttl_ms(ttl).unwrap_or(0))
            .invoke_async(&mut conn)
            .await
            .map_err(|e| CacheError::Connection(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A huge TTL must stay a valid `PX`/`PEXPIRE` argument.
    #[test]
    fn ttl_ms_is_clamped_to_a_valid_redis_expire() {
        assert_eq!(ttl_ms(Duration::MAX), MAX_TTL_MS);
        assert!(ttl_ms(Duration::MAX) <= i64::MAX as u64);
        assert_eq!(ttl_ms(Duration::from_millis(1500)), 1500);
        assert_eq!(ttl_ms(Duration::ZERO), 1);
    }

    /// The retry cap and backoff ceiling reach the connection manager.
    #[test]
    fn manager_config_caps_retries_and_backoff() {
        let t = Duration::from_millis(100);
        let config = format!("{:?}", manager_config(RedisTimeouts::new(t, t)));
        assert!(config.contains("number_of_retries: 2"), "{config}");
        assert!(config.contains("max_delay: Some(1000)"), "{config}");
    }

    /// Reads RESP commands and answers `+OK`, recording each one.
    async fn ok_server() -> (String, std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>) {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("redis://{}/", listener.local_addr().unwrap());
        let seen = std::sync::Arc::<std::sync::Mutex<Vec<Vec<String>>>>::default();
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let log = log.clone();
                tokio::spawn(async move {
                    let (r, mut w) = sock.into_split();
                    let mut r = BufReader::new(r);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let n: usize = line.trim()[1..].parse().unwrap();
                        let mut cmd = Vec::new();
                        for _ in 0..n {
                            line.clear();
                            r.read_line(&mut line).await.unwrap();
                            let len: usize = line.trim()[1..].parse().unwrap();
                            let mut buf = vec![0; len + 2];
                            r.read_exact(&mut buf).await.unwrap();
                            buf.truncate(len);
                            cmd.push(String::from_utf8(buf).unwrap());
                        }
                        log.lock().unwrap().push(cmd);
                        w.write_all(b"+OK\r\n").await.unwrap();
                    }
                });
            }
        });
        (url, seen)
    }

    /// `clear` must not wait on a large flush: the command timeout would fail it.
    #[tokio::test]
    async fn clear_flushes_asynchronously() {
        let (url, seen) = ok_server().await;
        let cache = RedisCache::new(&url).await.expect("connect");
        cache.clear().await.expect("clear");
        let seen = seen.lock().unwrap();
        let flush = seen.iter().find(|c| c[0].eq_ignore_ascii_case("FLUSHDB"));
        assert_eq!(flush.map(|c| &c[1..]), Some(&["ASYNC".to_owned()][..]));
    }

    /// A server that accepts but never answers fails the connect, not hangs it.
    #[tokio::test]
    async fn a_silent_server_fails_the_connect() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("redis://{}/", listener.local_addr().unwrap());
        let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = accepted.clone();
        let _accept = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                held.push(sock);
            }
        });
        let t = Duration::from_millis(100);
        let start = std::time::Instant::now();
        let built = tokio::time::timeout(
            Duration::from_secs(30),
            RedisCache::with_timeouts(&url, None, RedisTimeouts::new(t, t)),
        )
        .await
        .expect("connect hung past its timeout");
        assert!(built.is_err(), "connected to a silent server");
        // Three 100 ms attempts and two ~1 s retry waits.
        assert!(start.elapsed() < Duration::from_secs(3), "{:?}", start.elapsed());
        tokio::time::sleep(Duration::from_millis(200)).await;
        let attempts = accepted.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(attempts, 3, "one connect and two retries");
    }
}
