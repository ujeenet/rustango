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
//! - Buckets live in an in-process map, so each process limits on its
//!   own. For one shared limit across processes, use the
//!   `rate_limit_cache` module instead.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::{header, HeaderValue, Response, StatusCode};
use axum::middleware::Next;
use axum::Router;

/// Ceiling on how many buckets the map may hold.
///
/// 100k entries costs a few megabytes. That is more clients than a
/// real deployment sees inside one `refill_period`, but low enough
/// that a flood of forged header values cannot exhaust memory.
const MAX_BUCKETS: usize = 100_000;

/// Warn once per process that the limiter cannot tell clients apart
/// and is using one shared bucket. That turns the limiter into a
/// site-wide throttle. Logged once so a hot path cannot flood logs.
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

/// Warn once that a forwarding header arrived but no
/// [`TrustedRealIp`] did, so every client behind that proxy shares
/// one bucket. Either `RealIpLayer` is missing, it is mounted after
/// the limiter, or no proxies were marked trusted.
///
/// [`TrustedRealIp`]: crate::real_ip::TrustedRealIp
fn warn_forwarded_but_unresolved(req: &Request<Body>) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);

    let forwarded =
        req.headers().contains_key("x-forwarded-for") || req.headers().contains_key("x-real-ip");
    if forwarded && !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            target: "rustango::rate_limit",
            "per-IP rate limiting saw a forwarding header but no trusted client \
             address, so it is keying on the connecting address and EVERY client \
             behind that proxy shares ONE bucket. Mount `real_ip::RealIpLayer` \
             ahead of the limiter (layers apply outermost-last, so add it after) \
             AND name your proxies with `.trust_proxies([...])` — without that \
             the header is only a claim and is deliberately ignored here.",
        );
    }
}

/// The client IP the limiters key on: the forwarded address if it
/// came through a **trusted** proxy, otherwise the connecting socket.
///
/// This uses [`TrustedRealIp`], never [`RealIp`]. A `RealIp` is only a
/// claim by whoever sent the header. Keying on it would let any client
/// pick its own bucket and skip the limit entirely. Only
/// [`RealIpLayer::trust_proxies`] produces the trusted form, so an
/// operator must name the proxy hops; nothing is guessed.
///
/// All header parsing lives in `RealIpLayer`. The limiters never read
/// `X-Forwarded-For` themselves.
///
/// [`TrustedRealIp`]: crate::real_ip::TrustedRealIp
/// [`RealIp`]: crate::real_ip::RealIp
/// [`RealIpLayer::trust_proxies`]: crate::real_ip::RealIpLayer::trust_proxies
pub(crate) fn client_ip_key(req: &Request<Body>) -> String {
    if let Some(ip) = req.extensions().get::<crate::real_ip::TrustedRealIp>() {
        return ip.0.to_string();
    }
    match req.extensions().get::<ConnectInfo<SocketAddr>>() {
        Some(ci) => {
            warn_forwarded_but_unresolved(req);
            ci.ip().to_string()
        }
        None => {
            warn_missing_discriminator("IP (ConnectInfo missing)");
            "<no-ip>".to_owned()
        }
    }
}

/// Strategy for picking the bucket key per request.
#[derive(Clone, Debug)]
pub enum KeyBy {
    /// Use the connecting client's IP address (`ConnectInfo<SocketAddr>`).
    ///
    /// Serve with
    /// `into_make_service_with_connect_info::<SocketAddr>()`. Without
    /// it, every request shares one bucket, so a single client can
    /// throttle the whole site. The limiter warns once if that happens.
    Ip,
    /// Use the value of a request header, such as `"x-api-key"`.
    ///
    /// The cache-backed limiter hashes the value first, so a shared
    /// cache never holds a raw credential. Requests without the header
    /// share one bucket and log a warning, as with `Ip`.
    Header(&'static str),
    /// One bucket for everything. Coarse, but fine for "max N
    /// requests/sec on this endpoint".
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
    /// Ceiling on distinct buckets — see [`MAX_BUCKETS`] and
    /// [`RateLimitLayer::max_buckets`].
    max_buckets: usize,
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
            max_buckets: MAX_BUCKETS,
            store: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Override the distinct-bucket ceiling (default [`MAX_BUCKETS`]).
    ///
    /// Raise it if you really serve more than 100k distinct clients
    /// inside one `refill_period` and have the memory. Lower it on a
    /// small instance. `0` becomes 1: a limiter with no buckets cannot
    /// limit. Tests also use a small ceiling to reach the eviction path.
    #[must_use]
    pub fn max_buckets(mut self, n: usize) -> Self {
        self.max_buckets = n.max(1);
        self
    }

    fn rate_per_sec(&self) -> f64 {
        if self.refill_period.is_zero() {
            return f64::MAX;
        }
        self.capacity as f64 / self.refill_period.as_secs_f64()
    }

    /// Bound the bucket map before inserting a new key. With
    /// `KeyBy::Header` the key is an attacker-chosen header value, so
    /// an unbounded map is a way to kill the process.
    ///
    /// **The sweep drops only buckets that are full.** A full bucket
    /// and a missing bucket behave the same: a new key starts at
    /// `tokens = capacity`, and a bucket left alone for
    /// `refill_period` has refilled to exactly that. So dropping it
    /// cannot give anyone one extra request. Dropping a partly spent
    /// bucket would, and that is a rate-limit bypass.
    ///
    /// If every bucket is still partly spent, the hard cap kicks in
    /// and drops the **fullest** ones, which are the cheapest to lose.
    ///
    /// **Both passes rank on projected tokens, never on age.** Ranking
    /// by age is backwards: `last_refill` moves on every take, so the
    /// least recently used bucket is the one that spent its budget and
    /// went quiet. That is exactly the bucket an attacker wants
    /// evicted, because evicting it returns a full allowance.
    fn make_room(&self, store: &mut HashMap<String, Bucket>, now: Instant) {
        if store.len() < self.max_buckets {
            return;
        }
        let cap = self.capacity as f64;
        let rate = self.rate_per_sec();
        // What this bucket would hold if it were touched right now.
        let projected = |b: &Bucket| {
            let elapsed = now.duration_since(b.last_refill).as_secs_f64();
            (b.tokens + elapsed * rate).min(cap)
        };

        // A bucket at capacity allows the same as a missing one.
        store.retain(|_, b| projected(b) < cap);
        if store.len() < self.max_buckets {
            return;
        }

        // Still at the cap. Drop the fullest eighth, so this does not
        // run again on the very next miss.
        let mut by_fullness: Vec<(f64, String)> = store
            .iter()
            .map(|(k, b)| (projected(b), k.clone()))
            .collect();
        by_fullness.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
        for (_, k) in by_fullness.into_iter().take((self.max_buckets / 8).max(1)) {
            store.remove(&k);
        }
    }

    /// Take one token. Returns `Ok((remaining, retry_after_secs))` on success,
    /// `Err(retry_after_secs)` when the bucket is empty.
    async fn take(&self, key: &str) -> Result<(u32, u64), u64> {
        let now = Instant::now();
        let cap = self.capacity as f64;
        let rate = self.rate_per_sec();
        let mut store = self.store.lock().await;
        // Only a new key grows the map, so sweep on the miss path and
        // keep the hot path a plain lookup.
        if !store.contains_key(key) {
            self.make_room(&mut store, now);
        }
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
            KeyBy::Ip => client_ip_key(req),
            // This map stays in process memory and is never written
            // anywhere, so the raw header value needs no hashing. The
            // cache-backed limiter does hash it.
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
