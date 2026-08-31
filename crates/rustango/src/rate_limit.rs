//! Token-bucket rate limiting middleware for axum routers.
//!
//! Configurable per-IP or per-user limits with burst allowance. Returns
//! `429 Too Many Requests` when the bucket is exhausted.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::rate_limit::{RateLimitLayer, RateLimitRouterExt};
//! use std::time::Duration;
//!
//! let app = Router::new()
//!     .route("/api/login", post(login))
//!     .rate_limit(RateLimitLayer::per_ip(5, Duration::from_secs(60))); // 5 req/min
//! ```
//!
//! ## Strategy
//!
//! - **Token bucket**: each key (IP or user id) gets `capacity` tokens.
//! - On each request, one token is removed. If empty, return 429.
//! - Tokens refill at `capacity / refill_period` per second.
//! - Buckets live in an in-process `tokio::sync::Mutex<HashMap>`. Process-local —
//!   for distributed enforcement, integrate with the cache layer (future slice).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::{header, HeaderValue, Response, StatusCode};
use axum::middleware::Next;
use axum::Router;

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

/// Strategy for picking the bucket key per request.
#[derive(Clone, Debug)]
pub enum KeyBy {
    /// Use the connecting client's IP address (`ConnectInfo<SocketAddr>`).
    ///
    /// Requires the server to be built with
    /// `into_make_service_with_connect_info::<SocketAddr>()`. If it is
    /// **not**, every request falls back to one shared bucket, so one
    /// client throttles the whole site — a self-inflicted DoS. The
    /// limiter logs a one-time warning when this happens (#1252); make
    /// sure your bind wires up `ConnectInfo`.
    Ip,
    /// Use the value of a request header (e.g. `"x-api-key"` or `"authorization"`).
    ///
    /// In the cache-backed limiter the value is **hashed** before use as
    /// a key, so a shared cache never exposes a raw credential (#1252).
    /// Requests missing the header fall back to one shared bucket and
    /// log a one-time warning, as with `Ip`.
    Header(&'static str),
    /// Single global bucket — coarse but easy. Good for "max N requests/sec for the whole endpoint".
    Global,
}

/// Rate-limit configuration.
#[derive(Clone)]
pub struct RateLimitLayer {
    /// Maximum number of tokens in the bucket. Burst size.
    capacity: u32,
    /// How long the bucket takes to refill from empty to full.
    refill_period: Duration,
    /// Bucket key strategy.
    key_by: KeyBy,
    /// Shared bucket store across all requests.
    store: Arc<tokio::sync::Mutex<HashMap<String, Bucket>>>,
}

#[derive(Clone, Copy, Debug)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

impl RateLimitLayer {
    /// New per-IP rate limit: `capacity` requests every `refill_period`.
    #[must_use]
    pub fn per_ip(capacity: u32, refill_period: Duration) -> Self {
        Self::new(capacity, refill_period, KeyBy::Ip)
    }

    /// New per-header rate limit (e.g. per API key).
    #[must_use]
    pub fn per_header(header: &'static str, capacity: u32, refill_period: Duration) -> Self {
        Self::new(capacity, refill_period, KeyBy::Header(header))
    }

    /// New single-bucket global rate limit.
    #[must_use]
    pub fn global(capacity: u32, refill_period: Duration) -> Self {
        Self::new(capacity, refill_period, KeyBy::Global)
    }

    fn new(capacity: u32, refill_period: Duration, key_by: KeyBy) -> Self {
        Self {
            capacity,
            refill_period,
            key_by,
            store: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    fn rate_per_sec(&self) -> f64 {
        if self.refill_period.is_zero() {
            return f64::MAX;
        }
        self.capacity as f64 / self.refill_period.as_secs_f64()
    }

    /// Take one token. Returns `Ok((remaining, retry_after_secs))` on success,
    /// `Err(retry_after_secs)` when the bucket is empty.
    async fn take(&self, key: &str) -> Result<(u32, u64), u64> {
        let now = Instant::now();
        let cap = self.capacity as f64;
        let rate = self.rate_per_sec();
        let mut store = self.store.lock().await;
        let bucket = store.entry(key.to_owned()).or_insert(Bucket {
            tokens: cap,
            last_refill: now,
        });

        // Refill since last access
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * rate).min(cap);
        bucket.last_refill = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok((bucket.tokens.floor() as u32, 0))
        } else {
            // How many seconds until 1 token is available?
            let need = 1.0 - bucket.tokens;
            let retry = if rate > 0.0 {
                (need / rate).ceil() as u64
            } else {
                u64::MAX
            };
            Err(retry.max(1))
        }
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
            // Unlike the cache-backed limiter, this store is a
            // process-local `HashMap` that never leaves memory, so the
            // header value is not persisted anywhere an attacker could
            // read it — no hashing needed here (#1252).
            KeyBy::Header(name) => req
                .headers()
                .get(*name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    warn_missing_discriminator(name);
                    "<no-header>".to_owned()
                }),
            KeyBy::Global => "<global>".to_owned(),
        }
    }
}

/// Extension trait — apply a rate-limit layer to a router.
pub trait RateLimitRouterExt {
    /// Apply this rate-limit configuration to all routes in this router.
    #[must_use]
    fn rate_limit(self, layer: RateLimitLayer) -> Self;
}

impl<S: Clone + Send + Sync + 'static> RateLimitRouterExt for Router<S> {
    fn rate_limit(self, layer: RateLimitLayer) -> Self {
        let cfg = Arc::new(layer);
        self.layer(axum::middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let cfg = cfg.clone();
                async move { handle(cfg, req, next).await }
            },
        ))
    }
}

async fn handle(cfg: Arc<RateLimitLayer>, req: Request<Body>, next: Next) -> Response<Body> {
    let key = cfg.extract_key(&req);
    match cfg.take(&key).await {
        Ok((remaining, _)) => {
            let mut response = next.run(req).await;
            let _ = response.headers_mut().insert(
                "x-ratelimit-limit",
                HeaderValue::from_str(&cfg.capacity.to_string()).unwrap(),
            );
            let _ = response.headers_mut().insert(
                "x-ratelimit-remaining",
                HeaderValue::from_str(&remaining.to_string()).unwrap(),
            );
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
            .unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_n_requests_succeed_under_capacity() {
        let l = RateLimitLayer::global(3, Duration::from_secs(60));
        for _ in 0..3 {
            assert!(l.take("k").await.is_ok());
        }
    }

    #[tokio::test]
    async fn n_plus_one_request_is_rejected() {
        let l = RateLimitLayer::global(2, Duration::from_secs(60));
        assert!(l.take("k").await.is_ok());
        assert!(l.take("k").await.is_ok());
        let result = l.take("k").await;
        assert!(result.is_err());
        let retry_after = result.unwrap_err();
        assert!(retry_after >= 1);
    }

    #[tokio::test]
    async fn separate_keys_have_independent_buckets() {
        let l = RateLimitLayer::global(1, Duration::from_secs(60));
        assert!(l.take("alice").await.is_ok());
        assert!(l.take("alice").await.is_err());
        // Different key — fresh bucket
        assert!(l.take("bob").await.is_ok());
    }

    #[tokio::test]
    async fn refill_replenishes_tokens_over_time() {
        // 10 tokens / 100ms → 100 tokens/sec
        let l = RateLimitLayer::global(10, Duration::from_millis(100));
        // Drain
        for _ in 0..10 {
            assert!(l.take("k").await.is_ok());
        }
        assert!(l.take("k").await.is_err());
        // Wait 30ms — should refill ~3 tokens
        tokio::time::sleep(Duration::from_millis(35)).await;
        assert!(
            l.take("k").await.is_ok(),
            "should have refilled at least 1 token"
        );
    }
}
