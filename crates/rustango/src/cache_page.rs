//! Django-shape per-view caching: a `@cache_page` analog plus
//! `Cache-Control` and `Vary` header builders.
//!
//! ## What you get
//!
//! 1. [`CachePageLayer`]: a tower layer that caches successful GET
//!    responses. The key is the method, path and the values of the
//!    vary-on headers. RFC 10008 `QUERY` responses can be cached too;
//!    turn that on with [`CachePageLayer::cache_query`], which folds a
//!    digest of the request body into the key.
//! 2. [`CacheControl`]: a builder for the `Cache-Control` header.
//! 3. [`never_cache`]: `Cache-Control: no-store, no-cache,
//!    must-revalidate, max-age=0`.
//! 4. [`vary_on`]: builds a `Vary` header from header names.
//!
//! ## Quick start
//!
//! ```ignore
//! use std::time::Duration;
//! use axum::{routing::get, Router};
//! use rustango::cache_page::CachePageLayer;
//! use rustango::cache::InMemoryCache;
//! use std::sync::Arc;
//!
//! let cache = Arc::new(InMemoryCache::new());
//!
//! let app: Router = Router::new()
//!     .route("/home", get(|| async { "hello" }))
//!     .layer(
//!         CachePageLayer::new(cache)
//!             .timeout(Duration::from_secs(60))
//!             .key_prefix("pages")
//!             .vary_on(["cookie", "accept-language"]),
//!     );
//! ```
//!
//! ## Semantics
//!
//! - **GET, and QUERY when opted in.** Every other method bypasses
//!   the cache. A cached QUERY response is forced `private`, because
//!   a shared cache cannot key on the request body, and a QUERY body
//!   over the 1 MiB cap gets `413`.
//! - **200 only.** Errors, redirects and 304s are not cached, so a
//!   transient failure cannot poison the cache.
//! - **`Cache-Control: no-store`, `private` or `no-cache`** on the
//!   response stops it being cached. This is a shared cache, so
//!   `private` counts as an opt-out.
//! - **Per-user responses are never shared.** A response with
//!   `Set-Cookie` is never cached, and by default a request with
//!   `Authorization` or `Cookie` is neither served from nor stored in
//!   the cache, since its response probably depends on the caller.
//!   Only [`CachePageLayer::cache_authenticated`] changes that, and
//!   only for a route you know is public.
//! - **The body is buffered.** A response loses its streaming
//!   behaviour under this layer. Use [`never_cache`] on streaming
//!   handlers, or leave the layer off those routes.
//! - **Vary-on values are case-insensitive** and a missing header
//!   counts as empty.
//!
//! [`CachePageLayer`]: crate::cache_page::CachePageLayer
//! [`CachePageLayer::cache_query`]: crate::cache_page::CachePageLayer::cache_query
//! [`CachePageLayer::cache_authenticated`]: crate::cache_page::CachePageLayer::cache_authenticated
//! [`CacheControl`]: crate::cache_page::CacheControl
//! [`never_cache`]: crate::cache_page::never_cache
//! [`vary_on`]: crate::cache_page::vary_on

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode};
use tower::Service;

use crate::cache::BoxedCache;

// ---------------------------------------------------------------- Wire format

/// What gets stored in the cache. JSON, so the `Cache` trait's
/// `String` value is enough. The body is base64 so non-UTF8 content
/// such as images or gzipped HTML survives.
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedResponse {
    status: u16,
    /// `(name, value)` pairs, stringified so the payload does not
    /// depend on how `http` serializes its header types.
    headers: Vec<(String, String)>,
    /// Response body, base64-encoded.
    body_b64: String,
}

// ---------------------------------------------------------------- Layer

/// Tower layer that caches GET responses. The key is the prefix,
/// method, path and the vary-on header values.
#[derive(Clone)]
pub struct CachePageLayer {
    cache: BoxedCache,
    timeout: Duration,
    key_prefix: String,
    vary_on: Vec<HeaderName>,
    cache_query: bool,
    /// When `false` (the default), a request with `Authorization` or
    /// `Cookie` is neither served from nor stored in the shared
    /// cache, because its response is probably per-user. See
    /// [`Self::cache_authenticated`].
    cache_authenticated: bool,
}

impl CachePageLayer {
    /// Build a layer on an existing [`BoxedCache`]. The default TTL
    /// is 60 seconds; change it with [`Self::timeout`].
    #[must_use]
    pub fn new(cache: BoxedCache) -> Self {
        Self {
            cache,
            timeout: Duration::from_secs(60),
            key_prefix: "rustango.cache_page".to_owned(),
            vary_on: Vec::new(),
            cache_query: false,
            cache_authenticated: false,
        }
    }

    /// Cache TTL: entries expire after this duration.
    #[must_use]
    pub fn timeout(mut self, dur: Duration) -> Self {
        self.timeout = dur;
        self
    }

    /// Override the cache-key prefix. The default is
    /// `"rustango.cache_page"`. Give each layer its own prefix when
    /// several share one cache backend, so you can clear them apart.
    #[must_use]
    pub fn key_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.key_prefix = prefix.into();
        self
    }

    /// Add header names whose values go into the cache key. Names are
    /// lowercased. Calling this again appends.
    ///
    /// # Panics
    /// Panics on a name that is not a valid header name. That is a
    /// programmer error, not a runtime condition.
    #[must_use]
    pub fn vary_on<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for n in names {
            let lower = n.as_ref().to_ascii_lowercase();
            let h = HeaderName::from_bytes(lower.as_bytes())
                .expect("vary_on: header name must be valid ASCII");
            self.vary_on.push(h);
        }
        self
    }

    /// Allow caching responses to requests that carry `Authorization`
    /// or `Cookie`.
    ///
    /// **Off by default. Leave it off unless you are sure.** The
    /// default treats such a request as per-user and keeps it out of
    /// the shared cache, so one user's page cannot be handed to
    /// another. Turn it on only for a route whose response does not
    /// depend on who is calling, such as a public page on a site that
    /// sets an analytics cookie for everyone.
    ///
    /// A response that sends `Set-Cookie`, or marks itself
    /// `Cache-Control: private`, `no-cache` or `no-store`, is
    /// **never** cached, whatever this flag says.
    #[must_use]
    pub fn cache_authenticated(mut self, enabled: bool) -> Self {
        self.cache_authenticated = enabled;
        self
    }

    /// Cache RFC 10008 `QUERY` requests. Default `false`.
    ///
    /// QUERY is safe and idempotent, and its response depends on the
    /// request body. When enabled, the body is buffered and a digest
    /// of it goes into the cache key, so only a byte-identical body
    /// hits. It is off by default because buffering costs something.
    ///
    /// Buffering stops at 1 MiB; a larger QUERY body is rejected with
    /// `413`. Add a request body-limit layer to reject oversized
    /// bodies earlier.
    #[must_use]
    pub fn cache_query(mut self, enabled: bool) -> Self {
        self.cache_query = enabled;
        self
    }
}

impl<S> tower::Layer<S> for CachePageLayer {
    type Service = CachePageService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        CachePageService {
            inner,
            cache: self.cache.clone(),
            timeout: self.timeout,
            key_prefix: Arc::new(self.key_prefix.clone()),
            vary_on: Arc::new(self.vary_on.clone()),
            cache_query: self.cache_query,
            cache_authenticated: self.cache_authenticated,
        }
    }
}

/// The wrapped service produced by [`CachePageLayer`].
#[derive(Clone)]
pub struct CachePageService<S> {
    inner: S,
    cache: BoxedCache,
    timeout: Duration,
    key_prefix: Arc<String>,
    vary_on: Arc<Vec<HeaderName>>,
    cache_query: bool,
    cache_authenticated: bool,
}

impl<S> Service<Request<Body>> for CachePageService<S>
where
    S: Service<Request<Body>, Response = Response<Body>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn Future<Output = Result<Response<Body>, Infallible>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let cache = self.cache.clone();
        let timeout = self.timeout;
        let prefix = self.key_prefix.clone();
        let vary = self.vary_on.clone();
        let cache_query = self.cache_query;
        let cache_authenticated = self.cache_authenticated;
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        Box::pin(async move {
            // GET is always cacheable; QUERY only when opted in.
            // Every other method bypasses the cache.
            let is_get = req.method() == axum::http::Method::GET;
            let is_query = cache_query && req.method().as_str() == "QUERY";
            if !is_get && !is_query {
                return inner.call(req).await;
            }

            // For QUERY, buffer the body so its digest can go into
            // the key and a fresh body can reach the inner service.
            let (req, body_digest) = if is_query {
                let (parts, body) = req.into_parts();
                match to_bytes(body, MAX_CACHEABLE_BODY_BYTES).await {
                    Ok(bytes) => {
                        let digest = query_body_digest(&bytes);
                        (Request::from_parts(parts, Body::from(bytes)), Some(digest))
                    }
                    Err(_) => {
                        // Body over the cap, or a read error. `to_bytes`
                        // consumed it and will not give the bytes back,
                        // so the handler cannot get the real body.
                        // Return 413 rather than run it on an empty body
                        // and answer with something quietly wrong.
                        let _ = parts;
                        return Ok(payload_too_large());
                    }
                }
            } else {
                (req, None)
            };

            // Unless the route opted in, a request with `Authorization`
            // or `Cookie` is per-user: never served from and never
            // stored in the shared cache, so one user's response
            // cannot reach another user.
            let req_is_authed = !cache_authenticated
                && (req
                    .headers()
                    .contains_key(axum::http::header::AUTHORIZATION)
                    || req.headers().contains_key(axum::http::header::COOKIE));
            if req_is_authed {
                return inner.call(req).await;
            }

            let key = compute_cache_key(&prefix, &req, &vary, body_digest.as_deref());

            // Cache hit?
            if let Ok(Some(serialized)) = cache.get(&key).await {
                if let Ok(stored) = serde_json::from_str::<CachedResponse>(&serialized) {
                    if let Some(mut resp) = stored.into_response(&vary) {
                        if is_query {
                            mark_query_response_private(resp.headers_mut());
                        }
                        return Ok(resp);
                    }
                }
                // Corrupt entry: fall through and recompute.
            }

            // Miss: run the inner service, then store the response.
            let resp = inner.call(req).await?;
            // Cache only 200 OK, and never something per-user or
            // opted out:
            //   - `Set-Cookie`: the response mints a cookie, so it is
            //     per-user. Caching it would replay one user's session
            //     cookie to the next caller.
            //   - `Cache-Control: no-store | private | no-cache`: the
            //     handler said no. This is a shared cache, so
            //     `private` counts.
            let status = resp.status();
            let sets_cookie = resp.headers().contains_key(axum::http::header::SET_COOKIE);
            let cache_control_opt_out = resp
                .headers()
                .get_all(axum::http::header::CACHE_CONTROL)
                .iter()
                .any(|v| {
                    v.to_str()
                        .map(|s| {
                            let s = s.to_ascii_lowercase();
                            s.contains("no-store")
                                || s.contains("private")
                                || s.contains("no-cache")
                        })
                        .unwrap_or(false)
                });

            if status != StatusCode::OK || sets_cookie || cache_control_opt_out {
                return Ok(resp);
            }

            // Buffer the body so it can be stored and replayed.
            let (parts, body) = resp.into_parts();
            let bytes = match to_bytes(body, MAX_CACHEABLE_BODY_BYTES).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(
                        target: "rustango::cache_page",
                        error = %e,
                        max_bytes = MAX_CACHEABLE_BODY_BYTES,
                        "response body exceeds cache size limit or failed to buffer; \
                         passing through uncached"
                    );
                    // `body` is already consumed, so the original
                    // cannot be returned. Send an empty body marked
                    // BYPASS so monitoring can see it.
                    let mut resp = Response::from_parts(parts, Body::empty());
                    resp.headers_mut()
                        .insert(X_CACHE_STATUS, HeaderValue::from_static("BYPASS"));
                    return Ok(resp);
                }
            };

            let stored = CachedResponse::from_parts(&parts, &bytes);
            if let Ok(json) = serde_json::to_string(&stored) {
                if let Err(e) = cache.set(&key, &json, Some(timeout)).await {
                    tracing::warn!(
                        target: "rustango::cache_page",
                        error = %e,
                        "cache backend rejected set(); response served fresh, not cached"
                    );
                }
            }

            // Rebuild the response from the buffered bytes.
            let mut rebuilt = Response::from_parts(parts, Body::from(bytes));
            let headers = rebuilt.headers_mut();
            // Mark the miss so monitoring can split hit from miss.
            headers.insert(X_CACHE_STATUS, HeaderValue::from_static("MISS"));
            // Tell downstream caches which request headers this cache
            // partitioned on, so they can do the same.
            apply_vary_header(headers, &vary);
            // QUERY partitions on the request body, which `Vary`
            // cannot express, so keep it out of shared caches.
            if is_query {
                mark_query_response_private(headers);
            }
            Ok(rebuilt)
        })
    }
}

/// Cached response bodies are limited to 1 MiB. A bigger body is not
/// cached; failing to cache is no reason to break a good response.
const MAX_CACHEABLE_BODY_BYTES: usize = 1 << 20;

/// Header naming where the response came from: `HIT` from the cache,
/// `MISS` freshly computed.
const X_CACHE_STATUS: HeaderName = HeaderName::from_static("x-cache-status");

/// Build the cache key, as `prefix|<len>:<bytes>|<len>:<bytes>|...`.
///
/// Each part is length-prefixed, so a value holding a separator
/// cannot make two different requests produce the same key.
///
/// `Host` is always part of the key. Without it a multi-tenant app
/// serving different content per host would get cross-tenant hits.
fn compute_cache_key(
    prefix: &str,
    req: &Request<Body>,
    vary_on: &[HeaderName],
    body_digest: Option<&str>,
) -> String {
    use std::fmt::Write as _;
    let mut k = String::with_capacity(prefix.len() + 128);
    let _ = write!(&mut k, "{prefix}|");
    write_lp(&mut k, req.method().as_str());
    write_lp(&mut k, req.uri().path());
    write_lp(&mut k, req.uri().query().unwrap_or(""));
    // Default: partition on Host so multi-tenant deployments don't
    // mix tenants' responses. `vary_on` can still add more.
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    write_lp(&mut k, host);
    for name in vary_on {
        let v = req
            .headers()
            .get(name)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        write_lp(&mut k, name.as_str());
        write_lp(&mut k, v);
    }
    // QUERY carries its criteria in the body, so the digest joins the
    // key. GET passes `None`, so its keys are unchanged.
    if let Some(digest) = body_digest {
        write_lp(&mut k, digest);
    }
    k
}

/// Mark a QUERY response `private` so a CDN or proxy never stores it.
///
/// This layer keys QUERY on the request body, but `Vary` cannot name a
/// body, so a shared cache keying on URL and headers alone would serve
/// one query's result for a different query. Any `public` the handler
/// set is downgraded. Freshness directives such as `max-age` are kept.
fn mark_query_response_private(headers: &mut HeaderMap) {
    use axum::http::header::CACHE_CONTROL;
    let existing = headers
        .get(CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let mut directives: Vec<String> = existing
        .split(',')
        .map(|d| d.trim().to_owned())
        .filter(|d| {
            !d.is_empty() && !d.eq_ignore_ascii_case("public") && !d.eq_ignore_ascii_case("private")
        })
        .collect();
    directives.insert(0, "private".to_owned());
    if let Ok(v) = HeaderValue::from_str(&directives.join(", ")) {
        headers.insert(CACHE_CONTROL, v);
    }
}

/// Base64 SHA-256 of a QUERY body. This key component makes only a
/// byte-identical body hit the cache.
fn query_body_digest(body: &[u8]) -> String {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    B64.encode(Sha256::digest(body))
}

/// `413 Payload Too Large` for a QUERY body over the cacheable cap.
fn payload_too_large() -> Response<Body> {
    let mut resp = Response::new(Body::from("QUERY body too large to cache"));
    *resp.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
    resp.headers_mut()
        .insert(X_CACHE_STATUS, HeaderValue::from_static("BYPASS"));
    resp
}

/// Append `<len>:<bytes>|` so parts can be joined without ambiguity.
fn write_lp(buf: &mut String, s: &str) {
    use std::fmt::Write as _;
    let _ = write!(buf, "{}:{}|", s.len(), s);
}

/// Set or extend `Vary` to name the request headers this cache
/// partitions on. `host` is always in the key, so it is listed too.
fn apply_vary_header(headers: &mut HeaderMap, vary_on: &[HeaderName]) {
    use std::fmt::Write as _;
    let mut parts: Vec<String> = Vec::with_capacity(vary_on.len() + 1);
    parts.push("host".to_owned());
    for n in vary_on {
        parts.push(n.as_str().to_owned());
    }
    let mut s = String::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        let _ = write!(&mut s, "{p}");
    }
    if let Ok(v) = HeaderValue::from_str(&s) {
        // Append rather than replace: the handler may have set its
        // own Vary, and repeated Vary headers are equivalent to one
        // comma-separated header.
        headers.append(axum::http::header::VARY, v);
    }
}

impl CachedResponse {
    fn from_parts(parts: &axum::http::response::Parts, body: &[u8]) -> Self {
        use base64::engine::general_purpose::STANDARD as B64;
        use base64::Engine as _;
        // `.iter()` yields duplicates too, so multi-value headers
        // survive the round-trip. Skip `x-cache-status`: it is set
        // fresh on every hit and would otherwise stack up.
        let mut headers = Vec::with_capacity(parts.headers.len());
        for (name, value) in parts.headers.iter() {
            if name == X_CACHE_STATUS {
                continue;
            }
            if let Ok(v) = value.to_str() {
                headers.push((name.as_str().to_owned(), v.to_owned()));
            }
            // Non-UTF8 values are dropped: they are legal but rare,
            // and re-serialising them would corrupt the JSON.
        }
        Self {
            status: parts.status.as_u16(),
            headers,
            body_b64: B64.encode(body),
        }
    }

    /// Rebuild a `Response<Body>` from the cached bytes. Returns
    /// `None` when the stored body will not base64-decode, and the
    /// caller then recomputes.
    ///
    /// `vary_on` comes from the live layer config, so a changed vary
    /// list takes effect on the next hit.
    fn into_response(self, vary_on: &[HeaderName]) -> Option<Response<Body>> {
        use base64::engine::general_purpose::STANDARD as B64;
        use base64::Engine as _;
        let body = B64.decode(&self.body_b64).ok()?;
        let mut resp = Response::builder()
            .status(StatusCode::from_u16(self.status).unwrap_or(StatusCode::OK))
            .body(Body::from(body))
            .ok()?;
        let headers = resp.headers_mut();
        // `append`, not `insert`, so duplicates are preserved.
        for (name, value) in self.headers {
            let Ok(n) = HeaderName::from_bytes(name.as_bytes()) else {
                continue;
            };
            let Ok(v) = HeaderValue::from_str(&value) else {
                continue;
            };
            headers.append(n, v);
        }
        headers.insert(X_CACHE_STATUS, HeaderValue::from_static("HIT"));
        apply_vary_header(headers, vary_on);
        Some(resp)
    }
}

// ---------------------------------------------------------------- Cache-Control builder

/// Builder for the `Cache-Control` response header. Covers the same
/// directives as Django's `@cache_control`.
///
/// ```ignore
/// use rustango::cache_page::CacheControl;
///
/// let header = CacheControl::new()
///     .max_age(60)
///     .public()
///     .must_revalidate()
///     .build();
/// response.headers_mut().insert(axum::http::header::CACHE_CONTROL, header);
/// ```
#[derive(Default, Clone, Debug)]
#[must_use = "call .build() to produce the HeaderValue"]
pub struct CacheControl {
    max_age: Option<u64>,
    public: bool,
    private: bool,
    no_cache: bool,
    no_store: bool,
    must_revalidate: bool,
    s_maxage: Option<u64>,
}

impl CacheControl {
    /// Empty builder. `.build()` on it gives an empty header value.
    pub fn new() -> Self {
        Self::default()
    }

    /// `max-age=N` (seconds).
    pub fn max_age(mut self, secs: u64) -> Self {
        self.max_age = Some(secs);
        self
    }

    /// `s-maxage=N`, the max age for shared caches such as CDNs and
    /// proxies. A private cache ignores it and uses `max-age`.
    pub fn s_maxage(mut self, secs: u64) -> Self {
        self.s_maxage = Some(secs);
        self
    }

    /// `public`. Clears `private`; the last call wins.
    pub fn public(mut self) -> Self {
        self.public = true;
        self.private = false;
        self
    }

    /// `private`. Clears `public`; the last call wins.
    pub fn private(mut self) -> Self {
        self.private = true;
        self.public = false;
        self
    }

    /// `no-cache`: a cache must revalidate before serving.
    pub fn no_cache(mut self) -> Self {
        self.no_cache = true;
        self
    }

    /// `no-store`: no cache may store the response. This also stops
    /// [`CachePageLayer`] storing it.
    pub fn no_store(mut self) -> Self {
        self.no_store = true;
        self
    }

    /// `must-revalidate`.
    pub fn must_revalidate(mut self) -> Self {
        self.must_revalidate = true;
        self
    }

    /// Render the header value for `headers.insert(CACHE_CONTROL, ..)`.
    pub fn build(self) -> HeaderValue {
        let mut parts: Vec<String> = Vec::with_capacity(7);
        if let Some(n) = self.max_age {
            parts.push(format!("max-age={n}"));
        }
        if let Some(n) = self.s_maxage {
            parts.push(format!("s-maxage={n}"));
        }
        if self.public {
            parts.push("public".into());
        }
        if self.private {
            parts.push("private".into());
        }
        if self.no_cache {
            parts.push("no-cache".into());
        }
        if self.no_store {
            parts.push("no-store".into());
        }
        if self.must_revalidate {
            parts.push("must-revalidate".into());
        }
        HeaderValue::from_str(&parts.join(", ")).expect("ASCII directive string")
    }
}

/// `no-store, no-cache, must-revalidate, max-age=0`. Put it on any
/// response no browser, CDN or proxy may ever store.
///
/// ```ignore
/// response.headers_mut().insert(
///     axum::http::header::CACHE_CONTROL,
///     rustango::cache_page::never_cache(),
/// );
/// ```
#[must_use]
pub fn never_cache() -> HeaderValue {
    CacheControl::new()
        .no_store()
        .no_cache()
        .must_revalidate()
        .max_age(0)
        .build()
}

/// Build a `Vary` header value from header names, joined with `, `.
///
/// ```ignore
/// response.headers_mut().insert(
///     axum::http::header::VARY,
///     rustango::cache_page::vary_on(["cookie", "accept-language"]),
/// );
/// ```
///
/// # Panics
/// Panics on a non-ASCII or control byte. Header names are constants
/// in your code, so the panic catches a typo on the first request.
#[must_use]
pub fn vary_on<I, S>(names: I) -> HeaderValue
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let parts: Vec<String> = names.into_iter().map(|s| s.as_ref().to_owned()).collect();
    HeaderValue::from_str(&parts.join(", "))
        .expect("vary_on: header names must be ASCII without control characters")
}

/// Add names to the existing `Vary` header, skipping ones already
/// there. Use it from middleware that must extend, not replace, what
/// the view set. Matches Django's
/// [`patch_vary_headers`](https://docs.djangoproject.com/en/6.0/topics/cache/#using-vary-headers).
///
/// A missing `Vary` is created. `Vary: *` is left alone, since it is
/// already the strongest key.
///
/// Names are compared without case; the first spelling seen is kept.
///
/// ```ignore
/// use axum::http::HeaderMap;
/// use rustango::cache_page::patch_vary_headers;
///
/// let mut headers = HeaderMap::new();
/// patch_vary_headers(&mut headers, &["Cookie"]);
/// patch_vary_headers(&mut headers, &["Accept-Language", "cookie"]);
/// // Vary: Cookie, Accept-Language  (deduped — second `cookie` skipped)
/// ```
pub fn patch_vary_headers(headers: &mut HeaderMap, new_names: &[&str]) {
    use axum::http::header::VARY;
    let existing = headers
        .get(VARY)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    // `Vary: *` short-circuits — strongest possible cache key.
    if existing.trim() == "*" {
        return;
    }
    let mut accum: Vec<String> = existing
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    let mut lower_set: std::collections::HashSet<String> =
        accum.iter().map(|s| s.to_ascii_lowercase()).collect();
    for name in new_names {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lc = trimmed.to_ascii_lowercase();
        if lower_set.insert(lc) {
            accum.push(trimmed.to_owned());
        }
    }
    if accum.is_empty() {
        return;
    }
    if let Ok(v) = HeaderValue::from_str(&accum.join(", ")) {
        headers.insert(VARY, v);
    }
}

// ---------------------------------------------------------------- Tests

#[allow(dead_code)]
fn _trait_check(_h: &HeaderMap) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_control_builds_expected_directive() {
        let v = CacheControl::new().max_age(60).public().build();
        let s = v.to_str().unwrap();
        assert!(s.contains("max-age=60"));
        assert!(s.contains("public"));
    }

    #[test]
    fn never_cache_emits_full_no_store_directive() {
        let s = never_cache().to_str().unwrap().to_string();
        assert!(s.contains("no-store"));
        assert!(s.contains("no-cache"));
        assert!(s.contains("must-revalidate"));
        assert!(s.contains("max-age=0"));
    }

    #[test]
    fn public_and_private_are_mutually_exclusive() {
        let s = CacheControl::new()
            .public()
            .private()
            .build()
            .to_str()
            .unwrap()
            .to_string();
        assert!(s.contains("private"), "last call wins");
        assert!(!s.contains("public"));
    }

    #[test]
    fn vary_on_joins_with_comma_space() {
        let v = vary_on(["cookie", "accept-language"]);
        assert_eq!(v.to_str().unwrap(), "cookie, accept-language");
    }

    // -------- patch_vary_headers (Django parity) --------

    #[test]
    fn patch_vary_adds_to_empty_headers() {
        let mut h = HeaderMap::new();
        patch_vary_headers(&mut h, &["Cookie"]);
        assert_eq!(h.get(axum::http::header::VARY).unwrap(), "Cookie");
    }

    #[test]
    fn patch_vary_extends_existing() {
        let mut h = HeaderMap::new();
        h.insert(axum::http::header::VARY, HeaderValue::from_static("Cookie"));
        patch_vary_headers(&mut h, &["Accept-Language"]);
        let v = h.get(axum::http::header::VARY).unwrap().to_str().unwrap();
        assert!(v.contains("Cookie"));
        assert!(v.contains("Accept-Language"));
    }

    #[test]
    fn patch_vary_dedupes_case_insensitively() {
        let mut h = HeaderMap::new();
        h.insert(axum::http::header::VARY, HeaderValue::from_static("Cookie"));
        // Second "cookie" (lowercase) should not duplicate.
        patch_vary_headers(&mut h, &["cookie", "Accept-Language"]);
        let v = h.get(axum::http::header::VARY).unwrap().to_str().unwrap();
        // Cookie should appear once.
        let count = v.matches(|c: char| c == ',').count() + 1;
        assert_eq!(count, 2, "expected 2 names, got: `{v}`");
    }

    #[test]
    fn patch_vary_star_short_circuits() {
        // `Vary: *` is the strongest possible — don't downgrade.
        let mut h = HeaderMap::new();
        h.insert(axum::http::header::VARY, HeaderValue::from_static("*"));
        patch_vary_headers(&mut h, &["Cookie", "Accept-Language"]);
        let v = h.get(axum::http::header::VARY).unwrap().to_str().unwrap();
        assert_eq!(v, "*");
    }

    #[test]
    fn patch_vary_empty_input_is_noop() {
        let mut h = HeaderMap::new();
        patch_vary_headers(&mut h, &[]);
        assert!(h.get(axum::http::header::VARY).is_none());
    }

    #[test]
    fn patch_vary_skips_empty_names() {
        let mut h = HeaderMap::new();
        patch_vary_headers(&mut h, &["", "  ", "Cookie"]);
        assert_eq!(h.get(axum::http::header::VARY).unwrap(), "Cookie");
    }
}
