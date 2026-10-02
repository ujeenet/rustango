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
//! For each 2xx response with a body:
//! 1. Hash the body (64-bit FNV-1a plus the length).
//! 2. Set `ETag: "<base64 hash>"`.
//! 3. If `If-None-Match` matches — a list of etags, or `*` — reply
//!    `304 Not Modified` with no body, keeping the caching headers a
//!    200 would have sent (RFC 7232 §4.1).
//!
//! Non-2xx and `206` responses pass through. A response that already
//! has an `ETag` keeps it and is not buffered. So does a body whose
//! size hint is unknown (a stream, SSE) or over `max_body_bytes`.
//!
//! ## When to use
//!
//! Good for read-heavy GET endpoints that return the same bytes again
//! and again. Skip it for per-user responses.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::header::{ETAG, IF_NONE_MATCH};
use axum::http::{HeaderValue, Request, Response, StatusCode};
use axum::middleware::Next;
use axum::response::IntoResponse as _;
use axum::Router;

/// ETag middleware configuration.
#[derive(Clone)]
pub struct EtagLayer {
    /// Biggest body to hash. A larger response passes through
    /// unchanged. Default 4 MiB; `None` means no cap.
    pub max_body_bytes: Option<usize>,
}

impl Default for EtagLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl EtagLayer {
    /// Hash responses up to 4 MiB.
    #[must_use]
    pub fn new() -> Self {
        Self {
            max_body_bytes: Some(4 * 1024 * 1024),
        }
    }

    /// Set the maximum body size. `None` removes the cap; be careful,
    /// every sized body is then buffered in memory.
    #[must_use]
    pub fn max_body_bytes(mut self, n: Option<usize>) -> Self {
        self.max_body_bytes = n;
        self
    }
}

/// Adds `.etag(layer)` to `Router`.
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
    // Read If-None-Match before the request is consumed.
    let client_etag = req
        .headers()
        .get(IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let response = next.run(req).await;
    let (parts, body) = response.into_parts();

    // Only 2xx responses get an ETag; a 206 body is a slice, not the representation.
    if !parts.status.is_success() || parts.status == StatusCode::PARTIAL_CONTENT {
        return Response::from_parts(parts, body);
    }
    // The handler's own validator wins (static files): no rehash, no buffering.
    if let Some(etag) = parts.headers.get(ETAG).and_then(|v| v.to_str().ok()) {
        if client_etag.is_some_and(|c| if_none_match(&c, etag)) {
            return not_modified(&Response::from_parts(parts, Body::empty()));
        }
        return Response::from_parts(parts, body);
    }

    // Decide from the size hint, before reading: a stream or an
    // over-cap body passes through intact instead of being blanked.
    let limit = cfg.max_body_bytes.unwrap_or(usize::MAX);
    let fits = axum::body::HttpBody::size_hint(&body)
        .upper()
        .is_some_and(|n| n <= limit as u64);
    if !fits {
        return Response::from_parts(parts, body);
    }
    let Ok(bytes) = to_bytes(body, limit).await else {
        // The stream failed mid-body; there is nothing honest to send.
        tracing::error!(target: "rustango::error", "etag: response body failed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    if bytes.is_empty() {
        return Response::from_parts(parts, Body::from(bytes));
    }

    let etag = compute_etag(&bytes);
    let mut response = Response::from_parts(parts, Body::from(bytes));
    if let Ok(v) = HeaderValue::from_str(&etag) {
        response.headers_mut().insert(ETAG, v);
    }

    if client_etag.is_some_and(|c| if_none_match(&c, &etag)) {
        return not_modified(&response);
    }

    response
}

/// Whether an `If-None-Match` value — a list of etags, or `*` — matches
/// `etag`. A conditional GET uses weak comparison (RFC 7232 §3.2).
pub(crate) fn if_none_match(client: &str, etag: &str) -> bool {
    let ours = normalize_etag(etag);
    client.trim() == "*"
        || client
            .split(',')
            .any(|candidate| normalize_etag(candidate) == ours)
}

/// Drop the body but keep the caching headers a 200 would have sent
/// (RFC 7232 §4.1), so a downstream cache keeps freshness and vary key.
fn not_modified(response: &Response<Body>) -> Response<Body> {
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
    not_modified
}

/// ETag for `bytes`: base64 of a 64-bit FNV-1a hash plus the length.
///
/// Not a cryptographic hash. A collision only means a wrong 304, and
/// hash plus length is good enough for cache validation.
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

/// Drop the quotes and any `W/` prefix so two etags can be compared.
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

    /// `If-None-Match: *` matches anything, so 304.
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

    /// An `If-None-Match` list that contains our etag must 304.
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

    /// The 304 must carry the caching headers, not just the ETag.
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

    /// The default caps buffering, and a body over the cap or of
    /// unknown length passes through whole instead of blanked (#1866).
    #[tokio::test]
    async fn large_and_streamed_bodies_pass_through_intact() {
        let big = "x".repeat(64);
        let big2 = big.clone();
        let app = Router::new()
            .route("/big", get(move || async move { big2 }))
            .route(
                "/stream",
                get(|| async { Body::new(Unsized(Some(axum::body::Bytes::from("chunk")))) }),
            )
            .etag(EtagLayer::default().max_body_bytes(Some(16)));
        assert_eq!(
            EtagLayer::default().max_body_bytes,
            EtagLayer::new().max_body_bytes
        );
        for (uri, want) in [("/big", big.as_str()), ("/stream", "chunk")] {
            let r = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::OK);
            assert!(r.headers().get(ETAG).is_none(), "{uri} was hashed");
            let body = to_bytes(r.into_body(), usize::MAX).await.unwrap();
            assert_eq!(&body[..], want.as_bytes(), "{uri} body was blanked");
        }
    }

    /// A one-chunk body with no size hint, like a stream or SSE.
    struct Unsized(Option<axum::body::Bytes>);

    impl axum::body::HttpBody for Unsized {
        type Data = axum::body::Bytes;
        type Error = std::convert::Infallible;
        fn poll_frame(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
            std::task::Poll::Ready(self.0.take().map(|b| Ok(http_body::Frame::data(b))))
        }
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
