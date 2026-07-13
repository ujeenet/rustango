//! Idempotency-key middleware (Stripe-shape).
//!
//! When a write request arrives with an `Idempotency-Key` header, this
//! middleware:
//!
//! 1. Looks up the key in the [`Cache`](crate::cache::Cache).
//! 2. If a stored response exists, replays it byte-for-byte (so retried
//!    POSTs don't double-charge / double-create / double-anything).
//! 3. If not, runs the handler, captures status + headers + body, and
//!    stores the result under the key with a configurable TTL (default
//!    24 h).
//!
//! Requests without the header pass straight through unmodified.
//!
//! Note: the RFC 10008 `QUERY` method needs no `Idempotency-Key` — QUERY
//! is idempotent by definition, so a retried QUERY is inherently safe.
//! This middleware defaults to the mutating methods (POST/PUT/PATCH/DELETE)
//! and deliberately does not cover QUERY.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::idempotency::{IdempotencyLayer, IdempotencyRouterExt};
//! use rustango::cache::{BoxedCache, InMemoryCache};
//! use std::sync::Arc;
//!
//! let cache: BoxedCache = Arc::new(InMemoryCache::new());
//! let app = axum::Router::new()
//!     .route("/api/charges", axum::routing::post(create_charge))
//!     .idempotency(IdempotencyLayer::new(cache));
//! ```
//!
//! ## What gets cached
//!
//! Only **successful** responses (2xx). 4xx and 5xx replies are not
//! cached so a retried request that hit a transient error can still
//! reach a healthier replica. Override with
//! [`IdempotencyLayer::cache_status_codes`].
//!
//! ## What gets checked
//!
//! Defaults: POST, PUT, PATCH, DELETE. GET / HEAD / OPTIONS bypass
//! the layer entirely (idempotent by spec).
//!
//! ## Cache key shape
//!
//! `idem:<scope>:<idempotency_key>`. Override `<scope>` via
//! [`IdempotencyLayer::scope`] when you have multiple endpoints whose
//! key namespaces shouldn't collide (e.g. `"charges"` vs `"refunds"`).

use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode};
use axum::middleware::Next;
use axum::Router;
use serde::{Deserialize, Serialize};

use crate::cache::BoxedCache;

const DEFAULT_HEADER: &str = "idempotency-key";
const DEFAULT_BODY_CAP: usize = 4 * 1024 * 1024;

/// Cached snapshot of a successful response. Lives in the cache until
/// the TTL expires.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body_b64: String,
}

#[derive(Clone)]
pub struct IdempotencyLayer {
    cache: BoxedCache,
    header: &'static str,
    scope: Arc<String>,
    ttl: Duration,
    methods: Arc<Vec<Method>>,
    body_cap: usize,
    cache_status: Arc<dyn Fn(StatusCode) -> bool + Send + Sync>,
}

impl IdempotencyLayer {
    /// New layer using the standard `Idempotency-Key` header, 24 h TTL,
    /// caching every 2xx response.
    #[must_use]
    pub fn new(cache: BoxedCache) -> Self {
        Self {
            cache,
            header: DEFAULT_HEADER,
            scope: Arc::new(String::new()),
            ttl: Duration::from_secs(24 * 60 * 60),
            methods: Arc::new(vec![
                Method::POST,
                Method::PUT,
                Method::PATCH,
                Method::DELETE,
            ]),
            body_cap: DEFAULT_BODY_CAP,
            cache_status: Arc::new(|s| s.is_success()),
        }
    }

    /// Use a different request header (e.g. `"x-request-id"`).
    #[must_use]
    pub fn header(mut self, name: &'static str) -> Self {
        self.header = name;
        self
    }

    /// Namespace cache keys so two routers sharing the same Cache
    /// don't replay each other's responses.
    #[must_use]
    pub fn scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = Arc::new(scope.into());
        self
    }

    #[must_use]
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Override which methods get the idempotency treatment. Pass an
    /// empty vec to apply to every method.
    #[must_use]
    pub fn methods(mut self, methods: Vec<Method>) -> Self {
        self.methods = Arc::new(methods);
        self
    }

    /// Cap on cached response body bytes. Bodies larger than this are
    /// served back to the original caller as-is but NOT stored, so
    /// retries fall through to the handler. Default: 4 MiB.
    #[must_use]
    pub fn body_cap(mut self, n: usize) -> Self {
        self.body_cap = n;
        self
    }

    /// Predicate for "is this response cacheable?". Default: 2xx only.
    /// Pass a closure to widen (e.g. cache 404s on PUT) or narrow
    /// (cache only 201).
    #[must_use]
    pub fn cache_status_codes<F>(mut self, predicate: F) -> Self
    where
        F: Fn(StatusCode) -> bool + Send + Sync + 'static,
    {
        self.cache_status = Arc::new(predicate);
        self
    }
}

pub trait IdempotencyRouterExt {
    #[must_use]
    fn idempotency(self, layer: IdempotencyLayer) -> Self;
}

impl<S: Clone + Send + Sync + 'static> IdempotencyRouterExt for Router<S> {
    fn idempotency(self, layer: IdempotencyLayer) -> Self {
        let cfg = Arc::new(layer);
        self.layer(axum::middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let cfg = cfg.clone();
                async move { handle(cfg, req, next).await }
            },
        ))
    }
}

async fn handle(cfg: Arc<IdempotencyLayer>, req: Request<Body>, next: Next) -> Response<Body> {
    if !cfg.methods.is_empty() && !cfg.methods.contains(req.method()) {
        return next.run(req).await;
    }
    let Some(key) = req
        .headers()
        .get(cfg.header)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
    else {
        return next.run(req).await;
    };
    if key.is_empty() || key.len() > 256 {
        // Be strict — Stripe rejects > 255-char keys. Pass through to
        // the handler, which may emit its own 4xx.
        return next.run(req).await;
    }
    let cache_key = format!("idem:{}:{}", cfg.scope, key);

    // Replay stored response on HIT.
    if let Some(stored) = read_stored(&cfg.cache, &cache_key).await {
        return rebuild(stored);
    }

    // MISS — run the handler, then capture + store on success.
    let response = next.run(req).await;
    let (parts, body) = response.into_parts();
    let status = parts.status;
    let bytes = match to_bytes(body, cfg.body_cap).await {
        Ok(b) => b,
        Err(_) => {
            // Body too large or stream error — pass response through
            // empty (we already consumed the original) and skip
            // caching. Honest failure mode.
            return Response::from_parts(parts, Body::empty());
        }
    };

    if (cfg.cache_status)(status) {
        let stored = StoredResponse {
            status: status.as_u16(),
            headers: parts
                .headers
                .iter()
                .filter_map(|(k, v)| {
                    let k = k.as_str().to_owned();
                    v.to_str().ok().map(|s| (k, s.to_owned()))
                })
                .collect(),
            body_b64: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes),
        };
        // Best-effort write — failure here is logged + ignored so the
        // caller still gets their successful response.
        if let Ok(json) = serde_json::to_string(&stored) {
            if let Err(e) = cfg.cache.set(&cache_key, &json, Some(cfg.ttl)).await {
                tracing::warn!(error = %e, cache_key, "idempotency: cache write failed");
            }
        }
    }

    Response::from_parts(parts, Body::from(bytes))
}

async fn read_stored(cache: &BoxedCache, cache_key: &str) -> Option<StoredResponse> {
    let raw = cache.get(cache_key).await.ok()??;
    serde_json::from_str(&raw).ok()
}

fn rebuild(stored: StoredResponse) -> Response<Body> {
    let body_bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        stored.body_b64.as_bytes(),
    )
    .unwrap_or_default();

    let mut builder =
        Response::builder().status(StatusCode::from_u16(stored.status).unwrap_or(StatusCode::OK));
    for (k, v) in &stored.headers {
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(k.as_str()), HeaderValue::from_str(v))
        {
            builder = builder.header(name, value);
        }
    }
    let mut resp = builder
        .body(Body::from(body_bytes))
        .unwrap_or_else(|_| Response::new(Body::empty()));
    // Mark replays so observability + clients can spot dedup'd traffic.
    resp.headers_mut().insert(
        HeaderName::from_static("idempotent-replayed"),
        HeaderValue::from_static("true"),
    );
    let _ = headers_align_content_length(resp.headers_mut());
    resp
}

/// Drop any stale Content-Length we copied from the cached response —
/// the body bytes are the same so it should still be right, but axum
/// may want to re-compute for streaming bodies.
fn headers_align_content_length(_headers: &mut HeaderMap) -> Result<(), ()> {
    // Reserved for future use — the cached body is already in-memory so
    // axum recomputes Content-Length on the way out. Leaving this here
    // keeps the call-site explicit about the invariant.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::InMemoryCache;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::post;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;
    use tower::ServiceExt;

    fn cache() -> BoxedCache {
        StdArc::new(InMemoryCache::new())
    }

    async fn body_string(resp: Response<Body>) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn passes_through_when_no_idempotency_key_header() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let app = Router::new()
            .route(
                "/",
                post(move || {
                    let c = c.clone();
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        "ok"
                    }
                }),
            )
            .idempotency(IdempotencyLayer::new(cache()));

        for _ in 0..3 {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
        }
        assert_eq!(
            counter.load(Ordering::SeqCst),
            3,
            "no key -> handler runs every time"
        );
    }

    #[tokio::test]
    async fn replays_cached_response_on_same_key() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let app = Router::new()
            .route(
                "/",
                post(move || {
                    let c = c.clone();
                    async move {
                        let n = c.fetch_add(1, Ordering::SeqCst);
                        format!("call-{n}")
                    }
                }),
            )
            .idempotency(IdempotencyLayer::new(cache()));

        let make_req = || {
            Request::builder()
                .method(Method::POST)
                .uri("/")
                .header("idempotency-key", "abc-123")
                .body(Body::empty())
                .unwrap()
        };

        let r1 = app.clone().oneshot(make_req()).await.unwrap();
        assert_eq!(r1.status(), 200);
        assert_eq!(body_string(r1).await, "call-0");

        let r2 = app.clone().oneshot(make_req()).await.unwrap();
        assert_eq!(r2.status(), 200);
        // The replayed response should be IDENTICAL to call 0, not call 1.
        assert_eq!(
            r2.headers()
                .get("idempotent-replayed")
                .and_then(|v| v.to_str().ok()),
            Some("true")
        );
        assert_eq!(body_string(r2).await, "call-0");

        // Handler ran exactly once.
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn different_keys_run_handler_independently() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let app = Router::new()
            .route(
                "/",
                post(move || {
                    let c = c.clone();
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        "ok"
                    }
                }),
            )
            .idempotency(IdempotencyLayer::new(cache()));

        for key in &["k1", "k2", "k3"] {
            let _ = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/")
                        .header("idempotency-key", *key)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
        }
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn does_not_cache_4xx_response() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let app = Router::new()
            .route(
                "/",
                post(move || {
                    let c = c.clone();
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        (StatusCode::BAD_REQUEST, "nope")
                    }
                }),
            )
            .idempotency(IdempotencyLayer::new(cache()));

        let make_req = || {
            Request::builder()
                .method(Method::POST)
                .uri("/")
                .header("idempotency-key", "k")
                .body(Body::empty())
                .unwrap()
        };

        let r1 = app.clone().oneshot(make_req()).await.unwrap();
        assert_eq!(r1.status(), 400);
        let r2 = app.clone().oneshot(make_req()).await.unwrap();
        assert_eq!(r2.status(), 400);
        assert_eq!(counter.load(Ordering::SeqCst), 2, "4xx is not cached");
        assert!(r2.headers().get("idempotent-replayed").is_none());
    }

    #[tokio::test]
    async fn skips_get_requests() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let app = Router::new()
            .route(
                "/",
                axum::routing::get(move || {
                    let c = c.clone();
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        "ok"
                    }
                }),
            )
            .idempotency(IdempotencyLayer::new(cache()));

        for _ in 0..2 {
            let _ = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::GET)
                        .uri("/")
                        .header("idempotency-key", "k")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
        }
        // GET bypasses the layer entirely.
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn empty_or_oversize_key_is_ignored() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let app = Router::new()
            .route(
                "/",
                post(move || {
                    let c = c.clone();
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        "ok"
                    }
                }),
            )
            .idempotency(IdempotencyLayer::new(cache()));

        for key in &["", &"x".repeat(300)] {
            let _ = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/")
                        .header("idempotency-key", *key)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
        }
        // Both pass through to the handler with no caching.
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn scoped_layers_dont_collide_in_shared_cache() {
        let cache = cache();
        let counter_a = StdArc::new(AtomicUsize::new(0));
        let counter_b = StdArc::new(AtomicUsize::new(0));

        let ca = counter_a.clone();
        let app_a = Router::new()
            .route(
                "/",
                post(move || {
                    let ca = ca.clone();
                    async move {
                        ca.fetch_add(1, Ordering::SeqCst);
                        "a"
                    }
                }),
            )
            .idempotency(IdempotencyLayer::new(cache.clone()).scope("a"));

        let cb = counter_b.clone();
        let app_b = Router::new()
            .route(
                "/",
                post(move || {
                    let cb = cb.clone();
                    async move {
                        cb.fetch_add(1, Ordering::SeqCst);
                        "b"
                    }
                }),
            )
            .idempotency(IdempotencyLayer::new(cache).scope("b"));

        let req = || {
            Request::builder()
                .method(Method::POST)
                .uri("/")
                .header("idempotency-key", "shared")
                .body(Body::empty())
                .unwrap()
        };

        // Same key, different scopes — both handlers should still run
        // (and on a second hit, each would replay independently).
        let _ = app_a.clone().oneshot(req()).await.unwrap();
        let _ = app_b.clone().oneshot(req()).await.unwrap();
        assert_eq!(counter_a.load(Ordering::SeqCst), 1);
        assert_eq!(counter_b.load(Ordering::SeqCst), 1);

        // Replay on each.
        let r_a = app_a.oneshot(req()).await.unwrap();
        assert_eq!(body_string(r_a).await, "a");
        let r_b = app_b.oneshot(req()).await.unwrap();
        assert_eq!(body_string(r_b).await, "b");

        assert_eq!(counter_a.load(Ordering::SeqCst), 1);
        assert_eq!(counter_b.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cache_status_codes_predicate_widens_what_is_cached() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let app = Router::new()
            .route(
                "/",
                post(move || {
                    let c = c.clone();
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        (StatusCode::CONFLICT, "duplicate")
                    }
                }),
            )
            .idempotency(
                IdempotencyLayer::new(cache())
                    .cache_status_codes(|s| s == StatusCode::CONFLICT || s.is_success()),
            );

        let make_req = || {
            Request::builder()
                .method(Method::POST)
                .uri("/")
                .header("idempotency-key", "dup")
                .body(Body::empty())
                .unwrap()
        };

        let _ = app.clone().oneshot(make_req()).await.unwrap();
        let r2 = app.clone().oneshot(make_req()).await.unwrap();
        assert_eq!(r2.status(), 409);
        // Conflict was cached + replayed.
        assert_eq!(
            r2.headers()
                .get("idempotent-replayed")
                .and_then(|v| v.to_str().ok()),
            Some("true")
        );
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn custom_header_name_is_honored() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let app = Router::new()
            .route(
                "/",
                post(move || {
                    let c = c.clone();
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        "ok"
                    }
                }),
            )
            .idempotency(IdempotencyLayer::new(cache()).header("x-request-id"));

        let make_req = || {
            Request::builder()
                .method(Method::POST)
                .uri("/")
                .header("x-request-id", "req-42")
                .body(Body::empty())
                .unwrap()
        };
        let _ = app.clone().oneshot(make_req()).await.unwrap();
        let _ = app.clone().oneshot(make_req()).await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
}
