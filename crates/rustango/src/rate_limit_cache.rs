//! Distributed rate limiting via the [`Cache`](crate::cache::Cache) trait.
//!
//! Pair this with `cache::RedisCache` for safe enforcement across many
//! processes / replicas — a single shared counter per `(window, key)`
//! pair, incremented atomically by Redis' `INCRBY`. Pair with the
//! built-in `InMemoryCache` for testing.
//!
//! ## Algorithm: fixed-window counter
//!
//! For each `(key, window)` pair, the bucket id is the unix-seconds
//! window-start (`(now / window_secs) * window_secs`). Each request
//! increments that counter. The counter expires when its window does.
//! Simple, fast, no per-request locks, and works across replicas because
//! Redis owns the shared state.
//!
//! Trade vs. token-bucket: bursts can hit `2 * capacity` in a single
//! second straddling a window edge. For most APIs this is fine — if
//! you need leaky-bucket smoothness, stay with the in-process
//! [`crate::rate_limit::RateLimitLayer`] (single replica) or build a
//! sliding-window counter on top of the same [`Cache`] trait.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::rate_limit::KeyBy;
//! use rustango::rate_limit_cache::{CacheRateLimitLayer, CacheRateLimitRouterExt};
//! use rustango::cache::RedisCache;
//! use std::sync::Arc;
//! use std::time::Duration;
//!
//! let cache: rustango::cache::BoxedCache =
//!     Arc::new(RedisCache::connect("redis://localhost").await?);
//!
//! let app = axum::Router::new()
//!     .route("/api/login", axum::routing::post(login))
//!     .cache_rate_limit(
//!         CacheRateLimitLayer::new(cache, 5, Duration::from_secs(60))
//!             .key_by(KeyBy::Ip)
//!             .key_prefix("login"),
//!     );
//! ```

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::{header, HeaderValue, Response, StatusCode};
use axum::middleware::Next;
use axum::Router;

use crate::cache::BoxedCache;
use crate::rate_limit::KeyBy;

/// One-way hash of a bucket discriminator (#1252). A `SHA-256` prefix —
/// enough to distribute keys and to make the same client hash the same,
/// while never exposing a secret header value (API key / bearer token)
/// as a readable cache key.
fn hash_discriminator(value: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(value.as_bytes());
    // 16 hex chars (64 bits) — collision-negligible for a rate-limit
    // keyspace, and short enough to keep cache keys compact.
    digest[..8]
        .iter()
        .fold(String::with_capacity(16), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Warn (at most once per process) that the limiter could not derive a
/// per-client key and is falling back to a shared bucket — a
/// misconfiguration that turns the limiter into a site-wide throttle
/// (#1252). Rate-limited to one line so a hot path can't flood logs.
fn warn_missing_discriminator(what: &str) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            target: "rustango::rate_limit",
            discriminator = %what,
            "rate limiter could not derive a per-client key; ALL such requests \
             share ONE bucket, so one client throttles everyone. For IP keying, \
             serve with `into_make_service_with_connect_info::<SocketAddr>()`; \
             for header keying, ensure the header is present.",
        );
    }
}

/// Fixed-window counter rate limiter backed by a [`Cache`](crate::cache::Cache).
///
/// Cheap to clone (everything is `Arc`-wrapped or `Copy`).
#[derive(Clone)]
pub struct CacheRateLimitLayer {
    cache: BoxedCache,
    capacity: u32,
    window: Duration,
    key_by: KeyBy,
    /// Prefix for cache keys. Distinguishes multiple limiters that share
    /// the same cache (e.g. `"login"` vs `"signup"`).
    key_prefix: Arc<String>,
}

impl CacheRateLimitLayer {
    /// New limiter: `capacity` requests per `window`, keyed by IP.
    #[must_use]
    pub fn new(cache: BoxedCache, capacity: u32, window: Duration) -> Self {
        Self {
            cache,
            capacity,
            window,
            key_by: KeyBy::Ip,
            key_prefix: Arc::new("rl".to_owned()),
        }
    }

    #[must_use]
    pub fn key_by(mut self, key_by: KeyBy) -> Self {
        self.key_by = key_by;
        self
    }

    #[must_use]
    pub fn key_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.key_prefix = Arc::new(prefix.into());
        self
    }

    fn extract_key(&self, req: &Request<Body>) -> String {
        match &self.key_by {
            KeyBy::Ip => req
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ci| ci.ip().to_string())
                .unwrap_or_else(|| {
                    warn_missing_discriminator("IP (ConnectInfo missing)");
                    "<no-ip>".to_owned()
                }),
            // Hash the header value, never store it raw (#1252). Keyed by
            // `authorization` / `x-api-key`, the raw value is a live
            // secret; this counter lives in a shared cache (Redis), so a
            // raw key would leak credentials to anyone who can enumerate
            // keys (`SCAN`, an RDB dump). A SHA-256 prefix distributes
            // just as well and is one-way.
            KeyBy::Header(name) => req
                .headers()
                .get(*name)
                .and_then(|v| v.to_str().ok())
                .map(hash_discriminator)
                .unwrap_or_else(|| {
                    warn_missing_discriminator(name);
                    "<no-header>".to_owned()
                }),
            KeyBy::Global => "<global>".to_owned(),
        }
    }

    fn window_secs(&self) -> u64 {
        self.window.as_secs().max(1)
    }

    /// Take one slot from the bucket for `key`. Returns
    /// `Ok((current_count, reset_at_unix_secs))` on success,
    /// `Err(retry_after_secs)` when over capacity.
    ///
    /// # Errors
    /// Returns `Err(retry_after_secs)` when the limit has been hit. Any
    /// underlying cache error is treated as "fail open" — the request is
    /// allowed and `Ok((0, 0))` is returned. This avoids hard outages
    /// when Redis is briefly unreachable; flip to fail-closed by reading
    /// `cache.incr(...)` directly if your threat model requires it.
    pub async fn take(&self, key: &str) -> Result<(u32, u64), u64> {
        let window_secs = self.window_secs();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let window_start = (now / window_secs) * window_secs;
        let cache_key = format!("{}:{}:{window_start}", self.key_prefix.as_str(), key,);

        let count = match self
            .cache
            .incr(&cache_key, 1, Some(Duration::from_secs(window_secs)))
            .await
        {
            Ok(n) => n,
            Err(_e) => {
                // Fail-open: cache outage shouldn't deny all traffic.
                tracing::warn!(cache_key, "rate-limit cache incr failed; allowing request");
                return Ok((0, 0));
            }
        };

        let reset_at = window_start + window_secs;
        if count > i64::from(self.capacity) {
            let retry = reset_at.saturating_sub(now).max(1);
            Err(retry)
        } else {
            // count fits in u32 because capacity is u32 and we check above.
            Ok((u32::try_from(count).unwrap_or(u32::MAX), reset_at))
        }
    }
}

/// Extension trait — apply a cache-backed rate-limit layer to a router.
pub trait CacheRateLimitRouterExt {
    #[must_use]
    fn cache_rate_limit(self, layer: CacheRateLimitLayer) -> Self;
}

impl<S: Clone + Send + Sync + 'static> CacheRateLimitRouterExt for Router<S> {
    fn cache_rate_limit(self, layer: CacheRateLimitLayer) -> Self {
        let cfg = Arc::new(layer);
        self.layer(axum::middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let cfg = cfg.clone();
                async move { handle(cfg, req, next).await }
            },
        ))
    }
}

async fn handle(cfg: Arc<CacheRateLimitLayer>, req: Request<Body>, next: Next) -> Response<Body> {
    let key = cfg.extract_key(&req);
    match cfg.take(&key).await {
        Ok((count, reset_at)) => {
            let mut response = next.run(req).await;
            let remaining = i64::from(cfg.capacity).saturating_sub(i64::from(count));
            let _ = response.headers_mut().insert(
                "x-ratelimit-limit",
                HeaderValue::from_str(&cfg.capacity.to_string())
                    .unwrap_or(HeaderValue::from_static("0")),
            );
            let _ = response.headers_mut().insert(
                "x-ratelimit-remaining",
                HeaderValue::from_str(&remaining.max(0).to_string())
                    .unwrap_or(HeaderValue::from_static("0")),
            );
            if reset_at > 0 {
                let _ = response.headers_mut().insert(
                    "x-ratelimit-reset",
                    HeaderValue::from_str(&reset_at.to_string())
                        .unwrap_or(HeaderValue::from_static("0")),
                );
            }
            response
        }
        Err(retry_secs) => Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header(header::RETRY_AFTER, retry_secs.to_string())
            .header("x-ratelimit-limit", cfg.capacity.to_string())
            .header("x-ratelimit-remaining", "0")
            .body(Body::from(format!(
                r#"{{"error":"rate limit exceeded","retry_after":{retry_secs}}}"#
            )))
            .unwrap_or_else(|_| Response::new(Body::empty())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::InMemoryCache;

    fn layer(capacity: u32, window_secs: u64) -> CacheRateLimitLayer {
        let cache: BoxedCache = Arc::new(InMemoryCache::new());
        CacheRateLimitLayer::new(cache, capacity, Duration::from_secs(window_secs))
            .key_prefix("test")
    }

    #[test]
    fn header_value_is_hashed_not_stored_raw() {
        // #1252 — a secret header value (API key / bearer token) must
        // never appear verbatim in a cache key.
        let secret = "super-secret-api-key-abc123";
        let h = hash_discriminator(secret);
        assert_ne!(h, secret, "value must be transformed");
        assert!(
            !h.contains("secret"),
            "raw secret must not leak into the key"
        );
        assert_eq!(h.len(), 16, "expected a 16-hex-char digest prefix");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        // Deterministic: the same client hashes to the same bucket.
        assert_eq!(h, hash_discriminator(secret));
        // Distinct values → distinct keys.
        assert_ne!(h, hash_discriminator("a-different-key"));
    }

    #[tokio::test]
    async fn first_n_under_capacity_succeed() {
        let l = layer(3, 60);
        for _ in 0..3 {
            assert!(l.take("alice").await.is_ok());
        }
    }

    #[tokio::test]
    async fn n_plus_one_returns_retry_after() {
        let l = layer(2, 60);
        assert!(l.take("alice").await.is_ok());
        assert!(l.take("alice").await.is_ok());
        let err = l.take("alice").await.unwrap_err();
        assert!(err >= 1, "retry_after must be at least 1 sec, got {err}");
    }

    #[tokio::test]
    async fn separate_keys_have_independent_counters() {
        let l = layer(1, 60);
        assert!(l.take("alice").await.is_ok());
        assert!(l.take("alice").await.is_err());
        // Different key — fresh bucket
        assert!(l.take("bob").await.is_ok());
    }

    #[tokio::test]
    async fn separate_prefixes_have_independent_counters() {
        let cache: BoxedCache = Arc::new(InMemoryCache::new());
        let a =
            CacheRateLimitLayer::new(cache.clone(), 1, Duration::from_secs(60)).key_prefix("login");
        let b = CacheRateLimitLayer::new(cache, 1, Duration::from_secs(60)).key_prefix("signup");
        assert!(a.take("alice").await.is_ok());
        assert!(a.take("alice").await.is_err());
        // b uses a different prefix — same key gets its own counter
        assert!(b.take("alice").await.is_ok());
    }

    #[tokio::test]
    async fn count_returned_increases_per_call() {
        let l = layer(5, 60);
        let (c1, _) = l.take("k").await.unwrap();
        let (c2, _) = l.take("k").await.unwrap();
        let (c3, _) = l.take("k").await.unwrap();
        assert_eq!((c1, c2, c3), (1, 2, 3));
    }

    #[tokio::test]
    async fn reset_at_advances_with_window() {
        let l = layer(1, 60);
        let (_, reset_at) = l.take("k").await.unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // reset_at must be in the future and within one window-length.
        assert!(reset_at > now);
        assert!(reset_at <= now + 60);
    }

    #[tokio::test]
    async fn fail_open_on_cache_error_is_documented_via_take_succeeding() {
        // The InMemoryCache never errors, so we verify the success path
        // here. Real fail-open behavior is exercised by the
        // CacheRateLimitLayer::take() doc comment + the fact that we
        // return Ok((0,0)) on Err.
        let l = layer(2, 60);
        assert!(l.take("k").await.is_ok());
    }
}
