//! HMAC-signed request authentication for service-to-service traffic.
//!
//! AWS-style: each request carries `X-Date` and an `Authorization`
//! header containing a key id + HMAC-SHA256 over the canonical
//! request. The shared key is never transmitted, and replay attacks
//! are bounded by a configurable `X-Date` tolerance window.
//!
//! Distinct from [`crate::api_keys`] (bearer-token style — the key
//! itself rides the wire on every call): pick HMAC when callers can
//! sign and you don't trust the channel; pick bearer when TLS is
//! enough and you want the simplest possible client.
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
//! <PATH>\n
//! <SORTED-QUERY-STRING>\n
//! <X-DATE>\n
//! <HEX-SHA256(BODY)>
//! ```
//!
//! Sorted query so `?b=2&a=1` and `?a=1&b=2` produce the same
//! signature. Body is hashed (SHA-256 hex) — saves the verifier from
//! buffering and re-hashing inside HMAC.
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

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{to_bytes, Body};
use axum::http::{header, HeaderValue, Request, Response, StatusCode};
use base64::Engine;
use subtle::ConstantTimeEq;
use tower::Service;

const HEADER_DATE: &str = "x-date";
const HEADER_AUTH: &str = "authorization";
const SCHEME: &str = "HMAC-SHA256";
const DEFAULT_TOLERANCE_SECS: u64 = 300; // 5 min — RFC convention
const DEFAULT_BODY_LIMIT: usize = 10 * 1024 * 1024;

/// Closure that maps `key_id` → `Option<secret>`. Implementors typically
/// look up the key in a DB / cache. Returning `None` rejects the
/// request with 401.
pub type KeyResolver =
    Arc<dyn Fn(&str) -> Option<Vec<u8>> + Send + Sync>;

#[derive(Clone)]
pub struct HmacAuthLayer {
    inner: Arc<HmacAuthConfig>,
}

#[derive(Clone)]
struct HmacAuthConfig {
    resolver: KeyResolver,
    tolerance_secs: u64,
    body_limit: usize,
}

impl HmacAuthLayer {
    #[must_use]
    pub fn new(resolver: KeyResolver) -> Self {
        Self {
            inner: Arc::new(HmacAuthConfig {
                resolver,
                tolerance_secs: DEFAULT_TOLERANCE_SECS,
                body_limit: DEFAULT_BODY_LIMIT,
            }),
        }
    }

    /// Override the `X-Date` tolerance window (default 300 s = ±5 min).
    #[must_use]
    pub fn tolerance_secs(mut self, secs: u64) -> Self {
        Arc::make_mut(&mut self.inner).tolerance_secs = secs;
        self
    }

    /// Cap the body size we'll buffer for hashing. Requests over this
    /// are rejected with 413. Default 10 MiB.
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

    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let query = req.uri().query().unwrap_or("").to_owned();

    let (parts, body) = req.into_parts();
    let bytes = match to_bytes(body, cfg.body_limit).await {
        Ok(b) => b,
        Err(_) => return Err(too_large()),
    };
    let body_hash = sha256_hex(&bytes);

    let canonical = canonical_request(method.as_str(), &path, &query, &date, &body_hash);
    let expected_sig = hmac_sha256(&secret, canonical.as_bytes());

    if expected_sig.ct_eq(&parsed.signature).unwrap_u8() == 0 {
        return Err(deny("signature mismatch"));
    }
    Ok(Request::from_parts(parts, Body::from(bytes)))
}

fn deny(msg: &str) -> Response<Body> {
    let body = format!(r#"{{"error":"unauthorized","reason":"{msg}"}}"#);
    let mut resp = Response::new(Body::from(body));
    *resp.status_mut() = StatusCode::UNAUTHORIZED;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

fn too_large() -> Response<Body> {
    let mut resp = Response::new(Body::from(r#"{"error":"payload too large"}"#));
    *resp.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
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
            signature = base64::engine::general_purpose::STANDARD
                .decode(raw)
                .ok();
        }
    }
    Some(ParsedAuth {
        key_id: key_id?,
        signature: signature?,
    })
}

fn canonical_request(
    method: &str,
    path: &str,
    query: &str,
    date: &str,
    body_hash_hex: &str,
) -> String {
    let sorted_query = sort_query(query);
    format!(
        "{}\n{}\n{}\n{}\n{}",
        method.to_ascii_uppercase(),
        path,
        sorted_query,
        date,
        body_hash_hex
    )
}

fn sort_query(q: &str) -> String {
    if q.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<&str> = q.split('&').filter(|s| !s.is_empty()).collect();
    pairs.sort();
    pairs.join("&")
}

// SHA-256 / HMAC-SHA256 / hex-encode helpers live in
// [`crate::crypto`] — re-exported as the same names so the existing
// call sites read unchanged.
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
/// `secret` for key id `key_id`. Caller is responsible for setting
/// `X-Date` to a matching RFC 3339 timestamp.
///
/// `body` may be empty for GET/DELETE requests.
#[must_use]
pub fn sign_request(
    key_id: &str,
    secret: &[u8],
    method: &str,
    path: &str,
    query: &str,
    date_rfc3339: &str,
    body: &[u8],
) -> String {
    let body_hash = sha256_hex(body);
    let canonical = canonical_request(method, path, query, date_rfc3339, &body_hash);
    let sig = hmac_sha256(secret, canonical.as_bytes());
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig);
    format!("{SCHEME} keyId={key_id},signature={sig_b64}")
}

/// Convenience: pick `now()` as the date and return both headers
/// (the date + the authorization). Date is RFC 3339 / ISO 8601.
#[must_use]
pub fn sign_now(
    key_id: &str,
    secret: &[u8],
    method: &str,
    path: &str,
    query: &str,
    body: &[u8],
) -> (String, String) {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let auth = sign_request(key_id, secret, method, path, query, &now, body);
    (now, auth)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use axum::Router;
    use tower::{Layer, ServiceExt};

    fn resolver_for(key: &'static str, secret: &'static [u8]) -> KeyResolver {
        Arc::new(move |k| if k == key { Some(secret.to_vec()) } else { None })
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

    fn build_signed(method: &str, path_query: &str, body: &[u8], key_id: &str, secret: &[u8]) -> Request<Body> {
        // Split path + query from a "/r?x=1" style input.
        let (path, query) = path_query.split_once('?').unwrap_or((path_query, ""));
        let (date, auth) = sign_now(key_id, secret, method, path, query, body);
        Request::builder()
            .method(method)
            .uri(path_query)
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
            .header(HEADER_AUTH, "HMAC-SHA256 keyId=k1,signature=ZA==")
            .body(Body::empty())
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn missing_authorization_rejected_401() {
        let layer = HmacAuthLayer::new(resolver_for("k1", b"secret"));
        let svc = layer.layer(app().into_service::<Body>());
        let req = Request::builder()
            .method("POST")
            .uri("/r")
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

    #[tokio::test]
    async fn body_tampering_rejected() {
        let layer = HmacAuthLayer::new(resolver_for("k1", b"secret"));
        let svc = layer.layer(app().into_service::<Body>());
        // Sign with one body, but ship a different one.
        let (date, auth) = sign_now("k1", b"secret", "POST", "/r", "", b"original");
        let req = Request::builder()
            .method("POST")
            .uri("/r")
            .header(HEADER_DATE, date)
            .header(HEADER_AUTH, auth)
            .body(Body::from("tampered".to_owned()))
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn query_reordering_does_not_break_signature() {
        let layer = HmacAuthLayer::new(resolver_for("k1", b"secret"));
        let svc = layer.layer(app().into_service::<Body>());
        // Sign with one query order; ship a different order — must still pass
        // because we sort before signing on both ends.
        let (date, auth) = sign_now("k1", b"secret", "POST", "/r", "a=1&b=2", b"");
        let req = Request::builder()
            .method("POST")
            .uri("/r?b=2&a=1")
            .header(HEADER_DATE, date)
            .header(HEADER_AUTH, auth)
            .body(Body::empty())
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn old_date_rejected_outside_tolerance() {
        let layer = HmacAuthLayer::new(resolver_for("k1", b"secret"))
            .tolerance_secs(60); // 1 min
        let svc = layer.layer(app().into_service::<Body>());
        // Sign with a date 10 min in the past.
        let old = (chrono::Utc::now() - chrono::Duration::minutes(10))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let auth = sign_request("k1", b"secret", "POST", "/r", "", &old, b"");
        let req = Request::builder()
            .method("POST")
            .uri("/r")
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
        let a = canonical_request("POST", "/r", "x=1&y=2", "2026-05-02T12:00:00Z", "abc");
        let b = canonical_request("post", "/r", "y=2&x=1", "2026-05-02T12:00:00Z", "abc");
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
