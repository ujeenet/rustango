//! HMAC-signed request authentication for service-to-service traffic.
//!
//! AWS-style: each request carries `X-Date` and an `Authorization`
//! header containing a key id + HMAC-SHA256 over the canonical
//! request. The shared key is never transmitted, and replay attacks
//! are bounded by a configurable `X-Date` tolerance window.
//!
//! With [`crate::api_keys`] the key itself travels on every call.
//! Pick HMAC when callers can sign and you do not trust the channel.
//! Pick bearer tokens when TLS is enough and clients must stay simple.
//!
//! ## Wire format
//!
//! ```text
//! POST /webhooks/incoming HTTP/1.1
//! X-Date: 2026-05-02T12:34:56Z
//! Authorization: HMAC-SHA256 keyId=k_abc,signature=<base64>
//! ```
//!
//! ## Canonical request (what gets signed)
//!
//! ```text
//! <UPPER-METHOD>\n
//! <LOWERCASE-HOST>\n
//! <PATH>\n
//! <SORTED-QUERY-STRING>\n
//! <X-DATE>\n
//! <HEX-SHA256(BODY)>
//! ```
//!
//! The host is the `Host` header (or the HTTP/2 authority) without the
//! port. Unpinned, a replay to another service sharing the key passes
//! if it keeps the first `Host`; shared keys need [`HmacAuthLayer::host`].
//! [`HmacAuthLayer::signed_headers`] and [`HmacAuthLayer::sign_port`] append
//! one `name:value` line each, sorted; sign those with [`RequestToSign`].
//! The tenant header is not signed unless listed (#2492).
//! The query is sorted, so `?b=2&a=1` and `?a=1&b=2` sign the same.
//! The body is hashed first, so the verifier hashes it only once.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::hmac_auth::{HmacAuthLayer, KeyResolver};
//! use tower::ServiceBuilder;
//! use std::sync::Arc;
//!
//! // Lookup function: key id -> Some(secret) or None for unknown.
//! let resolver: KeyResolver = Arc::new(|key_id: &str| {
//!     if key_id == "k_abc" { Some(b"shared-secret-bytes".to_vec()) } else { None }
//! });
//!
//! let inner = axum::Router::new().route("/webhooks/incoming", post(handle));
//! let app = ServiceBuilder::new()
//!     .layer(HmacAuthLayer::new(resolver))
//!     .service(inner);
//! ```
//!
//! ## Signing on the client side
//!
//! Use [`sign_request`] to build the `Authorization` header value
//! that this layer will accept.
//!
//! [`sign_request`]: crate::hmac_auth::sign_request

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{to_bytes, Body};
use axum::http::{Request, Response, StatusCode};
use axum::response::IntoResponse as _;
use base64::Engine;

use crate::api_errors::ApiError;
use subtle::ConstantTimeEq;
use tower::Service;

const HEADER_DATE: &str = "x-date";
const HEADER_AUTH: &str = "authorization";
const SCHEME: &str = "HMAC-SHA256";
const DEFAULT_TOLERANCE_SECS: u64 = 300; // 5 min — RFC convention
const DEFAULT_BODY_LIMIT: usize = 10 * 1024 * 1024;

/// Maps a `key_id` to its secret, usually from a database or cache.
/// Return `None` to reject the request with 401.
pub type KeyResolver = Arc<dyn Fn(&str) -> Option<Vec<u8>> + Send + Sync>;

/// Maps a `key_id` to the tenant slug it is bound to; `None` = any tenant.
pub type KeyTenantResolver = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Pseudo-header name of the signed port; no real header can take it.
const PORT_LINE: &str = ":port";

#[derive(Clone)]
pub struct HmacAuthLayer {
    inner: Arc<HmacAuthConfig>,
}

#[derive(Clone)]
struct HmacAuthConfig {
    resolver: KeyResolver,
    tolerance_secs: u64,
    body_limit: usize,
    /// Signed host when set; else the request's own.
    host: Option<SignedHost>,
    /// Extra headers in the signature, lowercase.
    signed_headers: Vec<axum::http::HeaderName>,
    /// Sign the port the request came in on.
    sign_port: bool,
    /// Header naming the tenant, and the slug each key is bound to.
    tenant: Option<(axum::http::HeaderName, KeyTenantResolver)>,
    /// Optional replay defence. When set, each valid signature is
    /// stored for twice the tolerance and a repeat is rejected. The
    /// `X-Date` window alone only limits how long a replay works;
    /// this stops it. Opt-in, because it needs a shared cache.
    #[cfg(feature = "cache")]
    nonce_store: Option<Arc<dyn crate::cache::Cache>>,
}

impl HmacAuthLayer {
    #[must_use]
    pub fn new(resolver: KeyResolver) -> Self {
        Self {
            inner: Arc::new(HmacAuthConfig {
                resolver,
                tolerance_secs: DEFAULT_TOLERANCE_SECS,
                body_limit: DEFAULT_BODY_LIMIT,
                host: None,
                signed_headers: Vec::new(),
                sign_port: false,
                tenant: None,
                #[cfg(feature = "cache")]
                nonce_store: None,
            }),
        }
    }

    /// Override the `X-Date` tolerance window (default 300 s = ±5 min).
    #[must_use]
    pub fn tolerance_secs(mut self, secs: u64) -> Self {
        Arc::make_mut(&mut self.inner).tolerance_secs = secs;
        self
    }

    /// Turn on replay protection backed by a [`crate::cache::Cache`].
    /// A verified signature is stored for `2 × tolerance_secs`, and the
    /// same signature again in that window gets a 401.
    ///
    /// **Use a shared backend such as Redis.** An in-process cache
    /// only protects one replica, so a replay sent to another replica
    /// still works. Without this, a captured request can be replayed
    /// until the `X-Date` window closes. A `NullCache` keeps nothing,
    /// so it is accepted with a warning.
    #[cfg(feature = "cache")]
    #[must_use]
    pub fn nonce_store(mut self, store: Arc<dyn crate::cache::Cache>) -> Self {
        if store.stores_nothing() {
            tracing::warn!(
                target: "rustango::hmac_auth",
                "HMAC nonce store keeps nothing (`NullCache`), so replay protection is off; \
                 use a Redis or database cache"
            );
        }
        Arc::make_mut(&mut self.inner).nonce_store = Some(store);
        self
    }

    /// Verify against this host instead of the request's `Host`. Needed when
    /// services share a key, or behind a proxy that rewrites `Host`.
    ///
    /// # Panics
    /// If `host` is empty or not a valid host name.
    #[must_use]
    pub fn host(mut self, host: &str) -> Self {
        let pinned = SignedHost::parse(host)
            .unwrap_or_else(|| panic!("HmacAuthLayer::host: invalid host {host:?}"));
        Arc::make_mut(&mut self.inner).host = Some(pinned);
        self
    }

    /// Sign these headers too, such as the tenant header (#2492). A missing
    /// one signs as empty, so the client signs every listed header.
    ///
    /// # Panics
    /// If a name is not a valid header name.
    #[must_use]
    pub fn signed_headers<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let cfg = Arc::make_mut(&mut self.inner);
        for name in names {
            let name = axum::http::HeaderName::try_from(name.as_ref())
                .unwrap_or_else(|_| panic!("HmacAuthLayer: invalid header {:?}", name.as_ref()));
            if !cfg.signed_headers.contains(&name) {
                cfg.signed_headers.push(name);
            }
        }
        self
    }

    /// Sign the port: the listener's (`tenancy::ListenerPort`), else the one
    /// in `Host`. Stops a replay to another port-resolved tenant (#2492).
    #[must_use]
    pub fn sign_port(mut self) -> Self {
        Arc::make_mut(&mut self.inner).sign_port = true;
        self
    }

    /// Bind keys to tenants: a key `tenant_of` maps to a slug is refused
    /// (403) unless `header` carries that slug. The header is signed.
    #[must_use]
    pub fn tenant_header(self, header: &str, tenant_of: KeyTenantResolver) -> Self {
        let mut this = self.signed_headers([header]);
        let name = axum::http::HeaderName::try_from(header).expect("checked above");
        Arc::make_mut(&mut this.inner).tenant = Some((name, tenant_of));
        this
    }

    /// Largest body this will buffer for hashing. A bigger request
    /// gets a 413. Default 10 MiB.
    #[must_use]
    pub fn body_limit(mut self, n: usize) -> Self {
        Arc::make_mut(&mut self.inner).body_limit = n;
        self
    }
}

impl<S> tower::Layer<S> for HmacAuthLayer {
    type Service = HmacAuthService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        HmacAuthService {
            inner,
            cfg: Arc::clone(&self.inner),
        }
    }
}

#[derive(Clone)]
pub struct HmacAuthService<S> {
    inner: S,
    cfg: Arc<HmacAuthConfig>,
}

impl<S> Service<Request<Body>> for HmacAuthService<S>
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
        Pin<Box<dyn std::future::Future<Output = Result<Response<Body>, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let cfg = Arc::clone(&self.cfg);
        let mut inner = self.inner.clone();
        Box::pin(async move {
            match verify_request(&cfg, req).await {
                Ok(req) => inner.call(req).await,
                Err(resp) => Ok(resp),
            }
        })
    }
}

async fn verify_request(
    cfg: &HmacAuthConfig,
    req: Request<Body>,
) -> Result<Request<Body>, Response<Body>> {
    // Pull the headers up front (before we move the body).
    let date = match req.headers().get(HEADER_DATE).and_then(|v| v.to_str().ok()) {
        Some(s) => s.to_owned(),
        None => return Err(deny("missing X-Date")),
    };
    if !date_within_tolerance(&date, cfg.tolerance_secs) {
        return Err(deny("X-Date outside tolerance window"));
    }
    let auth = match req.headers().get(HEADER_AUTH).and_then(|v| v.to_str().ok()) {
        Some(s) => s.to_owned(),
        None => return Err(deny("missing Authorization")),
    };
    let parsed = match parse_auth(&auth) {
        Some(p) => p,
        None => return Err(deny("malformed Authorization")),
    };
    let secret = match (cfg.resolver)(&parsed.key_id) {
        Some(s) => s,
        None => return Err(deny("unknown key id")),
    };

    let host = match &cfg.host {
        Some(h) => h.clone(),
        None => match request_host(&req) {
            Ok(h) => h,
            Err(msg) => return Err(deny(msg)),
        },
    };
    let mut extras = SignedExtras::default();
    for name in &cfg.signed_headers {
        let value = match req.headers().get(name).map(|v| v.to_str()) {
            Some(Ok(v)) => v,
            Some(Err(_)) => return Err(deny("malformed signed header")),
            None => "",
        };
        extras.insert(name.as_str(), value);
    }
    if cfg.sign_port {
        extras.insert(
            PORT_LINE,
            &request_port(&req).map_or(String::new(), |p| p.to_string()),
        );
    }
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let query = req.uri().query().unwrap_or("").to_owned();

    let (parts, body) = req.into_parts();
    let bytes = match to_bytes(body, cfg.body_limit).await {
        Ok(b) => b,
        Err(_) => return Err(too_large()),
    };
    let body_hash = sha256_hex(&bytes);

    let canonical = canonical_request(
        method.as_str(),
        Some(&host),
        &path,
        &query,
        &date,
        &body_hash,
        &extras,
    );
    let expected_sig = hmac_sha256(&secret, canonical.as_bytes());

    if expected_sig.ct_eq(&parsed.signature).unwrap_u8() == 0 {
        return Err(deny("signature mismatch"));
    }
    if let Some((header, tenant_of)) = &cfg.tenant {
        if let Some(slug) = tenant_of(&parsed.key_id) {
            if extras.get(header.as_str()) != Some(slug.as_str()) {
                return Err(ApiError::forbidden("key is bound to another tenant").into_response());
            }
        }
    }

    // Replay defence. It runs only after the signature checks out, so
    // an unauthenticated attacker cannot fill the cache. A signature
    // is unique per method, host, path, query, date and body, so a replay
    // carries the same one. `add` claims it in one step, so of two
    // simultaneous copies only one wins.
    #[cfg(feature = "cache")]
    if let Some(store) = &cfg.nonce_store {
        let nonce_key = format!(
            "hmac_nonce:{}",
            base64::engine::general_purpose::STANDARD.encode(&parsed.signature)
        );
        // X-Date is accepted ±tolerance, so a signature stays valid for
        // up to 2 × tolerance; keep the nonce that long.
        let ttl = std::time::Duration::from_secs(cfg.tolerance_secs.saturating_mul(2));
        match store.add(&nonce_key, "1", Some(ttl)).await {
            Ok(true) => {}
            Ok(false) => return Err(deny("replayed request")),
            // Auth must not depend on the cache, and X-Date still
            // bounds a replay, so pass, but say so.
            Err(e) => tracing::warn!(
                target: "rustango::hmac_auth",
                error = %e,
                "HMAC nonce store failed; replay check skipped for this request"
            ),
        }
    }

    Ok(Request::from_parts(parts, Body::from(bytes)))
}

fn deny(msg: &str) -> Response<Body> {
    ApiError::unauthorized(msg).into_response()
}

fn too_large() -> Response<Body> {
    ApiError::from_status(StatusCode::PAYLOAD_TOO_LARGE, "payload too large").into_response()
}

// =====================================================================
// Authorization parsing + canonicalization
// =====================================================================

#[derive(Debug, PartialEq)]
struct ParsedAuth {
    key_id: String,
    /// Decoded raw bytes of the signature.
    signature: Vec<u8>,
}

/// Parse `HMAC-SHA256 keyId=<x>,signature=<base64>`. Returns `None`
/// for any non-conforming header.
fn parse_auth(value: &str) -> Option<ParsedAuth> {
    let value = value.trim();
    let rest = value.strip_prefix(SCHEME)?.trim();
    let mut key_id: Option<String> = None;
    let mut signature: Option<Vec<u8>> = None;
    for pair in rest.split(',') {
        let pair = pair.trim();
        if let Some(v) = pair.strip_prefix("keyId=") {
            key_id = Some(v.trim_matches('"').to_owned());
        } else if let Some(v) = pair.strip_prefix("signature=") {
            let raw = v.trim_matches('"');
            signature = base64::engine::general_purpose::STANDARD.decode(raw).ok();
        }
    }
    Some(ParsedAuth {
        key_id: key_id?,
        signature: signature?,
    })
}

/// The host a signature covers: lowercase, no port, no trailing dot, IPv6
/// bracketed. Only built here, so signer and verifier cannot fold it differently.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SignedHost(String);

impl SignedHost {
    /// `None` for an empty or malformed host, or one with userinfo.
    fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if let Ok(ip) = raw.parse::<std::net::Ipv6Addr>() {
            return Some(Self(format!("[{ip}]")));
        }
        let authority = raw.parse::<axum::http::uri::Authority>().ok()?;
        if authority.as_str().contains('@') {
            return None;
        }
        let host = authority.host();
        if let Some(ip) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
            return ip
                .parse::<std::net::Ipv6Addr>()
                .ok()
                .map(|ip| Self(format!("[{ip}]")));
        }
        let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
        (!host.is_empty()).then_some(Self(host))
    }
}

/// The `Host` header, or the HTTP/2 `:authority` when there is none.
/// Both present and naming different hosts is refused.
fn request_host(req: &Request<Body>) -> Result<SignedHost, &'static str> {
    let header = match req.headers().get(axum::http::header::HOST) {
        Some(v) => match v.to_str().ok().and_then(SignedHost::parse) {
            Some(h) => Some(h),
            None => return Err("malformed Host"),
        },
        None => None,
    };
    let target = match req.uri().authority() {
        Some(a) => Some(SignedHost::parse(a.as_str()).ok_or("malformed request target")?),
        None => None,
    };
    match (header, target) {
        (Some(h), Some(t)) if h != t => Err("Host does not match the request target"),
        (Some(h), _) | (None, Some(h)) => Ok(h),
        (None, None) => Err("missing Host"),
    }
}

/// Port of the listener, else of the `Host` header or request target.
fn request_port(req: &Request<Body>) -> Option<u16> {
    #[cfg(feature = "tenancy")]
    if let Some(crate::tenancy::ListenerPort(p)) = req.extensions().get().copied() {
        return Some(p);
    }
    req.headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.parse::<axum::http::uri::Authority>().ok())
        .and_then(|a| a.port_u16())
        .or_else(|| req.uri().port_u16())
}

/// Signed headers and port, one `name:value` line each, sorted by name.
/// Empty adds nothing, so a layer without them verifies the old format.
#[derive(Debug, Default)]
struct SignedExtras(std::collections::BTreeMap<String, String>);

impl SignedExtras {
    fn insert(&mut self, name: &str, value: &str) {
        self.0
            .insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }
}

fn canonical_request(
    method: &str,
    host: Option<&SignedHost>,
    path: &str,
    query: &str,
    date: &str,
    body_hash_hex: &str,
    extras: &SignedExtras,
) -> String {
    use std::fmt::Write as _;
    let sorted_query = sort_query(query);
    let mut out = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method.to_ascii_uppercase(),
        host.map_or("", |h| h.0.as_str()),
        path,
        sorted_query,
        date,
        body_hash_hex
    );
    for (name, value) in &extras.0 {
        let _ = write!(out, "\n{name}:{value}");
    }
    out
}

fn sort_query(q: &str) -> String {
    if q.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<&str> = q.split('&').filter(|s| !s.is_empty()).collect();
    pairs.sort();
    pairs.join("&")
}

// The SHA-256, HMAC and hex helpers live in `crate::crypto`.
use crate::crypto::{hmac_sha256, sha256_hex};

fn date_within_tolerance(date_str: &str, tolerance_secs: u64) -> bool {
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(date_str) else {
        return false;
    };
    let then = parsed.timestamp();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let delta = (now - then).abs();
    u64::try_from(delta).map_or(false, |d| d <= tolerance_secs)
}

// =====================================================================
// Client-side signing helper
// =====================================================================

/// Build the `Authorization` header value for a request signed with
/// `secret` under `key_id`. You must also set `X-Date` to the same
/// RFC 3339 timestamp. `host` is the server's host name (a port is
/// ignored; an invalid one signs nothing a server accepts). `body` may be
/// empty for GET or DELETE.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn sign_request(
    key_id: &str,
    secret: &[u8],
    method: &str,
    host: &str,
    path: &str,
    query: &str,
    date_rfc3339: &str,
    body: &[u8],
) -> String {
    RequestToSign::new(method, host, path, query, date_rfc3339, body).authorization(key_id, secret)
}

/// A request to sign, for a layer with [`HmacAuthLayer::signed_headers`]
/// or [`HmacAuthLayer::sign_port`]. Sign every header the layer lists.
pub struct RequestToSign<'a> {
    method: &'a str,
    host: &'a str,
    path: &'a str,
    query: &'a str,
    date: &'a str,
    body: &'a [u8],
    extras: SignedExtras,
}

impl<'a> RequestToSign<'a> {
    /// The fields [`sign_request`] takes; `date_rfc3339` goes in `X-Date`.
    #[must_use]
    pub fn new(
        method: &'a str,
        host: &'a str,
        path: &'a str,
        query: &'a str,
        date_rfc3339: &'a str,
        body: &'a [u8],
    ) -> Self {
        Self {
            method,
            host,
            path,
            query,
            date: date_rfc3339,
            body,
            extras: SignedExtras::default(),
        }
    }

    /// Sign a header with the value the request sends.
    #[must_use]
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.extras.insert(name, value);
        self
    }

    /// Sign the port the request is sent to.
    #[must_use]
    pub fn port(mut self, port: u16) -> Self {
        self.extras.insert(PORT_LINE, &port.to_string());
        self
    }

    /// The `Authorization` header value.
    #[must_use]
    pub fn authorization(&self, key_id: &str, secret: &[u8]) -> String {
        let canonical = canonical_request(
            self.method,
            SignedHost::parse(self.host).as_ref(),
            self.path,
            self.query,
            self.date,
            &sha256_hex(self.body),
            &self.extras,
        );
        let sig = hmac_sha256(secret, canonical.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig);
        format!("{SCHEME} keyId={key_id},signature={sig_b64}")
    }
}

/// Sign with the current time and return both headers: the RFC 3339
/// date and the authorization value.
#[must_use]
pub fn sign_now(
    key_id: &str,
    secret: &[u8],
    method: &str,
    host: &str,
    path: &str,
    query: &str,
    body: &[u8],
) -> (String, String) {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let auth = sign_request(key_id, secret, method, host, path, query, &now, body);
    (now, auth)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use axum::Router;
    use tower::{Layer, ServiceExt};

    const HOST: &str = "api.test";

    fn resolver_for(key: &'static str, secret: &'static [u8]) -> KeyResolver {
        Arc::new(move |k| {
            if k == key {
                Some(secret.to_vec())
            } else {
                None
            }
        })
    }

    fn app() -> Router {
        Router::new().route(
            "/r",
            post(|body: axum::body::Body| async move {
                let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
                format!("ok:{}", bytes.len())
            }),
        )
    }

    fn build_signed(
        method: &str,
        path_query: &str,
        body: &[u8],
        key_id: &str,
        secret: &[u8],
    ) -> Request<Body> {
        // Split path + query from a "/r?x=1" style input.
        let (path, query) = path_query.split_once('?').unwrap_or((path_query, ""));
        let (date, auth) = sign_now(key_id, secret, method, HOST, path, query, body);
        Request::builder()
            .method(method)
            .uri(path_query)
            .header("host", HOST)
            .header(HEADER_DATE, date)
            .header(HEADER_AUTH, auth)
            .body(Body::from(body.to_vec()))
            .unwrap()
    }

    #[tokio::test]
    async fn correctly_signed_request_passes_through() {
        let layer = HmacAuthLayer::new(resolver_for("k1", b"secret"));
        let svc = layer.layer(app().into_service::<Body>());
        let req = build_signed("POST", "/r", b"hello", "k1", b"secret");
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn missing_x_date_rejected_401() {
        let layer = HmacAuthLayer::new(resolver_for("k1", b"secret"));
        let svc = layer.layer(app().into_service::<Body>());
        let req = Request::builder()
            .method("POST")
            .uri("/r")
            .header("host", HOST)
            .header(HEADER_AUTH, "HMAC-SHA256 keyId=k1,signature=ZA==")
            .body(Body::empty())
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 401);
        let b = to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["error"], "unauthorized", "the ApiError envelope (#1193)");
    }

    #[tokio::test]
    async fn missing_authorization_rejected_401() {
        let layer = HmacAuthLayer::new(resolver_for("k1", b"secret"));
        let svc = layer.layer(app().into_service::<Body>());
        let req = Request::builder()
            .method("POST")
            .uri("/r")
            .header("host", HOST)
            .header(HEADER_DATE, "2026-05-02T12:00:00Z")
            .body(Body::empty())
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn unknown_key_id_rejected() {
        let layer = HmacAuthLayer::new(resolver_for("k1", b"secret"));
        let svc = layer.layer(app().into_service::<Body>());
        let req = build_signed("POST", "/r", b"x", "different", b"secret");
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn wrong_secret_rejected() {
        let layer = HmacAuthLayer::new(resolver_for("k1", b"secret"));
        let svc = layer.layer(app().into_service::<Body>());
        // Sign with a wrong secret but the expected key id.
        let req = build_signed("POST", "/r", b"x", "k1", b"wrong");
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    /// Sign for `signed_for`; send with `host` (a header) and `uri`.
    async fn status_for(
        layer: HmacAuthLayer,
        signed_for: &str,
        uri: &str,
        host: Option<&str>,
    ) -> u16 {
        let (date, auth) = sign_now("k1", b"secret", "POST", signed_for, "/r", "", b"x");
        let mut req = Request::builder().method("POST").uri(uri);
        if let Some(h) = host {
            req = req.header("host", h);
        }
        let req = req
            .header(HEADER_DATE, date)
            .header(HEADER_AUTH, auth)
            .body(Body::from("x"))
            .unwrap();
        let svc = layer.layer(app().into_service::<Body>());
        svc.oneshot(req).await.unwrap().status().as_u16()
    }

    /// #1836 — a signature for one host fails on another sharing the key.
    #[tokio::test]
    async fn a_signature_is_bound_to_its_host() {
        let layer = || HmacAuthLayer::new(resolver_for("k1", b"secret"));
        assert_eq!(
            status_for(layer(), "a.test", "/r", Some("a.test")).await,
            200
        );
        assert_eq!(
            status_for(layer(), "a.test", "/r", Some("b.test")).await,
            401
        );
        // Case and port do not change the signed host.
        assert_eq!(
            status_for(layer(), "A.test", "/r", Some("a.test:8443")).await,
            200
        );
        assert_eq!(status_for(layer(), "a.test", "/r", None).await, 401);
    }

    /// The HTTP/2 authority stands in for `Host`, and must agree with it.
    #[tokio::test]
    async fn the_request_authority_is_the_host_and_must_agree() {
        let layer = || HmacAuthLayer::new(resolver_for("k1", b"secret"));
        let h2 = "https://a.test/r";
        assert_eq!(status_for(layer(), "a.test", h2, None).await, 200);
        assert_eq!(status_for(layer(), "b.test", h2, Some("b.test")).await, 401);
        assert_eq!(status_for(layer(), "a.test", h2, Some("b.test")).await, 401);
    }

    /// A pinned host is what gets verified, whatever `Host` says.
    #[tokio::test]
    async fn a_pinned_host_replaces_the_request_host() {
        let layer = || HmacAuthLayer::new(resolver_for("k1", b"secret")).host("api.test");
        assert_eq!(
            status_for(layer(), "api.test", "/r", Some("backend:8080")).await,
            200
        );
        assert_eq!(
            status_for(layer(), "backend", "/r", Some("backend:8080")).await,
            401
        );
    }

    /// One spelling per host: IPv6 bracketed, trailing dot dropped, junk refused.
    #[test]
    fn signed_host_has_one_spelling() {
        let p = |s: &str| SignedHost::parse(s).map(|h| h.0);
        assert_eq!(p("[::1]:8080").as_deref(), Some("[::1]"));
        assert_eq!(p("::1").as_deref(), Some("[::1]"));
        assert_eq!(p("[0:0::1]").as_deref(), Some("[::1]"));
        assert_eq!(p("A.test.").as_deref(), Some("a.test"));
        assert_eq!(p("a.test.:443").as_deref(), Some("a.test"));
        for bad in ["", " ", ".", "a b", "u@a.test", "[zz]"] {
            assert_eq!(p(bad), None, "{bad:?}");
        }
    }

    #[test]
    #[should_panic(expected = "invalid host")]
    fn an_empty_pin_is_refused() {
        let _ = HmacAuthLayer::new(resolver_for("k1", b"secret")).host("");
    }

    /// The signer and the verifier fold IPv6 and a trailing dot the same way.
    #[tokio::test]
    async fn ipv6_and_trailing_dot_hosts_verify() {
        let layer = || HmacAuthLayer::new(resolver_for("k1", b"secret"));
        assert_eq!(
            status_for(layer(), "::1", "/r", Some("[::1]:8080")).await,
            200
        );
        assert_eq!(
            status_for(layer(), "a.test.", "/r", Some("a.test")).await,
            200
        );
        assert_eq!(
            status_for(layer(), "a.test", "/r", Some("a.test.")).await,
            200
        );
    }

    /// Send `/r` signed by `signer`, with `x-org: org` and `Host: host`.
    async fn send_signed(
        layer: HmacAuthLayer,
        signer: RequestToSign<'_>,
        org: &str,
        host: &str,
    ) -> u16 {
        let auth = signer.authorization("k1", b"secret");
        let req = Request::builder()
            .method("POST")
            .uri("/r")
            .header("host", host)
            .header("x-org", org)
            .header(HEADER_DATE, signer.date)
            .header(HEADER_AUTH, auth)
            .body(Body::from("x"))
            .unwrap();
        let svc = layer.layer(app().into_service::<Body>());
        svc.oneshot(req).await.unwrap().status().as_u16()
    }

    fn now() -> String {
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    /// #2492 — a request signed for one tenant header fails for another.
    #[tokio::test]
    async fn a_signed_header_cannot_be_swapped() {
        let layer = || HmacAuthLayer::new(resolver_for("k1", b"secret")).signed_headers(["X-Org"]);
        let date = now();
        let signer =
            || RequestToSign::new("POST", HOST, "/r", "", &date, b"x").header("x-org", "acme");
        assert_eq!(send_signed(layer(), signer(), "acme", HOST).await, 200);
        assert_eq!(send_signed(layer(), signer(), "globex", HOST).await, 401);
        // Unconfigured, the header is not part of the signature.
        let plain = RequestToSign::new("POST", HOST, "/r", "", &date, b"x");
        let open = HmacAuthLayer::new(resolver_for("k1", b"secret"));
        assert_eq!(send_signed(open, plain, "globex", HOST).await, 200);
    }

    /// #2492 — with `sign_port`, a replay to another port fails.
    #[tokio::test]
    async fn a_signed_port_cannot_be_swapped() {
        let layer = || HmacAuthLayer::new(resolver_for("k1", b"secret")).sign_port();
        let date = now();
        let signer = || RequestToSign::new("POST", HOST, "/r", "", &date, b"x").port(8001);
        assert_eq!(
            send_signed(layer(), signer(), "", "api.test:8001").await,
            200
        );
        assert_eq!(
            send_signed(layer(), signer(), "", "api.test:8002").await,
            401
        );
    }

    /// #2492 — the listener port wins over `Host`, which the client picks.
    #[cfg(feature = "tenancy")]
    #[tokio::test]
    async fn the_listener_port_is_the_signed_port() {
        let date = now();
        let signer = RequestToSign::new("POST", HOST, "/r", "", &date, b"x").port(8001);
        let auth = signer.authorization("k1", b"secret");
        let mut req = Request::builder()
            .method("POST")
            .uri("/r")
            .header("host", "api.test:8001")
            .header(HEADER_DATE, date.as_str())
            .header(HEADER_AUTH, auth)
            .body(Body::from("x"))
            .unwrap();
        req.extensions_mut()
            .insert(crate::tenancy::ListenerPort(8002));
        let layer = HmacAuthLayer::new(resolver_for("k1", b"secret")).sign_port();
        let svc = layer.layer(app().into_service::<Body>());
        assert_eq!(svc.oneshot(req).await.unwrap().status(), 401);
    }

    /// #2492 — a key bound to a slug only acts for that tenant.
    #[tokio::test]
    async fn a_bound_key_only_acts_for_its_tenant() {
        let tenant_of: KeyTenantResolver = Arc::new(|k| (k == "k1").then(|| "acme".to_owned()));
        let layer = || {
            HmacAuthLayer::new(resolver_for("k1", b"secret"))
                .tenant_header("x-org", tenant_of.clone())
        };
        let date = now();
        let signer = |org: &'static str| {
            RequestToSign::new("POST", HOST, "/r", "", &date, b"x").header("x-org", org)
        };
        assert_eq!(
            send_signed(layer(), signer("acme"), "acme", HOST).await,
            200
        );
        // Validly signed, but for another tenant.
        assert_eq!(
            send_signed(layer(), signer("globex"), "globex", HOST).await,
            403
        );
        // The tenant header is signed too.
        assert_eq!(
            send_signed(layer(), signer("acme"), "globex", HOST).await,
            401
        );
    }

    #[tokio::test]
    async fn body_tampering_rejected() {
        let layer = HmacAuthLayer::new(resolver_for("k1", b"secret"));
        let svc = layer.layer(app().into_service::<Body>());
        // Sign with one body, but ship a different one.
        let (date, auth) = sign_now("k1", b"secret", "POST", HOST, "/r", "", b"original");
        let req = Request::builder()
            .method("POST")
            .uri("/r")
            .header("host", HOST)
            .header(HEADER_DATE, date)
            .header(HEADER_AUTH, auth)
            .body(Body::from("tampered".to_owned()))
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[cfg(feature = "cache")]
    #[tokio::test]
    async fn nonce_store_rejects_replay_within_window() {
        use crate::cache::InMemoryCache;
        let store = std::sync::Arc::new(InMemoryCache::new());
        let layer = HmacAuthLayer::new(resolver_for("k1", b"secret")).nonce_store(store);
        // One signed request, replayed verbatim (identical signature).
        let (date, auth) = sign_now("k1", b"secret", "POST", HOST, "/r", "", b"hello");
        let mk = || {
            Request::builder()
                .method("POST")
                .uri("/r")
                .header("host", HOST)
                .header(HEADER_DATE, date.clone())
                .header(HEADER_AUTH, auth.clone())
                .body(Body::from("hello"))
                .unwrap()
        };
        // First delivery: accepted (signature valid, nonce unseen).
        let svc = layer.clone().layer(app().into_service::<Body>());
        assert_eq!(svc.oneshot(mk()).await.unwrap().status(), 200);
        // Replay: same signature, now seen → rejected (shared store).
        let svc = layer.layer(app().into_service::<Body>());
        assert_eq!(svc.oneshot(mk()).await.unwrap().status(), 401);
    }

    /// A cache whose `exists` is slow, so a check-then-set lets two
    /// copies through; `add` stays the inner cache's atomic one.
    #[cfg(feature = "cache")]
    struct SlowExists(crate::cache::InMemoryCache);

    #[cfg(feature = "cache")]
    #[async_trait::async_trait]
    impl crate::cache::Cache for SlowExists {
        async fn get(&self, k: &str) -> Result<Option<String>, crate::cache::CacheError> {
            self.0.get(k).await
        }
        async fn set(
            &self,
            k: &str,
            v: &str,
            ttl: Option<std::time::Duration>,
        ) -> Result<(), crate::cache::CacheError> {
            self.0.set(k, v, ttl).await
        }
        async fn delete(&self, k: &str) -> Result<(), crate::cache::CacheError> {
            self.0.delete(k).await
        }
        async fn exists(&self, k: &str) -> Result<bool, crate::cache::CacheError> {
            // Read, then stall: the answer goes stale before the caller acts.
            let seen = self.0.exists(k).await;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            seen
        }
        async fn clear(&self) -> Result<(), crate::cache::CacheError> {
            self.0.clear().await
        }
        async fn add(
            &self,
            k: &str,
            v: &str,
            ttl: Option<std::time::Duration>,
        ) -> Result<bool, crate::cache::CacheError> {
            self.0.add(k, v, ttl).await
        }
    }

    /// #1828 — two copies of one request sent at once: only one gets in.
    #[cfg(feature = "cache")]
    #[tokio::test]
    async fn simultaneous_replays_let_only_one_through() {
        let store = Arc::new(SlowExists(crate::cache::InMemoryCache::new()));
        let layer = HmacAuthLayer::new(resolver_for("k1", b"secret")).nonce_store(store);
        let (date, auth) = sign_now("k1", b"secret", "POST", HOST, "/r", "", b"hello");
        let send = || {
            let svc = layer.clone().layer(app().into_service::<Body>());
            let req = Request::builder()
                .method("POST")
                .uri("/r")
                .header("host", HOST)
                .header(HEADER_DATE, date.clone())
                .header(HEADER_AUTH, auth.clone())
                .body(Body::from("hello"))
                .unwrap();
            async move { svc.oneshot(req).await.unwrap().status().as_u16() }
        };
        let (a, b) = tokio::join!(send(), send());
        let mut got = [a, b];
        got.sort_unstable();
        assert_eq!(got, [200, 401], "both copies of a replay were accepted");
    }

    /// A request dated `tolerance` ahead is valid for `2 × tolerance`,
    /// so its nonce must outlive a plain `tolerance`.
    #[cfg(feature = "cache")]
    #[tokio::test]
    async fn a_future_dated_request_cannot_be_replayed_after_tolerance() {
        let store = Arc::new(crate::cache::InMemoryCache::new());
        let layer = HmacAuthLayer::new(resolver_for("k1", b"secret"))
            .tolerance_secs(2)
            .nonce_store(store);
        let date = (chrono::Utc::now() + chrono::Duration::seconds(2))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let auth = sign_request("k1", b"secret", "POST", HOST, "/r", "", &date, b"x");
        let send = || {
            let svc = layer.clone().layer(app().into_service::<Body>());
            let req = Request::builder()
                .method("POST")
                .uri("/r")
                .header("host", HOST)
                .header(HEADER_DATE, date.clone())
                .header(HEADER_AUTH, auth.clone())
                .body(Body::from("x"))
                .unwrap();
            async move { svc.oneshot(req).await.unwrap().status().as_u16() }
        };
        assert_eq!(send().await, 200);
        // Past `tolerance`, but X-Date is still inside the window.
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        assert_eq!(send().await, 401, "the replay outlived its nonce");
    }

    /// A cache that fails every call.
    #[cfg(feature = "cache")]
    struct Down;

    #[cfg(feature = "cache")]
    #[async_trait::async_trait]
    impl crate::cache::Cache for Down {
        async fn get(&self, _: &str) -> Result<Option<String>, crate::cache::CacheError> {
            Err(crate::cache::CacheError::Connection("down".into()))
        }
        async fn set(
            &self,
            _: &str,
            _: &str,
            _: Option<std::time::Duration>,
        ) -> Result<(), crate::cache::CacheError> {
            Err(crate::cache::CacheError::Connection("down".into()))
        }
        async fn delete(&self, _: &str) -> Result<(), crate::cache::CacheError> {
            Err(crate::cache::CacheError::Connection("down".into()))
        }
        async fn exists(&self, _: &str) -> Result<bool, crate::cache::CacheError> {
            Err(crate::cache::CacheError::Connection("down".into()))
        }
        async fn clear(&self) -> Result<(), crate::cache::CacheError> {
            Err(crate::cache::CacheError::Connection("down".into()))
        }
    }

    /// A nonce store outage lets the request through, but not silently.
    #[cfg(all(feature = "cache", feature = "runtime"))]
    #[test]
    fn a_failing_nonce_store_passes_and_warns() {
        let buf = crate::testkit::CaptureWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let status = tracing::subscriber::with_default(subscriber, || {
            let layer =
                HmacAuthLayer::new(resolver_for("k1", b"secret")).nonce_store(Arc::new(Down));
            let svc = layer.layer(app().into_service::<Body>());
            let req = build_signed("POST", "/r", b"x", "k1", b"secret");
            rt.block_on(svc.oneshot(req)).unwrap().status()
        });
        assert_eq!(status, 200);
        let out = buf.contents();
        assert!(out.contains("nonce store failed"), "{out}");
        assert!(out.contains("rustango::hmac_auth"), "{out}");
    }

    /// #1828 — a nonce store that keeps nothing is not a silent no-op.
    #[cfg(all(feature = "cache", feature = "runtime"))]
    #[test]
    fn a_null_nonce_store_warns() {
        let buf = crate::testkit::CaptureWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let _ = HmacAuthLayer::new(resolver_for("k1", b"secret"))
                .nonce_store(Arc::new(crate::cache::NullCache));
            let _ = HmacAuthLayer::new(resolver_for("k1", b"secret"))
                .nonce_store(Arc::new(crate::cache::InMemoryCache::new()));
        });
        let out = buf.contents();
        assert_eq!(out.matches("replay protection is off").count(), 1, "{out}");
        assert!(out.contains("rustango::hmac_auth"), "{out}");
    }

    #[tokio::test]
    async fn query_reordering_does_not_break_signature() {
        let layer = HmacAuthLayer::new(resolver_for("k1", b"secret"));
        let svc = layer.layer(app().into_service::<Body>());
        // Sign with one query order; ship a different order — must still pass
        // because we sort before signing on both ends.
        let (date, auth) = sign_now("k1", b"secret", "POST", HOST, "/r", "a=1&b=2", b"");
        let req = Request::builder()
            .method("POST")
            .uri("/r?b=2&a=1")
            .header("host", HOST)
            .header(HEADER_DATE, date)
            .header(HEADER_AUTH, auth)
            .body(Body::empty())
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn old_date_rejected_outside_tolerance() {
        let layer = HmacAuthLayer::new(resolver_for("k1", b"secret")).tolerance_secs(60); // 1 min
        let svc = layer.layer(app().into_service::<Body>());
        // Sign with a date 10 min in the past.
        let old = (chrono::Utc::now() - chrono::Duration::minutes(10))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let auth = sign_request("k1", b"secret", "POST", HOST, "/r", "", &old, b"");
        let req = Request::builder()
            .method("POST")
            .uri("/r")
            .header("host", HOST)
            .header(HEADER_DATE, old)
            .header(HEADER_AUTH, auth)
            .body(Body::empty())
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn malformed_authorization_rejected() {
        let layer = HmacAuthLayer::new(resolver_for("k1", b"secret"));
        let svc = layer.layer(app().into_service::<Body>());
        let req = Request::builder()
            .method("POST")
            .uri("/r")
            .header("host", HOST)
            .header(HEADER_DATE, "2026-05-02T12:00:00Z")
            .header(HEADER_AUTH, "Bearer some-token")
            .body(Body::empty())
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    // -------- pure helpers

    #[test]
    fn parse_auth_extracts_key_and_signature() {
        let p = parse_auth("HMAC-SHA256 keyId=k1,signature=YWJj").unwrap();
        assert_eq!(p.key_id, "k1");
        assert_eq!(p.signature, b"abc");
    }

    #[test]
    fn parse_auth_handles_quoted_values() {
        let p = parse_auth(r#"HMAC-SHA256 keyId="k1",signature="YWJj""#).unwrap();
        assert_eq!(p.key_id, "k1");
        assert_eq!(p.signature, b"abc");
    }

    #[test]
    fn parse_auth_rejects_other_schemes() {
        assert!(parse_auth("Bearer abc").is_none());
    }

    #[test]
    fn canonical_request_is_deterministic() {
        let h = SignedHost::parse("API.test:443");
        let a = canonical_request(
            "POST",
            h.as_ref(),
            "/r",
            "x=1&y=2",
            "2026-05-02T12:00:00Z",
            "abc",
            &SignedExtras::default(),
        );
        let b = canonical_request(
            "post",
            h.as_ref(),
            "/r",
            "y=2&x=1",
            "2026-05-02T12:00:00Z",
            "abc",
            &SignedExtras::default(),
        );
        // Method case + query order shouldn't change the canonical form.
        assert_eq!(a, b);
    }

    #[test]
    fn sort_query_is_alphabetical() {
        assert_eq!(sort_query("z=3&a=1&m=2"), "a=1&m=2&z=3");
        assert_eq!(sort_query(""), "");
    }

    #[test]
    fn date_within_tolerance_round_trip() {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        assert!(date_within_tolerance(&now, 60));
    }

    #[test]
    fn date_far_outside_tolerance_rejected() {
        let old = (chrono::Utc::now() - chrono::Duration::hours(1))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        assert!(!date_within_tolerance(&old, 60));
    }

    #[test]
    fn date_with_garbage_string_rejected() {
        assert!(!date_within_tolerance("not-a-date", 60));
    }

    #[test]
    fn hex_encode_and_sha256_round_trip() {
        // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        let h = sha256_hex(b"");
        assert_eq!(
            h,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
