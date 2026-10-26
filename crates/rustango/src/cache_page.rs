//! Per-view caching: cache a whole successful response, plus
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
//! 5. [`CachePageLayer::invalidate`]: purge a page's cached entries.
//!    Never rebuild the key by hand.
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
//!   `Authorization`, `Cookie` or `X-Api-Key` is neither served from nor stored in
//!   the cache, since its response probably depends on the caller.
//!   Only [`CachePageLayer::cache_authenticated`] changes that, and
//!   only for a route you know is public.
//! - **The response's own `Vary` is honoured.** A response whose
//!   `Vary` is `*` or names a request header outside the key (such as
//!   `Accept-Encoding` from compression) is not cached; add that header
//!   with [`CachePageLayer::vary_on`] to cache it per value.
//! - **CSP nonces are never replayed.** Inside the CSP nonce layer, a
//!   hit gets the current request's nonce in place of the stored one.
//!   A response whose CSP already names a nonce the layer did not see
//!   (the layer sits outside the nonce layer) is not cached.
//! - **The body is buffered.** A response loses its streaming
//!   behaviour under this layer. Use [`never_cache`] on streaming
//!   handlers, or leave the layer off those routes.
//! - **Vary-on values are case-insensitive** and a missing header
//!   counts as empty.
//! - **Keyed on the resolved tenant.** With the `tenancy` feature the
//!   layer resolves the request's tenant and puts it in the key, so a
//!   tenant picked by a header never gets another tenant's page. Mount
//!   the layer inside the tenancy layer (any router passed to the
//!   server builder is). Where no tenant context is present, or no
//!   tenant resolves, it does not cache, unless
//!   [`CachePageLayer::tenant_agnostic`] is set. A
//!   CDN in front must still vary on the tenant header itself.
//!
//! [`CachePageLayer`]: crate::cache_page::CachePageLayer
//! [`CachePageLayer::cache_query`]: crate::cache_page::CachePageLayer::cache_query
//! [`CachePageLayer::cache_authenticated`]: crate::cache_page::CachePageLayer::cache_authenticated
//! [`CachePageLayer::tenant_agnostic`]: crate::cache_page::CachePageLayer::tenant_agnostic
//! [`CachePageLayer::vary_on`]: crate::cache_page::CachePageLayer::vary_on
//! [`CachePageLayer::invalidate`]: crate::cache_page::CachePageLayer::invalidate
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

use crate::body_limit::collect_capped;
use crate::cache::{BoxedCache, CacheError};

// ---------------------------------------------------------------- Wire format

/// A stored page, written with [`Cache::set_bytes`](crate::cache::Cache::set_bytes)
/// as one binary frame: no JSON or base64 on a hit.
///
/// Frame: `[FRAME_V1][status u16][count u32]`, then per header
/// `[name_len u32][name][value_len u32][value]`, then the raw body.
/// All integers big-endian.
#[derive(Debug)]
struct CachedPage {
    status: StatusCode,
    headers: HeaderMap,
    body: bytes::Bytes,
}

/// The frame version. The JSON entries older releases wrote start with
/// `{`, so they decode as an unknown version: a miss, then overwritten.
const FRAME_V1: u8 = 1;

/// Why a stored frame was refused; the layer treats each as a miss.
#[derive(Debug, PartialEq, Eq)]
enum FrameError {
    UnknownVersion,
    Truncated,
    BadStatus,
    BadHeader,
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
    /// When `false` (the default), a request with `Authorization`,
    /// `Cookie` or `X-Api-Key` is neither served from nor stored in the shared
    /// cache, because its response is probably per-user. See
    /// [`Self::cache_authenticated`].
    cache_authenticated: bool,
    /// Request headers that make a request per-user. See [`Self::credential_headers`].
    credential_headers: Vec<HeaderName>,
    /// Skip tenant resolution. See [`Self::tenant_agnostic`].
    tenant_agnostic: bool,
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
            credential_headers: crate::vary::CREDENTIAL_HEADERS.to_vec(),
            tenant_agnostic: false,
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
    /// lowercased. Calling this again appends. A response that `Vary`s
    /// on a header not listed here is not cached.
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

    /// Allow caching responses to requests that carry `Authorization`,
    /// `Cookie` or `X-Api-Key`.
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

    /// More request headers that carry a credential, such as a custom
    /// API token header. Like `Authorization`, `Cookie` and `X-Api-Key`,
    /// a request with one skips the cache unless [`Self::cache_authenticated`].
    ///
    /// # Panics
    /// Panics on a name that is not a valid header name.
    #[must_use]
    pub fn credential_headers<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for n in names {
            let lower = n.as_ref().to_ascii_lowercase();
            let h = HeaderName::from_bytes(lower.as_bytes())
                .expect("credential_headers: header name must be valid ASCII");
            self.credential_headers.push(h);
        }
        self
    }

    /// Cache without resolving a tenant. Default `false`.
    ///
    /// Only for a route whose response is the same for every tenant,
    /// or for a layer mounted outside any tenancy layer on purpose.
    /// `Host` stays in the key. Without the `tenancy` feature this
    /// changes nothing.
    #[must_use]
    pub fn tenant_agnostic(mut self, enabled: bool) -> Self {
        self.tenant_agnostic = enabled;
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

    /// The key this layer writes for a `GET` of `page`, built by the
    /// layer's own key function. The tenant is ignored when the layer
    /// does not key on tenants.
    #[must_use]
    pub(crate) fn key_for(&self, page: &PageKey) -> String {
        let tenant = if tenant_keyed(self.tenant_agnostic) {
            page.tenant.as_deref()
        } else {
            None
        };
        build_key(
            &self.key_prefix,
            "GET",
            &page.path,
            &page.query,
            &page.host,
            tenant,
            self.vary_on
                .iter()
                .map(|name| (name.as_str(), page.vary_value(name))),
            None,
        )
    }

    /// Delete the cached `GET` response of every page given, from the
    /// layer's own cache. Pass one [`PageKey`] per `vary_on` value set.
    ///
    /// Only the exact query string and vary values given are purged: the
    /// key holds them raw, not sorted or decoded. Other variants (say
    /// `?utm_source=x`) stay until the layer's timeout.
    ///
    /// # Errors
    /// The cache backend's delete error.
    pub async fn invalidate<'a, I>(&self, pages: I) -> Result<(), CacheError>
    where
        I: IntoIterator<Item = &'a PageKey>,
    {
        let keyed = tenant_keyed(self.tenant_agnostic);
        let keys: Vec<String> = pages
            .into_iter()
            .inspect(|p| {
                if keyed && p.tenant.is_none() {
                    // Such a page is never cached, so this deletes nothing.
                    tracing::warn!(
                        target: "rustango::cache_page",
                        path = %p.path,
                        "invalidate: page has no tenant but the layer keys on it"
                    );
                }
            })
            .map(|p| self.key_for(p))
            .collect();
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        self.cache.delete_many(&refs).await
    }
}

/// A cached page, for [`CachePageLayer::invalidate`].
///
/// Give path, query and host exactly as the request sends them: path
/// percent-encoded (`/caf%C3%A9`) and in full, any `Router::nest` prefix
/// included, host with the same case and port. A `vary_on` header not set here
/// counts as missing, as on a request.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PageKey {
    path: String,
    query: String,
    host: String,
    tenant: Option<String>,
    vary: Vec<(String, String)>,
}

impl PageKey {
    /// The page at `path` (no query string) served for `host`, as the
    /// request's `Host` header carries it, port included.
    #[must_use]
    pub fn new(path: impl Into<String>, host: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            query: String::new(),
            host: host.into(),
            tenant: None,
            vary: Vec::new(),
        }
    }

    /// The raw query string, without the `?`.
    #[must_use]
    pub fn query(mut self, query: impl Into<String>) -> Self {
        self.query = query.into();
        self
    }

    /// The tenant slug the page was resolved for.
    #[must_use]
    pub fn tenant(mut self, slug: impl Into<String>) -> Self {
        self.tenant = Some(slug.into());
        self
    }

    /// The request's value of a `vary_on` header. Names are case-insensitive.
    #[must_use]
    pub fn vary(mut self, name: impl AsRef<str>, value: impl Into<String>) -> Self {
        self.vary
            .push((name.as_ref().to_ascii_lowercase(), value.into()));
        self
    }

    fn vary_value(&self, name: &HeaderName) -> &str {
        self.vary
            .iter()
            .rev()
            .find(|(n, _)| n == name.as_str())
            .map_or("", |(_, v)| v.as_str())
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
            credentials: Arc::new(if self.cache_authenticated {
                Vec::new()
            } else {
                self.credential_headers.clone()
            }),
            tenant_agnostic: self.tenant_agnostic,
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
    /// Credential headers that bypass the cache; empty with `cache_authenticated`.
    credentials: Arc<Vec<HeaderName>>,
    tenant_agnostic: bool,
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
        let credentials = self.credentials.clone();
        let tenant_agnostic = self.tenant_agnostic;
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

            // Unless the route opted in, a request with a credential
            // header is per-user: never served from and never stored
            // in the shared cache.
            let req_is_authed = credentials.iter().any(|n| req.headers().contains_key(n));
            if req_is_authed {
                return inner.call(req).await;
            }

            let (req, tenant) = if tenant_keyed(tenant_agnostic) {
                request_tenant(req).await
            } else {
                (req, PageTenant::NoTenant)
            };
            let tenant = match tenant {
                PageTenant::Unknown => return inner.call(req).await,
                PageTenant::Slug(slug) => Some(slug),
                PageTenant::NoTenant => None,
            };
            let key = compute_cache_key(
                &prefix,
                &req,
                tenant.as_deref(),
                &vary,
                body_digest.as_deref(),
            );
            let nonce = req.extensions().get::<crate::csp_nonce::Nonce>().cloned();
            let nonce = nonce.as_ref().map(crate::csp_nonce::Nonce::value);

            // Cache hit?
            if let Ok(Some(frame)) = cache.get_bytes(&key).await {
                if let Some(stored) = CachedPage::decode(frame)
                    .ok()
                    .and_then(|p| p.with_nonce(nonce))
                {
                    let mut resp = stored.into_response(&vary);
                    if is_query {
                        crate::vary::mark_private(resp.headers_mut());
                    }
                    return Ok(resp);
                }
                // Corrupt or older-format entry: fall through and recompute.
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
            if let Some(token) = unkeyed_vary(resp.headers(), &vary, &credentials) {
                warn_unkeyed_vary(&token);
                let mut resp = resp;
                resp.headers_mut()
                    .insert(X_CACHE_STATUS, HeaderValue::from_static("BYPASS"));
                return Ok(resp);
            }

            // Buffer the body so it can be stored and replayed.
            let (parts, body) = resp.into_parts();
            let bytes = match collect_capped(body, MAX_CACHEABLE_BODY_BYTES).await {
                Ok(Ok(b)) => b,
                Ok(Err(whole)) => {
                    // Too large to store: send it all, uncached (#2218).
                    let mut resp = Response::from_parts(parts, whole);
                    resp.headers_mut()
                        .insert(X_CACHE_STATUS, HeaderValue::from_static("BYPASS"));
                    return Ok(resp);
                }
                Err(e) => {
                    // The stream broke; a 500 beats a truncated 200.
                    tracing::warn!(
                        target: "rustango::cache_page",
                        error = %e,
                        "response body failed to buffer"
                    );
                    let mut resp = Response::new(Body::empty());
                    *resp.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                    return Ok(resp);
                }
            };

            let stored = CachedPage {
                status: parts.status,
                headers: parts.headers.clone(),
                body: bytes.clone(),
            }
            .without_nonce(nonce);
            let Some(stored) = stored else {
                let mut resp = Response::from_parts(parts, Body::from(bytes));
                resp.headers_mut()
                    .insert(X_CACHE_STATUS, HeaderValue::from_static("BYPASS"));
                return Ok(resp);
            };
            if let Some(frame) = stored.encode() {
                if let Err(e) = cache.set_bytes(&key, &frame, Some(timeout)).await {
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
                crate::vary::mark_private(headers);
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

/// Stands in for the request's CSP nonce in a stored page. Random per
/// process, so a page cannot carry it on purpose.
fn cached_nonce() -> &'static str {
    use rand::RngCore as _;
    static TOKEN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TOKEN.get_or_init(|| {
        let mut raw = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut raw);
        let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        format!("__RUSTANGO_CACHED_NONCE_{hex}__")
    })
}

/// Marks a stored page holding a [`cached_nonce`] token, and names it, so
/// another process sharing the cache swaps the right one. Never sent.
const NONCE_MARK: HeaderName = HeaderName::from_static("x-rustango-cached-nonce");

/// `hay` with every `from` replaced by `to`; `None` when there is none.
fn replace_bytes(hay: &[u8], from: &[u8], to: &[u8]) -> Option<Vec<u8>> {
    if from.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    let (mut i, mut copied) = (0, 0);
    while i + from.len() <= hay.len() {
        if hay[i..].starts_with(from) {
            out.extend_from_slice(&hay[copied..i]);
            out.extend_from_slice(to);
            i += from.len();
            copied = i;
        } else {
            i += 1;
        }
    }
    if copied == 0 {
        return None;
    }
    out.extend_from_slice(&hay[copied..]);
    Some(out)
}

/// The tenant a page is cached for.
#[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
enum PageTenant {
    /// No tenant: tenancy is off, or the route is tenant-agnostic.
    NoTenant,
    Slug(String),
    /// No tenant context, no match, or resolution failed: never share a page.
    Unknown,
}

/// The request's tenant: a `TenantSlug` already set, else the mounted
/// tenant context's resolver.
#[cfg(feature = "tenancy")]
async fn request_tenant(req: Request<Body>) -> (Request<Body>, PageTenant) {
    if let Some(crate::tenancy::TenantSlug(slug)) = req.extensions().get() {
        let slug = slug.clone();
        return (req, PageTenant::Slug(slug));
    }
    let (parts, body) = req.into_parts();
    let tenant = match crate::tenancy::middleware::request_org(&parts, &parts.extensions).await {
        Some(Ok(Some(org))) => PageTenant::Slug(org.slug),
        // The route may still vary on an input the resolver ignored (#2045).
        Some(Ok(None)) => PageTenant::Unknown,
        Some(Err(e)) => {
            tracing::warn!(target: "rustango::cache_page", error = %e, "tenant resolution failed; not caching");
            PageTenant::Unknown
        }
        None => {
            warn_no_tenant_context();
            PageTenant::Unknown
        }
    };
    (Request::from_parts(parts, body), tenant)
}

#[cfg(not(feature = "tenancy"))]
#[allow(clippy::unused_async)]
async fn request_tenant(req: Request<Body>) -> (Request<Body>, PageTenant) {
    (req, PageTenant::NoTenant)
}

/// Log once per process: a misplaced layer would otherwise just stop caching.
#[cfg(feature = "tenancy")]
fn warn_no_tenant_context() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        tracing::warn!(
            target: "rustango::cache_page",
            "no tenant context on the request; page cache bypassed. Mount \
             CachePageLayer inside the tenancy layer, or set tenant_agnostic(true)"
        );
    });
}

/// Whether keys carry the resolved tenant, as the layer decides it.
fn tenant_keyed(tenant_agnostic: bool) -> bool {
    cfg!(feature = "tenancy") && !tenant_agnostic
}

/// The key of the request the layer is serving.
fn compute_cache_key(
    prefix: &str,
    req: &Request<Body>,
    tenant: Option<&str>,
    vary_on: &[HeaderName],
    body_digest: Option<&str>,
) -> String {
    // HTTP/2 sends `:authority`, not `Host`.
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .or_else(|| {
            req.uri()
                .authority()
                .map(axum::http::uri::Authority::as_str)
        })
        .unwrap_or("");
    // A nested router strips its prefix from `uri`; key on the full path.
    let uri = req
        .extensions()
        .get::<axum::extract::OriginalUri>()
        .map_or(req.uri(), |o| &o.0);
    build_key(
        prefix,
        req.method().as_str(),
        uri.path(),
        uri.query().unwrap_or(""),
        host,
        tenant,
        vary_on.iter().map(|name| {
            let v = req
                .headers()
                .get(name)
                .and_then(|h| h.to_str().ok())
                .unwrap_or("");
            (name.as_str(), v)
        }),
        body_digest,
    )
}

/// The only place the key format lives, as `prefix|<len>:<bytes>|...`.
///
/// Each part is length-prefixed, so a value holding a separator
/// cannot make two different requests produce the same key.
///
/// `Host` and the resolved tenant slug are always part of the key, so
/// neither a per-host nor a per-header tenant gets another's page.
#[allow(clippy::too_many_arguments)]
fn build_key<'a>(
    prefix: &str,
    method: &str,
    path: &str,
    query: &str,
    host: &str,
    tenant: Option<&str>,
    vary: impl Iterator<Item = (&'a str, &'a str)>,
    body_digest: Option<&str>,
) -> String {
    use std::fmt::Write as _;
    let mut k = String::with_capacity(prefix.len() + 128);
    let _ = write!(&mut k, "{prefix}|");
    write_lp(&mut k, method);
    write_lp(&mut k, path);
    write_lp(&mut k, query);
    write_lp(&mut k, host);
    // Slugs are never empty, so "" means "tenant-agnostic".
    write_lp(&mut k, tenant.unwrap_or(""));
    for (name, v) in vary {
        write_lp(&mut k, name);
        write_lp(&mut k, v);
    }
    // QUERY carries its criteria in the body, so the digest joins the
    // key. GET passes `None`, so its keys are unchanged.
    if let Some(digest) = body_digest {
        write_lp(&mut k, digest);
    }
    k
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

/// The first `Vary` token outside the key, if any (#2219).
/// `credentials` count: such requests skip this cache, so every entry
/// was made without them.
fn unkeyed_vary(
    headers: &HeaderMap,
    vary_on: &[HeaderName],
    credentials: &[HeaderName],
) -> Option<String> {
    headers
        .get_all(axum::http::header::VARY)
        .iter()
        .flat_map(|v| v.to_str().unwrap_or("*").split(','))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .find(|t| {
            !(t.eq_ignore_ascii_case("host")
                || vary_on.iter().any(|n| t.eq_ignore_ascii_case(n.as_str()))
                || credentials
                    .iter()
                    .any(|n| t.eq_ignore_ascii_case(n.as_str())))
        })
        .map(str::to_owned)
}

/// Log once per process: an unkeyed `Vary` otherwise just stops caching.
fn warn_unkeyed_vary(token: &str) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        tracing::warn!(
            target: "rustango::cache_page",
            vary = token,
            "response varies on a header outside the cache key; not cached. \
             Add it with CachePageLayer::vary_on to cache per value"
        );
    });
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

impl CachedPage {
    /// The frame for this page; `None` if a length overflows `u32`.
    ///
    /// Every header value is kept, duplicates and non-UTF-8 included,
    /// except `x-cache-status`, which is set fresh on every hit.
    fn encode(&self) -> Option<Vec<u8>> {
        let kept = || self.headers.iter().filter(|(n, _)| *n != X_CACHE_STATUS);
        let header_bytes: usize = kept().map(|(n, v)| 8 + n.as_str().len() + v.len()).sum();
        let mut out = Vec::with_capacity(7 + header_bytes + self.body.len());
        out.push(FRAME_V1);
        out.extend_from_slice(&self.status.as_u16().to_be_bytes());
        out.extend_from_slice(&u32::try_from(kept().count()).ok()?.to_be_bytes());
        for (name, value) in kept() {
            for part in [name.as_str().as_bytes(), value.as_bytes()] {
                out.extend_from_slice(&u32::try_from(part.len()).ok()?.to_be_bytes());
                out.extend_from_slice(part);
            }
        }
        out.extend_from_slice(&self.body);
        Some(out)
    }

    /// Parse a frame. Header values and the body share `frame`'s buffer.
    fn decode(mut frame: bytes::Bytes) -> Result<Self, FrameError> {
        use bytes::Buf as _;
        fn need(frame: &bytes::Bytes, n: usize) -> Result<(), FrameError> {
            (frame.len() >= n)
                .then_some(())
                .ok_or(FrameError::Truncated)
        }
        fn len(frame: &mut bytes::Bytes) -> Result<usize, FrameError> {
            need(frame, 4)?;
            usize::try_from(frame.get_u32()).map_err(|_| FrameError::Truncated)
        }
        fn part(frame: &mut bytes::Bytes) -> Result<bytes::Bytes, FrameError> {
            let n = len(frame)?;
            need(frame, n)?;
            Ok(frame.split_to(n))
        }

        need(&frame, 1)?;
        if frame.get_u8() != FRAME_V1 {
            return Err(FrameError::UnknownVersion);
        }
        need(&frame, 2)?;
        let status = StatusCode::from_u16(frame.get_u16()).map_err(|_| FrameError::BadStatus)?;
        let count = len(&mut frame)?;
        let mut headers = HeaderMap::with_capacity(count.min(64));
        for _ in 0..count {
            let name =
                HeaderName::from_bytes(&part(&mut frame)?).map_err(|_| FrameError::BadHeader)?;
            let value = HeaderValue::from_maybe_shared(part(&mut frame)?)
                .map_err(|_| FrameError::BadHeader)?;
            // `append`, not `insert`, so duplicates are preserved.
            headers
                .try_append(name, value)
                .map_err(|_| FrameError::BadHeader)?;
        }
        Ok(Self {
            status,
            headers,
            body: frame,
        })
    }

    /// Whether the body or a header value contains `needle`.
    fn holds(&self, needle: &str) -> bool {
        let n = needle.as_bytes();
        let has = |hay: &[u8]| hay.windows(n.len()).any(|w| w == n);
        has(&self.body) || self.headers.values().any(|v| has(v.as_bytes()))
    }

    /// Swap every `from` for `to`, in the body and each header value.
    /// `true` when something changed.
    fn swap(&mut self, from: &str, to: &str) -> bool {
        let mut changed = false;
        if let Some(body) = replace_bytes(&self.body, from.as_bytes(), to.as_bytes()) {
            self.body = body.into();
            // The length changed; hyper sets it from the body.
            self.headers.remove(axum::http::header::CONTENT_LENGTH);
            changed = true;
        }
        for value in self.headers.values_mut() {
            if let Some(v) = replace_bytes(value.as_bytes(), from.as_bytes(), to.as_bytes()) {
                if let Ok(v) = HeaderValue::from_bytes(&v) {
                    *value = v;
                    changed = true;
                }
            }
        }
        changed
    }

    /// The page to store: the request's nonce (if any) swapped out.
    /// `None` when its CSP names a nonce this layer did not mint into it.
    fn without_nonce(mut self, nonce: Option<&str>) -> Option<Self> {
        let token = cached_nonce();
        // Already there: a hit would hand it this visitor's nonce.
        if self.holds(token) {
            return None;
        }
        if let Some(n) = nonce {
            if self.swap(n, token) {
                self.headers
                    .insert(NONCE_MARK, HeaderValue::from_str(token).ok()?);
            }
        }
        let ours = format!("'nonce-{token}'");
        let foreign = [
            "content-security-policy",
            "content-security-policy-report-only",
        ]
        .iter()
        .flat_map(|name| self.headers.get_all(*name))
        .any(|v| {
            String::from_utf8_lossy(v.as_bytes())
                .replace(crate::csp_nonce::CSP_NONCE_PLACEHOLDER, "")
                .replace(&ours, "")
                .to_ascii_lowercase()
                .contains("'nonce-")
        });
        (!foreign).then_some(self)
    }

    /// A stored page for this request: its nonce swapped back in.
    /// `None` (a miss) when the page needs a nonce the request lacks.
    fn with_nonce(mut self, nonce: Option<&str>) -> Option<Self> {
        if let Some(token) = self.headers.remove(NONCE_MARK) {
            self.swap(token.to_str().ok()?, nonce?);
        }
        Some(self)
    }

    /// The response for a hit. `vary_on` comes from the live layer
    /// config, so a changed vary list takes effect on the next hit.
    fn into_response(self, vary_on: &[HeaderName]) -> Response<Body> {
        let mut resp = Response::new(Body::from(self.body));
        *resp.status_mut() = self.status;
        *resp.headers_mut() = self.headers;
        let headers = resp.headers_mut();
        headers.insert(X_CACHE_STATUS, HeaderValue::from_static("HIT"));
        apply_vary_header(headers, vary_on);
        resp
    }
}

// ---------------------------------------------------------------- Cache-Control builder

/// Builder for the `Cache-Control` response header.
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
/// the view set.
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

    fn sample_page() -> CachedPage {
        let mut headers = HeaderMap::new();
        for i in 0..40 {
            headers.append(
                HeaderName::from_bytes(format!("x-h{i}").as_bytes()).unwrap(),
                HeaderValue::from_str(&format!("v{i}")).unwrap(),
            );
        }
        headers.append("set-thing", HeaderValue::from_static("a"));
        headers.append("set-thing", HeaderValue::from_static("b"));
        headers.insert("x-latin1", HeaderValue::from_bytes(b"caf\xe9").unwrap());
        headers.insert(X_CACHE_STATUS, HeaderValue::from_static("MISS"));
        CachedPage {
            status: StatusCode::NON_AUTHORITATIVE_INFORMATION,
            headers,
            body: bytes::Bytes::from_static(b"\xff\x00\xfe binary \xc3"),
        }
    }

    /// A page already holding the stand-in token is not stored, so the
    /// token in it cannot turn into a later visitor's nonce.
    #[test]
    fn a_page_holding_the_token_is_not_stored() {
        let token = cached_nonce();
        let mut in_body = sample_page();
        in_body.body = format!("<script nonce=\"{token}\">").into();
        assert!(in_body.without_nonce(Some("abc")).is_none());
        let mut in_header = sample_page();
        in_header
            .headers
            .insert("link", HeaderValue::from_str(token).unwrap());
        assert!(in_header.without_nonce(Some("abc")).is_none());
        assert!(sample_page().without_nonce(Some("abc")).is_some());
    }

    /// The token is not a constant anyone can write into a page.
    #[test]
    fn the_token_is_random() {
        let token = cached_nonce();
        let hex = token
            .strip_prefix("__RUSTANGO_CACHED_NONCE_")
            .and_then(|t| t.strip_suffix("__"))
            .unwrap();
        assert_eq!(hex.len(), 32);
        assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    /// The frame round-trips status, every header (duplicates and
    /// non-UTF-8 too) and a non-UTF-8 body, minus `x-cache-status`.
    #[test]
    fn a_page_frame_round_trips() {
        let page = sample_page();
        let back = CachedPage::decode(page.encode().unwrap().into()).unwrap();
        assert_eq!(back.status, page.status);
        assert_eq!(back.body, page.body);
        assert!(back.headers.get(X_CACHE_STATUS).is_none());
        let mut want = page.headers.clone();
        want.remove(X_CACHE_STATUS);
        assert_eq!(back.headers, want);
        let dup: Vec<_> = back.headers.get_all("set-thing").iter().collect();
        assert_eq!(dup, ["a", "b"]);
    }

    /// Unknown versions, the old JSON format and cut-off frames are refused.
    #[test]
    fn a_bad_frame_is_refused() {
        let frame = sample_page().encode().unwrap();
        let mut v2 = frame.clone();
        v2[0] = 2;
        assert_eq!(
            CachedPage::decode(v2.into()).unwrap_err(),
            FrameError::UnknownVersion
        );
        let json = br#"{"status":200,"headers":[],"body_b64":"aGk="}"#;
        assert_eq!(
            CachedPage::decode(bytes::Bytes::from_static(json)).unwrap_err(),
            FrameError::UnknownVersion
        );
        // Cut inside the header block; past it, the cut only shortens the body.
        for cut in [0, 1, 2, 5, 9, 20] {
            assert_eq!(
                CachedPage::decode(bytes::Bytes::copy_from_slice(&frame[..cut])).unwrap_err(),
                FrameError::Truncated,
                "cut at {cut}"
            );
        }
        let mut bad_status = frame;
        bad_status[1..3].copy_from_slice(&1000u16.to_be_bytes());
        assert_eq!(
            CachedPage::decode(bad_status.into()).unwrap_err(),
            FrameError::BadStatus
        );
    }

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

    // -------- patch_vary_headers --------

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
