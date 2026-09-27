//! Idempotency-key middleware, in the shape Stripe uses.
//!
//! When a write request carries an `Idempotency-Key` header, the
//! middleware looks the key up in the [`Cache`](crate::cache::Cache).
//! On a hit it replays the stored response byte for byte, so a retried
//! POST does not charge or create anything twice. On a miss it runs the
//! handler, then stores the status, headers and body under the key for
//! the TTL, 24 hours by default.
//!
//! A request without the header passes through untouched.
//!
//! The RFC 10008 `QUERY` method is safe to repeat on its own, so this
//! layer does not cover it.
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
//! Only 2xx responses. A 4xx or 5xx is not stored, so a retry after a
//! short-lived error can still reach a healthy replica. Change this
//! with [`IdempotencyLayer::cache_status_codes`].
//!
//! A response that sets a cookie is never stored: it carries per-client
//! state, and a replay must be the original response or nothing.
//!
//! ## What gets checked
//!
//! POST, PUT, PATCH and DELETE by default. GET, HEAD and OPTIONS skip
//! the layer.
//!
//! ## Cache keys
//!
//! `idem:<sha256>` over the [`IdempotencyLayer::scope`], host, the tenant
//! the tenancy layer resolves for the request, the caller, method, path
//! and query, and the client's key. The same key from another caller, tenant
//! or route is a different entry.
//!
//! The caller is the resolved principal when there is one, else the
//! `Authorization`, `Cookie` and `X-Api-Key` headers. Add the auth layer
//! after `.idempotency(..)`, so it runs first and a token refresh between
//! retries keeps the same key.
//!
//! A reused key with a different request body gets `422`. A body over
//! [`IdempotencyLayer::body_cap`] gets `413`.
//!
//! [`IdempotencyLayer::scope`]: crate::idempotency::IdempotencyLayer::scope
//! [`IdempotencyLayer::cache_status_codes`]: crate::idempotency::IdempotencyLayer::cache_status_codes
//! [`IdempotencyLayer::body_cap`]: crate::idempotency::IdempotencyLayer::body_cap

use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::{OriginalUri, Request};
use axum::http::header::{AUTHORIZATION, CONTENT_LENGTH, COOKIE, HOST, SET_COOKIE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode};
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::Router;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::api_errors::ApiError;
use crate::cache::BoxedCache;

const DEFAULT_HEADER: &str = "idempotency-key";
const DEFAULT_BODY_CAP: usize = 4 * 1024 * 1024;

/// A stored response, kept until its TTL runs out.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredResponse {
    /// SHA-256 of the request body that produced this response.
    request_sha256: String,
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
    /// New layer: the `Idempotency-Key` header, a 24 hour TTL, and
    /// every 2xx response cached.
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

    /// Read the key from another header, such as `"x-request-id"`.
    #[must_use]
    pub fn header(mut self, name: &'static str) -> Self {
        self.header = name;
        self
    }

    /// Add a name to the cache keys, so two routers on one cache
    /// cannot replay each other's responses.
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

    /// Choose which methods the layer handles. An empty vec means all
    /// of them.
    #[must_use]
    pub fn methods(mut self, methods: Vec<Method>) -> Self {
        self.methods = Arc::new(methods);
        self
    }

    /// Largest request or response body the layer handles, 4 MiB by
    /// default. A keyed request over it gets `413`; a response over it is
    /// not stored.
    #[must_use]
    pub fn body_cap(mut self, n: usize) -> Self {
        self.body_cap = n;
        self
    }

    /// Decide which responses to store. The default stores 2xx only;
    /// a closure can widen or narrow that.
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
        // Stripe rejects keys over 255 chars. Pass the request to the
        // handler, which may answer with its own 4xx.
        return next.run(req).await;
    }
    let (parts, body) = req.into_parts();
    let declared_len = parts
        .headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok());
    // 413 whether declared or streamed: a streamed body is already
    // consumed, and a silent pass-through would drop the replay guarantee.
    if declared_len.is_some_and(|n| n > cfg.body_cap) {
        return too_large();
    }
    let req_bytes = match to_bytes(body, cfg.body_cap).await {
        Ok(b) => b,
        Err(e) if over_cap(&e) => return too_large(),
        Err(_) => {
            return ApiError::new(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "request body could not be read",
            )
            .into_response()
        }
    };
    let Ok(tenant) = request_tenant(&parts).await else {
        // Unkeyable without a tenant; the handler reports the error.
        return next
            .run(Request::from_parts(parts, Body::from(req_bytes)))
            .await;
    };
    let cache_key = cache_key(&cfg.scope, &parts, tenant.as_deref(), &key);
    let request_sha256 = hex(&Sha256::digest(&req_bytes));

    // Hit: replay the stored response, if it answered this same body.
    if let Some(stored) = read_stored(&cfg.cache, &cache_key).await {
        if stored.request_sha256 != request_sha256 {
            return ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "idempotency_key_reused",
                "idempotency key was used with a different request body",
            )
            .into_response();
        }
        return rebuild(stored);
    }

    // Miss: run the handler, then store the result if it succeeded.
    let response = next
        .run(Request::from_parts(parts, Body::from(req_bytes)))
        .await;
    let (parts, body) = response.into_parts();
    let status = parts.status;
    let bytes = match to_bytes(body, cfg.body_cap).await {
        Ok(b) => b,
        Err(_) => {
            // Body too large, or the stream broke. The original is
            // already consumed, so return an empty body and store
            // nothing.
            return Response::from_parts(parts, Body::empty());
        }
    };

    let cacheable = (cfg.cache_status)(status);
    let sets_cookie = parts.headers.contains_key(SET_COOKIE);
    if cacheable && sets_cookie {
        tracing::warn!(
            cache_key,
            "idempotency: response sets a cookie, so it is not stored"
        );
    }
    if cacheable && !sets_cookie {
        let stored = StoredResponse {
            request_sha256,
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
        // A failed write is logged and ignored, so the caller still
        // gets the successful response.
        if let Ok(json) = serde_json::to_string(&stored) {
            if let Err(e) = cfg.cache.set(&cache_key, &json, Some(cfg.ttl)).await {
                tracing::warn!(error = %e, cache_key, "idempotency: cache write failed");
            }
        }
    }

    Response::from_parts(parts, Body::from(bytes))
}

/// Feeds length-prefixed fields to SHA-256, so no two field lists hash alike.
struct KeyHasher(Sha256);

impl KeyHasher {
    fn field(&mut self, bytes: &[u8]) {
        self.0.update((bytes.len() as u64).to_le_bytes());
        self.0.update(bytes);
    }

    fn opt(&mut self, bytes: Option<&[u8]>) {
        match bytes {
            None => self.0.update([0]),
            Some(b) => {
                self.0.update([1]);
                self.field(b);
            }
        }
    }

    fn all(&mut self, parts: &Parts, name: HeaderName) {
        let values: Vec<&HeaderValue> = parts.headers.get_all(name).iter().collect();
        self.0.update((values.len() as u64).to_le_bytes());
        for v in values {
            self.field(v.as_bytes());
        }
    }
}

fn too_large() -> Response<Body> {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "payload_too_large",
        "request body too large",
    )
    .into_response()
}

/// Whether a body read failed on the size cap, not on the stream.
fn over_cap(e: &axum::Error) -> bool {
    let mut err: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(e) = err {
        if e.is::<http_body_util::LengthLimitError>() {
            return true;
        }
        err = e.source();
    }
    false
}

/// Headers that carry a credential, hashed only when no identity is resolved.
const CREDENTIAL_HEADERS: [HeaderName; 3] =
    [AUTHORIZATION, COOKIE, HeaderName::from_static("x-api-key")];

/// Cache key for who sent the request, where, and with which client key.
fn cache_key(scope: &str, parts: &Parts, tenant: Option<&str>, key: &str) -> String {
    let mut h = KeyHasher(Sha256::new());
    h.field(scope.as_bytes());
    let host = parts
        .uri
        .authority()
        .map(|a| a.as_str().as_bytes())
        .or_else(|| parts.headers.get(HOST).map(HeaderValue::as_bytes));
    h.opt(host);
    h.opt(tenant.map(str::as_bytes));
    // A resolved identity survives a token refresh; raw headers do not.
    if !principal_fields(&mut h, parts) {
        for name in CREDENTIAL_HEADERS {
            h.all(parts, name);
        }
    }
    h.field(parts.method.as_str().as_bytes());
    // A nested router strips its prefix from `uri`; key on the full path.
    let uri = parts
        .extensions
        .get::<OriginalUri>()
        .map_or(&parts.uri, |o| &o.0);
    h.opt(uri.path_and_query().map(|pq| pq.as_str().as_bytes()));
    h.field(key.as_bytes());
    format!("idem:{}", hex(&h.0.finalize()))
}

/// Hash the resolved caller; `false` when the request has none.
#[cfg(feature = "tenancy")]
fn principal_fields(h: &mut KeyHasher, parts: &Parts) -> bool {
    use crate::tenancy::{Principal, PrincipalKind};
    let Some(p) = Principal::from_parts(parts) else {
        h.field(b"anonymous");
        return false;
    };
    h.field(match p.kind {
        PrincipalKind::User => b"user",
        PrincipalKind::Agent => b"agent",
    });
    h.field(&p.user_id.to_le_bytes());
    h.opt(p.agent_id.map(i64::to_le_bytes).as_ref().map(|b| &b[..]));
    h.opt(p.tenant.as_deref().map(str::as_bytes));
    true
}

#[cfg(not(feature = "tenancy"))]
fn principal_fields(h: &mut KeyHasher, _parts: &Parts) -> bool {
    h.field(b"anonymous");
    false
}

/// The request's tenant slug. `Err` when the tenant context failed to resolve.
#[cfg(feature = "tenancy")]
async fn request_tenant(parts: &Parts) -> Result<Option<String>, ()> {
    use crate::tenancy::TenantSlug;
    if let Some(TenantSlug(s)) = parts.extensions.get::<TenantSlug>() {
        return Ok(Some(s.clone()));
    }
    match crate::tenancy::middleware::request_org(parts, &parts.extensions).await {
        None => Ok(None),
        Some(Ok(org)) => Ok(org.map(|o| o.slug)),
        Some(Err(e)) => {
            tracing::warn!(error = %e, "idempotency: tenant resolution failed, key not used");
            Err(())
        }
    }
}

#[cfg(not(feature = "tenancy"))]
async fn request_tenant(_parts: &Parts) -> Result<Option<String>, ()> {
    Ok(None)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
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
    // Mark replays, so clients and dashboards can see deduped traffic.
    resp.headers_mut().insert(
        HeaderName::from_static("idempotent-replayed"),
        HeaderValue::from_static("true"),
    );
    let _ = headers_align_content_length(resp.headers_mut());
    resp
}

/// Hook for fixing up `Content-Length` on a replayed response. It does
/// nothing today: the cached body is already in memory, so axum works
/// the length out on the way out.
fn headers_align_content_length(_headers: &mut HeaderMap) -> Result<(), ()> {
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
        // The replay must match call 0, not call 1.
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

        // Same key, different scopes, so both handlers run.
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
        // The 409 was stored and replayed.
        assert_eq!(
            r2.headers()
                .get("idempotent-replayed")
                .and_then(|v| v.to_str().ok()),
            Some("true")
        );
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    /// A counting app on two paths; each call answers `call-<n>`.
    fn counting_app(counter: StdArc<AtomicUsize>, layer: IdempotencyLayer) -> Router {
        let handler = move |body: String| {
            let c = counter.clone();
            async move { format!("call-{}:{body}", c.fetch_add(1, Ordering::SeqCst)) }
        };
        Router::new()
            .route("/a", post(handler.clone()))
            .route("/b", post(handler))
            .idempotency(layer)
    }

    fn keyed(path: &str) -> axum::http::request::Builder {
        Request::builder()
            .method(Method::POST)
            .uri(path)
            .header("host", "shop.example")
            .header("idempotency-key", "k1")
    }

    #[tokio::test]
    async fn same_key_from_two_credentials_does_not_replay() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let app = counting_app(counter.clone(), IdempotencyLayer::new(cache()));
        let send = |auth: &'static str| {
            keyed("/a")
                .header("authorization", auth)
                .body(Body::empty())
                .unwrap()
        };
        let r1 = app.clone().oneshot(send("Bearer alice")).await.unwrap();
        assert_eq!(body_string(r1).await, "call-0:");
        let r2 = app.clone().oneshot(send("Bearer bob")).await.unwrap();
        assert!(r2.headers().get("idempotent-replayed").is_none());
        assert_eq!(body_string(r2).await, "call-1:");
        let r3 = app.oneshot(send("Bearer alice")).await.unwrap();
        assert_eq!(body_string(r3).await, "call-0:");
    }

    #[cfg(feature = "tenancy")]
    #[tokio::test]
    async fn same_key_from_two_principals_does_not_replay() {
        use crate::tenancy::Principal;
        let counter = StdArc::new(AtomicUsize::new(0));
        // Stands in for an auth layer mounted outside the idempotency layer.
        let app = counting_app(counter.clone(), IdempotencyLayer::new(cache())).layer(
            axum::middleware::from_fn(|mut req: Request<Body>, next: Next| async move {
                let user = req
                    .headers()
                    .get("x-test-user")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<i64>().ok());
                if let Some(id) = user {
                    req.extensions_mut()
                        .insert(Principal::user(id, false, None));
                }
                next.run(req).await
            }),
        );
        let send = |user: Option<&'static str>| {
            let b = keyed("/a");
            let b = match user {
                Some(u) => b.header("x-test-user", u),
                None => b,
            };
            b.body(Body::empty()).unwrap()
        };
        let r = app.clone().oneshot(send(Some("1"))).await.unwrap();
        assert_eq!(body_string(r).await, "call-0:");
        let r = app.clone().oneshot(send(Some("2"))).await.unwrap();
        assert_eq!(body_string(r).await, "call-1:");
        let r = app.clone().oneshot(send(None)).await.unwrap();
        assert_eq!(
            body_string(r).await,
            "call-2:",
            "anonymous is its own caller"
        );
        let r = app.oneshot(send(Some("1"))).await.unwrap();
        assert_eq!(body_string(r).await, "call-0:");
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn same_key_on_two_paths_does_not_replay() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let app = counting_app(counter.clone(), IdempotencyLayer::new(cache()));
        let r = app
            .clone()
            .oneshot(keyed("/a").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(body_string(r).await, "call-0:");
        let r = app
            .oneshot(keyed("/b").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(r.headers().get("idempotent-replayed").is_none());
        assert_eq!(body_string(r).await, "call-1:");
    }

    #[tokio::test]
    async fn set_cookie_response_is_never_replayed() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let app = Router::new()
            .route(
                "/",
                post(move || {
                    let c = c.clone();
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        ([(SET_COOKIE, "session=secret; HttpOnly")], "ok")
                    }
                }),
            )
            .idempotency(IdempotencyLayer::new(cache()));
        for _ in 0..2 {
            let r = app
                .clone()
                .oneshot(keyed("/").body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert!(r.headers().get("idempotent-replayed").is_none());
        }
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn reused_key_with_different_body_is_422() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let app = counting_app(counter.clone(), IdempotencyLayer::new(cache()));
        let send = |body: &'static str| keyed("/a").body(Body::from(body)).unwrap();
        let r = app.clone().oneshot(send("amount=10")).await.unwrap();
        assert_eq!(
            body_string(r).await,
            "call-0:amount=10",
            "body reaches handler"
        );
        let r = app.clone().oneshot(send("amount=99")).await.unwrap();
        assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let r = app.oneshot(send("amount=10")).await.unwrap();
        assert_eq!(body_string(r).await, "call-0:amount=10");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    fn parts_for(b: axum::http::request::Builder) -> Parts {
        b.method(Method::POST).body(()).unwrap().into_parts().0
    }

    #[test]
    fn key_fields_cannot_shift_across_a_separator() {
        // Path and client key are hashed back to back.
        let ab = parts_for(Request::builder().uri("/ab"));
        let a = parts_for(Request::builder().uri("/a"));
        assert_ne!(cache_key("", &ab, None, "c"), cache_key("", &a, None, "bc"));
        assert_eq!(cache_key("x", &a, None, "z").len(), "idem:".len() + 64);
    }

    #[test]
    fn host_tenant_and_credentials_are_key_components() {
        let base = || Request::builder().uri("/a").header("host", "one.example");
        let k =
            |b: axum::http::request::Builder, t: Option<&str>| cache_key("", &parts_for(b), t, "k");
        let plain = k(base(), None);
        assert_ne!(
            plain,
            k(
                Request::builder().uri("/a").header("host", "two.example"),
                None
            )
        );
        assert_ne!(plain, k(base(), Some("acme")));
        assert_ne!(k(base(), Some("acme")), k(base(), Some("globex")));
        assert_ne!(plain, k(base().header("cookie", "sid=1"), None));
        assert_ne!(plain, k(base().header("x-api-key", "live_1"), None));
        assert_ne!(
            k(base().header("x-api-key", "live_1"), None),
            k(base().header("x-api-key", "live_2"), None)
        );
    }

    #[test]
    fn only_a_size_limit_error_counts_as_over_cap() {
        let io = axum::Error::new(std::io::Error::other("connection reset"));
        assert!(!over_cap(&io));
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let err = rt
            .block_on(to_bytes(Body::from("0123456789"), 4))
            .unwrap_err();
        assert!(over_cap(&err));
    }

    #[tokio::test]
    async fn body_over_cap_is_413_declared_or_streamed() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let app = counting_app(counter.clone(), IdempotencyLayer::new(cache()).body_cap(4));
        let declared = keyed("/a")
            .header("content-length", "10")
            .body(Body::from("0123456789"))
            .unwrap();
        let r = app.clone().oneshot(declared).await.unwrap();
        assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
        // No Content-Length: the cap trips while the stream is read.
        let streamed = keyed("/a").body(Body::from("0123456789")).unwrap();
        let r = app.oneshot(streamed).await.unwrap();
        assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn full_path_is_keyed_under_a_nested_router() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let shared = cache();
        let app = Router::new()
            .nest(
                "/v1",
                counting_app(counter.clone(), IdempotencyLayer::new(shared.clone())),
            )
            .nest(
                "/v2",
                counting_app(counter.clone(), IdempotencyLayer::new(shared)),
            );
        let r = app
            .clone()
            .oneshot(keyed("/v1/a").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(body_string(r).await, "call-0:");
        let r = app
            .oneshot(keyed("/v2/a").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(r.headers().get("idempotent-replayed").is_none());
        assert_eq!(body_string(r).await, "call-1:");
    }

    #[tokio::test]
    async fn same_key_from_two_api_keys_does_not_replay() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let app = counting_app(counter.clone(), IdempotencyLayer::new(cache()));
        let send = |k: &'static str| {
            keyed("/a")
                .header("x-api-key", k)
                .body(Body::empty())
                .unwrap()
        };
        let r = app.clone().oneshot(send("live_alice")).await.unwrap();
        assert_eq!(body_string(r).await, "call-0:");
        let r = app.oneshot(send("live_bob")).await.unwrap();
        assert_eq!(body_string(r).await, "call-1:");
    }

    /// Inserts `Principal::user(<x-test-user>)` and `TenantSlug(<x-test-slug>)`,
    /// standing in for an auth layer mounted outside the idempotency layer.
    #[cfg(feature = "tenancy")]
    fn with_test_identity(app: Router) -> Router {
        use crate::tenancy::{Principal, TenantSlug};
        app.layer(axum::middleware::from_fn(
            |mut req: Request<Body>, next: Next| async move {
                let h = req.headers();
                let user = h
                    .get("x-test-user")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<i64>().ok());
                let slug = h
                    .get("x-test-slug")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                if let Some(id) = user {
                    req.extensions_mut()
                        .insert(Principal::user(id, false, None));
                }
                if let Some(s) = slug {
                    req.extensions_mut().insert(TenantSlug(s));
                }
                next.run(req).await
            },
        ))
    }

    #[cfg(feature = "tenancy")]
    #[tokio::test]
    async fn resolved_caller_replays_across_a_token_refresh() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let app = with_test_identity(counting_app(
            counter.clone(),
            IdempotencyLayer::new(cache()),
        ));
        let send = |auth: &'static str| {
            keyed("/a")
                .header("x-test-user", "7")
                .header("authorization", auth)
                .header("cookie", auth)
                .body(Body::empty())
                .unwrap()
        };
        let r = app.clone().oneshot(send("Bearer old")).await.unwrap();
        assert_eq!(body_string(r).await, "call-0:");
        let r = app.oneshot(send("Bearer refreshed")).await.unwrap();
        assert_eq!(body_string(r).await, "call-0:", "same caller, new token");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[cfg(feature = "tenancy")]
    #[tokio::test]
    async fn same_key_under_two_tenant_slugs_does_not_replay() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let app = with_test_identity(counting_app(
            counter.clone(),
            IdempotencyLayer::new(cache()),
        ));
        let send = |slug: &'static str| {
            keyed("/a")
                .header("x-test-slug", slug)
                .body(Body::empty())
                .unwrap()
        };
        let r = app.clone().oneshot(send("acme")).await.unwrap();
        assert_eq!(body_string(r).await, "call-0:");
        let r = app.oneshot(send("globex")).await.unwrap();
        assert_eq!(body_string(r).await, "call-1:");
    }

    /// `X-Org: <slug>` → a synthetic Org, no registry lookup.
    #[cfg(all(feature = "tenancy", feature = "sqlite"))]
    struct XOrgResolver;

    #[cfg(all(feature = "tenancy", feature = "sqlite"))]
    #[async_trait::async_trait]
    impl crate::tenancy::OrgResolver for XOrgResolver {
        async fn resolve(
            &self,
            parts: &Parts,
            _registry: &crate::sql::Pool,
        ) -> Result<Option<crate::tenancy::Org>, crate::tenancy::TenancyError> {
            Ok(parts
                .headers
                .get("x-org")
                .and_then(|v| v.to_str().ok())
                .map(|slug| crate::tenancy::Org {
                    slug: slug.to_owned(),
                    ..crate::testkit::org()
                }))
        }
    }

    #[cfg(all(feature = "tenancy", feature = "sqlite"))]
    #[tokio::test]
    async fn two_tenants_by_header_on_one_host_do_not_share_replays() {
        use crate::extractors::DatabaseTenantContext;
        use crate::tenancy::{session::SessionSecret, BackendKind, ChainResolver, DatabasePools};
        let ctx = StdArc::new(DatabaseTenantContext {
            pools: StdArc::new(DatabasePools::<sqlx::Sqlite>::new(BackendKind::Sqlite)),
            resolver: ChainResolver::new().push(XOrgResolver),
            session_secret: SessionSecret::from_bytes(vec![1; 32]),
            operator_secret: SessionSecret::from_bytes(vec![2; 32]),
            registry: crate::sql::Pool::Sqlite(
                sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap(),
            ),
        });
        let counter = StdArc::new(AtomicUsize::new(0));
        let app = counting_app(counter.clone(), IdempotencyLayer::new(cache()))
            .layer(axum::Extension(ctx));
        let send = |org: &'static str| {
            keyed("/a")
                .header("x-org", org)
                .body(Body::empty())
                .unwrap()
        };
        let r = app.clone().oneshot(send("acme")).await.unwrap();
        assert_eq!(body_string(r).await, "call-0:");
        let r = app.clone().oneshot(send("globex")).await.unwrap();
        assert!(r.headers().get("idempotent-replayed").is_none());
        assert_eq!(body_string(r).await, "call-1:");
        let r = app.oneshot(send("acme")).await.unwrap();
        assert_eq!(body_string(r).await, "call-0:", "own tenant still replays");
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
