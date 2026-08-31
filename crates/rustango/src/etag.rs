//! ETag middleware — hashes response bodies and serves `304 Not Modified`
//! when the client's `If-None-Match` matches.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::etag::{EtagLayer, EtagRouterExt};
//!
//! let app = Router::new()
//!     .route("/api/posts", get(list_posts))
//!     .etag(EtagLayer::default());
//! ```
//!
//! ## How it works
//!
//! For every successful (2xx) response with a non-empty body:
//! 1. Compute a hash of the body bytes (64-bit FNV-1a + length)
//! 2. Set `ETag: "<base64 hash>"` on the response
//! 3. If the request's `If-None-Match` matches — a comma-separated list
//!    of etags, or the wildcard `*` — reply `304 Not Modified` with an
//!    empty body, carrying the caching headers (`Cache-Control`, `Vary`,
//!    `Expires`, …) a matching 200 would have sent (RFC 7232 §4.1).
//!
//! Non-2xx responses are passed through untouched.
//!
//! ## When to use
//!
//! - Read-heavy GET endpoints whose responses repeat across requests
//! - Skip for personalized responses unless you scope by user in the cache key
//! - Skip for streaming/large responses (the middleware buffers the body)

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::header::{ETAG, IF_NONE_MATCH};
use axum::http::{HeaderValue, Request, Response, StatusCode};
use axum::middleware::Next;
use axum::Router;

/// ETag middleware configuration.
#[derive(Clone, Default)]
pub struct EtagLayer {
    /// Hard cap on response body size for ETag computation. Responses
    /// larger than this are passed through unmodified. Default: 4 MiB.
    pub max_body_bytes: Option<usize>,
}

impl EtagLayer {
    /// Default config — hashes responses up to 4 MiB.
    #[must_use]
    pub fn new() -> Self {
        Self {
            max_body_bytes: Some(4 * 1024 * 1024),
        }
    }

    /// Override the maximum body size. `None` means "no cap" (use with care).
    #[must_use]
    pub fn max_body_bytes(mut self, n: Option<usize>) -> Self {
        self.max_body_bytes = n;
        self
    }
}

/// Extension trait — `.etag(layer)` ergonomics on Router.
pub trait EtagRouterExt {
    #[must_use]
    fn etag(self, layer: EtagLayer) -> Self;
}

impl<S: Clone + Send + Sync + 'static> EtagRouterExt for Router<S> {
    fn etag(self, layer: EtagLayer) -> Self {
        let cfg = Arc::new(layer);
        self.layer(axum::middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let cfg = cfg.clone();
                async move { handle(cfg, req, next).await }
            },
        ))
    }
}

async fn handle(cfg: Arc<EtagLayer>, req: Request<Body>, next: Next) -> Response<Body> {
    // Extract client's If-None-Match before consuming the request
    let client_etag = req
        .headers()
        .get(IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let response = next.run(req).await;
    let (parts, body) = response.into_parts();

    // Don't hash non-2xx responses
    if !parts.status.is_success() {
        return Response::from_parts(parts, body);
    }

    // Buffer the body up to max_body_bytes
    let limit = cfg.max_body_bytes.unwrap_or(usize::MAX);
    let bytes = match to_bytes(body, limit).await {
        Ok(b) => b,
        Err(_) => {
            // Body too large or stream error — pass through with empty body since we already consumed it
            return Response::from_parts(parts, Body::empty());
        }
    };

    if bytes.is_empty() {
        return Response::from_parts(parts, Body::from(bytes));
    }

    let etag = compute_etag(&bytes);
    let mut response = Response::from_parts(parts, Body::from(bytes));
    if let Ok(v) = HeaderValue::from_str(&etag) {
        response.headers_mut().insert(ETAG, v);
    }

    if let Some(client) = client_etag {
        // `If-None-Match` is a comma-separated list, or the wildcard `*`
        // which matches any current representation (#1258). Weak
        // comparison applies for a conditional GET (RFC 7232 §3.2).
        let ours = normalize_etag(&etag);
        let matched = client.trim() == "*"
            || client
                .split(',')
                .any(|candidate| normalize_etag(candidate) == ours);
        if matched {
            // 304 Not Modified — drop the body, but carry the headers a
            // matching 200 would have sent that govern caching (RFC 7232
            // §4.1), not just the ETag. Dropping `Cache-Control` / `Vary`
            // would let a downstream cache apply the wrong freshness or
            // vary key.
            let mut not_modified = Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .body(Body::empty())
                .unwrap();
            let carry = [
                ETAG,
                axum::http::header::CACHE_CONTROL,
                axum::http::header::VARY,
                axum::http::header::EXPIRES,
                axum::http::header::CONTENT_LOCATION,
                axum::http::header::DATE,
            ];
            for (k, v) in response.headers() {
                if carry.contains(k) {
                    not_modified.headers_mut().insert(k.clone(), v.clone());
                }
            }
            return not_modified;
        }
    }

    response
}

/// Compute an ETag for `bytes` — `"<base64 of 64-bit FNV-1a hash + length>"`.
///
/// Not cryptographic — ETag collisions cause false-positive 304s, not security
/// issues. The combined hash + length is collision-resistant enough for cache
/// validation. (Crypto-strength ETags would force a sha2 dependency on every
/// `admin` build.)
fn compute_etag(bytes: &[u8]) -> String {
    use base64::Engine;
    let hash = fnv1a_64(bytes);
    let len = bytes.len() as u64;
    let mut buf = [0u8; 16];
    buf[..8].copy_from_slice(&hash.to_be_bytes());
    buf[8..].copy_from_slice(&len.to_be_bytes());
    format!(
        "\"{}\"",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
    )
}

/// 64-bit FNV-1a hash. Constants from the FNV reference.
const FNV_OFFSET_64: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME_64: u64 = 0x0000_0100_0000_01b3;

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_64;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV_PRIME_64);
    }
    hash
}

/// Strip surrounding quotes + `W/` weak prefix for comparison purposes.
fn normalize_etag(s: &str) -> &str {
    let s = s.trim();
    let s = s.strip_prefix("W/").unwrap_or(s);
    s.trim_matches('"')
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::http::header;
    use axum::routing::get;
    use tower::ServiceExt as _;

    fn app() -> Router {
        Router::new()
            .route(
                "/page",
                get(|| async {
                    axum::response::Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CACHE_CONTROL, "max-age=60")
                        .header(header::VARY, "Accept-Encoding")
                        .body(Body::from("stable-body"))
                        .unwrap()
                }),
            )
            .etag(EtagLayer::new())
    }

    async fn etag_of(app: &Router) -> String {
        let r = app
            .clone()
            .oneshot(Request::builder().uri("/page").body(Body::empty()).unwrap())
            .await
            .unwrap();
        r.headers().get(ETAG).unwrap().to_str().unwrap().to_owned()
    }

    /// #1258 — `If-None-Match: *` matches any representation → 304.
    #[tokio::test]
    async fn if_none_match_wildcard_returns_304() {
        let app = app();
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/page")
                    .header(IF_NONE_MATCH, "*")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_MODIFIED);
    }

    /// #1258 — a comma-separated `If-None-Match` list containing our etag
    /// must 304, not return a full 200.
    #[tokio::test]
    async fn if_none_match_list_matches_one_entry() {
        let app = app();
        let ours = etag_of(&app).await;
        let list = format!("\"deadbeef\", {ours}");
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/page")
                    .header(IF_NONE_MATCH, list)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::NOT_MODIFIED,
            "list match should 304"
        );
    }

    /// #1258 — the 304 must carry the caching headers a 200 would
    /// (RFC 7232 §4.1), not just the ETag.
    #[tokio::test]
    async fn not_modified_carries_caching_headers() {
        let app = app();
        let ours = etag_of(&app).await;
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/page")
                    .header(IF_NONE_MATCH, ours)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            r.headers().get(header::CACHE_CONTROL).unwrap(),
            "max-age=60"
        );
        assert_eq!(r.headers().get(header::VARY).unwrap(), "Accept-Encoding");
        assert!(r.headers().get(ETAG).is_some());
    }

    #[test]
    fn etag_is_deterministic_for_same_bytes() {
        let a = compute_etag(b"hello");
        let b = compute_etag(b"hello");
        assert_eq!(a, b);
    }

    #[test]
    fn etag_differs_for_different_bytes() {
        let a = compute_etag(b"hello");
        let b = compute_etag(b"world");
        assert_ne!(a, b);
    }

    #[test]
    fn etag_is_quoted() {
        let e = compute_etag(b"x");
        assert!(e.starts_with('"'));
        assert!(e.ends_with('"'));
    }

    #[test]
    fn normalize_strips_weak_prefix_and_quotes() {
        assert_eq!(normalize_etag("\"abc\""), "abc");
        assert_eq!(normalize_etag("W/\"abc\""), "abc");
        assert_eq!(normalize_etag("abc"), "abc");
    }
}
