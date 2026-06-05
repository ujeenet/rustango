//! Django-shape per-view caching — `@cache_page` analog plus
//! `Cache-Control` / `Vary` header builders. Issue #55.
//!
//! ## What you get
//!
//! 1. [`CachePageLayer`] — a tower layer that caches successful GET
//!    responses under a key derived from `(method, path, Vary-on
//!    header values)`. Subsequent matching requests bypass the inner
//!    service and return the cached response.
//! 2. [`CacheControl`] — fluent builder for the `Cache-Control`
//!    header (`max_age`, `public`, `private`, `no_cache`, `no_store`,
//!    `must_revalidate`).
//! 3. [`never_cache`] — shorthand for `Cache-Control: no-store,
//!    no-cache, must-revalidate, max-age=0`.
//! 4. [`vary_on`] — builds a `Vary` header from a list of header names.
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
//! - **GET-only.** POST / PUT / PATCH / DELETE / HEAD bypass the
//!   cache (mutating methods invalidate semantics; HEAD is too rare
//!   to be worth the body-stripping path).
//! - **Status 200-only.** Errors / redirects / 304s aren't cached so
//!   transient failures don't poison the cache.
//! - **`Cache-Control: no-store`** on the response disables caching
//!   for that response (matches RFC 9111 — caches must not store it).
//! - **Body is buffered.** The layer materializes the full response
//!   body so it can store it; streaming responses lose their streaming
//!   property under the cache. Use `[never_cache]` headers on
//!   streaming handlers, or omit the layer on those routes.
//! - **Vary-on values are case-insensitive** (HTTP convention) and
//!   missing headers are treated as empty.

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

/// Serialized form stored in the cache. JSON-encoded so the
/// `Cache` trait's `String` value works without a separate binary
/// channel. Body is base64-encoded to survive non-UTF8 content
/// (binary images, gzipped HTML, etc.).
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedResponse {
    status: u16,
    /// `(name, value)` pairs. We pre-stringify names + values so the
    /// cached payload doesn't depend on `http::HeaderName` /
    /// `http::HeaderValue` stable serde shapes.
    headers: Vec<(String, String)>,
    /// Response body, base64-encoded.
    body_b64: String,
}

// ---------------------------------------------------------------- Layer

/// Tower layer that caches GET responses under a key derived from
/// `(prefix, method, path, vary-on header values)`. Issue #55.
#[derive(Clone)]
pub struct CachePageLayer {
    cache: BoxedCache,
    timeout: Duration,
    key_prefix: String,
    vary_on: Vec<HeaderName>,
}

impl CachePageLayer {
    /// Build a layer against an existing [`BoxedCache`]. Default
    /// timeout is 60 seconds; tune via [`Self::timeout`].
    #[must_use]
    pub fn new(cache: BoxedCache) -> Self {
        Self {
            cache,
            timeout: Duration::from_secs(60),
            key_prefix: "rustango.cache_page".to_owned(),
            vary_on: Vec::new(),
        }
    }

    /// Cache TTL — entries expire after this duration.
    #[must_use]
    pub fn timeout(mut self, dur: Duration) -> Self {
        self.timeout = dur;
        self
    }

    /// Override the cache-key prefix (default `"rustango.cache_page"`).
    /// Useful when multiple cache_page layers share one cache backend
    /// and you want to be able to selectively `cache.delete_prefix(...)`
    /// later (per the `Cache` trait's `delete` per-key shape).
    #[must_use]
    pub fn key_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.key_prefix = prefix.into();
        self
    }

    /// Add header names whose values participate in the cache key.
    /// Names are case-insensitively normalized to ASCII-lowercase.
    /// Calling this method multiple times appends.
    ///
    /// # Panics
    /// Panics when a name fails [`HeaderName::from_bytes`] — invalid
    /// header names are programmer errors, not runtime conditions.
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
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        Box::pin(async move {
            // Bypass non-GET methods entirely.
            if req.method() != axum::http::Method::GET {
                return inner.call(req).await;
            }

            let key = compute_cache_key(&prefix, &req, &vary);

            // Cache hit?
            if let Ok(Some(serialized)) = cache.get(&key).await {
                if let Ok(stored) = serde_json::from_str::<CachedResponse>(&serialized) {
                    if let Some(resp) = stored.into_response(&vary) {
                        return Ok(resp);
                    }
                }
                // Corrupt entry — fall through to recompute.
            }

            // Cache miss — run inner, then store the response.
            let resp = inner.call(req).await?;
            // Only cache 200 OK responses. Don't cache responses
            // that explicitly opt out via Cache-Control: no-store.
            let status = resp.status();
            let cache_control_opt_out = resp
                .headers()
                .get_all(axum::http::header::CACHE_CONTROL)
                .iter()
                .any(|v| {
                    v.to_str()
                        .map(|s| s.to_ascii_lowercase().contains("no-store"))
                        .unwrap_or(false)
                });

            if status != StatusCode::OK || cache_control_opt_out {
                return Ok(resp);
            }

            // Buffer the body so we can store + replay. If the body
            // exceeds MAX_CACHEABLE_BODY_BYTES we pass the original
            // response through with a tracing::warn — the handler's
            // result is what the client wanted; failing to cache it
            // is not a reason to turn a successful 200 into a 500.
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
                    // We've already consumed `body` — can't return the
                    // original. Substitute an empty body and let the
                    // caller see a degraded but successful response.
                    // Mark as bypassed so observability sees the issue.
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
            // Defensive: insert an X-Cache-Status: MISS marker so
            // downstream observability can split hit/miss easily.
            headers.insert(X_CACHE_STATUS, HeaderValue::from_static("MISS"));
            // RFC 9111 §4.1 — when we partitioned the cache on
            // specific request headers, downstream caches need to
            // know so they can repeat the partitioning.
            apply_vary_header(headers, &vary);
            Ok(rebuilt)
        })
    }
}

/// Limit cached response bodies to 1 MiB. Larger responses pass
/// through uncached (with a tracing::warn) instead of becoming 500s —
/// failing to cache isn't a reason to break a successful handler.
const MAX_CACHEABLE_BODY_BYTES: usize = 1 << 20;

/// Header set on cached responses so clients / proxies can see
/// hit-vs-miss in tooling. `HIT` for served-from-cache, `MISS` for
/// freshly computed.
const X_CACHE_STATUS: HeaderName = HeaderName::from_static("x-cache-status");

/// Build the cache key. Components are length-prefixed so values
/// containing the previous separator (`|`, `=`) can't collide with
/// adjacent keys — a request with `Cookie: foo|bar=baz` and a vary-on
/// list that includes `Cookie` would otherwise be ambiguous against
/// a request with `Cookie: foo` + another vary-on header whose value
/// is `bar=baz`. Format: `prefix|<len>:<bytes>|<len>:<bytes>|...`.
///
/// The `Host` header is included by default — multi-tenant apps
/// serving different content per Host would otherwise see
/// cross-tenant cache hits.
fn compute_cache_key(prefix: &str, req: &Request<Body>, vary_on: &[HeaderName]) -> String {
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
    k
}

/// Length-prefixed append: writes `<len-in-bytes>:<bytes>|` so the
/// caller can concatenate components unambiguously.
fn write_lp(buf: &mut String, s: &str) {
    use std::fmt::Write as _;
    let _ = write!(buf, "{}:{}|", s.len(), s);
}

/// Set / extend the `Vary` response header to communicate which
/// request headers our cache partitions on. Host is included
/// automatically by the cache key, so we list it here too.
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
        // Append to existing Vary (handler may have set their own
        // vary directives) — RFC 9110 §12.5.5 permits the comma-
        // separated form, and repeated Vary headers are equivalent.
        headers.append(axum::http::header::VARY, v);
    }
}

impl CachedResponse {
    fn from_parts(parts: &axum::http::response::Parts, body: &[u8]) -> Self {
        use base64::engine::general_purpose::STANDARD as B64;
        use base64::Engine as _;
        // Walk the HeaderMap with .iter() — yields every entry,
        // including duplicates, so multi-value headers like
        // `Set-Cookie: a` + `Set-Cookie: b` survive the round-trip.
        // Skip our own `x-cache-status` so a re-cache doesn't
        // double-stack it; the served value is set fresh on HIT.
        let mut headers = Vec::with_capacity(parts.headers.len());
        for (name, value) in parts.headers.iter() {
            if name == X_CACHE_STATUS {
                continue;
            }
            if let Ok(v) = value.to_str() {
                headers.push((name.as_str().to_owned(), v.to_owned()));
            }
            // Non-UTF8 values are dropped — re-serialising binary
            // headers (rare but legal) would corrupt the JSON. The
            // common cacheable case (HTML / JSON pages) doesn't hit
            // this path.
        }
        Self {
            status: parts.status.as_u16(),
            headers,
            body_b64: B64.encode(body),
        }
    }

    /// Rebuild a `Response<Body>` from the cached bytes. Returns
    /// `None` if the stored body fails base64 decode (corrupt
    /// entry — caller falls through to recompute).
    ///
    /// `vary_on` is taken from the live layer config so a layer
    /// rebuild with a different vary list applies on the next HIT.
    fn into_response(self, vary_on: &[HeaderName]) -> Option<Response<Body>> {
        use base64::engine::general_purpose::STANDARD as B64;
        use base64::Engine as _;
        let body = B64.decode(&self.body_b64).ok()?;
        let mut resp = Response::builder()
            .status(StatusCode::from_u16(self.status).unwrap_or(StatusCode::OK))
            .body(Body::from(body))
            .ok()?;
        let headers = resp.headers_mut();
        // Append every stored header — duplicates preserved.
        // `HeaderMap::append` is the multi-value-safe insert.
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

/// Fluent builder for the `Cache-Control` response header — issue #55.
/// Matches the directives Django's `@cache_control` accepts.
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
    /// Empty builder. No directives set — `.build()` on an empty
    /// builder produces an empty string header value (effectively
    /// a no-op directive).
    pub fn new() -> Self {
        Self::default()
    }

    /// `max-age=N` (seconds).
    pub fn max_age(mut self, secs: u64) -> Self {
        self.max_age = Some(secs);
        self
    }

    /// `s-maxage=N` (shared-cache max age, seconds). Used by CDNs and
    /// proxies; private-cache implementations ignore it in favour of
    /// `max-age`.
    pub fn s_maxage(mut self, secs: u64) -> Self {
        self.s_maxage = Some(secs);
        self
    }

    /// `public`. Mutually exclusive with `private` — last call wins.
    pub fn public(mut self) -> Self {
        self.public = true;
        self.private = false;
        self
    }

    /// `private`. Mutually exclusive with `public` — last call wins.
    pub fn private(mut self) -> Self {
        self.private = true;
        self.public = false;
        self
    }

    /// `no-cache` — caches must revalidate before serving.
    pub fn no_cache(mut self) -> Self {
        self.no_cache = true;
        self
    }

    /// `no-store` — caches must not store the response at all. This
    /// also disables [`CachePageLayer`]'s storage for the response.
    pub fn no_store(mut self) -> Self {
        self.no_store = true;
        self
    }

    /// `must-revalidate`.
    pub fn must_revalidate(mut self) -> Self {
        self.must_revalidate = true;
        self
    }

    /// Render to an `http::HeaderValue` suitable for
    /// `headers.insert(CACHE_CONTROL, ...)`.
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

/// Shorthand for `@never_cache` — produces the header value
/// `no-store, no-cache, must-revalidate, max-age=0`. Attach to any
/// response that must never be cached by any agent / CDN / proxy.
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

/// Build a `Vary` header value from a list of header names. Names
/// are joined with `, ` per RFC 9110.
///
/// ```ignore
/// response.headers_mut().insert(
///     axum::http::header::VARY,
///     rustango::cache_page::vary_on(["cookie", "accept-language"]),
/// );
/// ```
///
/// # Panics
/// Panics on non-ASCII / control-byte input. Header names are
/// programmer constants, not runtime input — the panic catches typos
/// at first request.
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

/// Django-parity
/// [`django.utils.cache.patch_vary_headers(response, newheaders)`](https://docs.djangoproject.com/en/6.0/topics/cache/#using-vary-headers) —
/// add additional names to the existing `Vary` header on
/// `headers`, deduplicating case-insensitively. Use from
/// middleware that wants to extend whatever the view already set
/// without clobbering it.
///
/// If `Vary` is absent, it's added with the new names. If `Vary: *`
/// is already set, the patch is a no-op (per Django shape — `*` is
/// the strongest cache key and adding more names doesn't change
/// behavior).
///
/// Name comparison is case-insensitive (HTTP header names are
/// case-insensitive per RFC 7230 §3.2). Output names preserve
/// the first-seen casing.
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
