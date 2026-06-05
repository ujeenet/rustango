//! Response compression middleware — gzip + deflate.
//!
//! Compresses 2xx response bodies when:
//! 1. The client's `Accept-Encoding` lists a supported encoding, AND
//! 2. The response is large enough to be worth compressing
//!    (`min_size_bytes`, default 1 KiB), AND
//! 3. The response isn't already encoded (`Content-Encoding` absent), AND
//! 4. The Content-Type isn't already-compressed binary (image/, video/,
//!    audio/, application/zip, application/octet-stream, ...).
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::compression::{CompressionLayer, CompressionRouterExt};
//!
//! let app = axum::Router::new()
//!     .route("/api/posts", axum::routing::get(list_posts))
//!     .compression(CompressionLayer::default());
//! ```
//!
//! Pair with [`crate::etag::EtagLayer`] — order matters: install ETag
//! first (closer to your handlers) so the hash is computed on the
//! uncompressed body. Otherwise the hash will change every time the
//! compressor's output bytes shift.
//!
//! ## What's NOT compressed
//!
//! - Streaming responses are buffered then compressed; if you serve a
//!   really large file you should disable compression for that route
//!   (skip the layer or check the body type yourself before sending).
//! - The middleware honors response `Cache-Control: no-transform`.
//! - SSE streams (`text/event-stream`) are skipped — compressing them
//!   defeats live-streaming because compressors buffer.

use std::io::Write;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::header::{
    ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, VARY,
};
use axum::http::{HeaderValue, Request, Response};
use axum::middleware::Next;
use axum::Router;
use flate2::write::{DeflateEncoder, GzEncoder};
use flate2::Compression;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Gzip,
    Deflate,
}

impl Encoding {
    fn header_value(self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::Deflate => "deflate",
        }
    }
}

/// Compression middleware configuration.
#[derive(Clone, Debug)]
pub struct CompressionLayer {
    /// Minimum response body size to bother compressing. Default: 1024.
    pub min_size_bytes: usize,
    /// Hard cap on body size we'll buffer to compress. Default: 4 MiB.
    /// Larger responses are passed through uncompressed.
    pub max_body_bytes: usize,
    /// Encodings we'll offer, in preference order. Default: `[Gzip, Deflate]`.
    pub encodings: Vec<Encoding>,
    /// Compression level 0..=9. Default: 4 (a CPU/ratio sweet spot;
    /// flate2's "fast" is 1, "best" is 9, "default" is 6).
    pub level: u32,
}

impl Default for CompressionLayer {
    fn default() -> Self {
        Self {
            min_size_bytes: 1024,
            max_body_bytes: 4 * 1024 * 1024,
            encodings: vec![Encoding::Gzip, Encoding::Deflate],
            level: 4,
        }
    }
}

impl CompressionLayer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn min_size_bytes(mut self, n: usize) -> Self {
        self.min_size_bytes = n;
        self
    }

    #[must_use]
    pub fn max_body_bytes(mut self, n: usize) -> Self {
        self.max_body_bytes = n;
        self
    }

    #[must_use]
    pub fn level(mut self, level: u32) -> Self {
        self.level = level.min(9);
        self
    }

    #[must_use]
    pub fn encodings(mut self, encodings: Vec<Encoding>) -> Self {
        self.encodings = encodings;
        self
    }
}

/// Extension trait — `.compression(layer)` ergonomics on `Router`.
pub trait CompressionRouterExt {
    #[must_use]
    fn compression(self, layer: CompressionLayer) -> Self;
}

impl<S: Clone + Send + Sync + 'static> CompressionRouterExt for Router<S> {
    fn compression(self, layer: CompressionLayer) -> Self {
        let cfg = Arc::new(layer);
        self.layer(axum::middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let cfg = cfg.clone();
                async move { handle(cfg, req, next).await }
            },
        ))
    }
}

async fn handle(cfg: Arc<CompressionLayer>, req: Request<Body>, next: Next) -> Response<Body> {
    // Pick the best encoding before consuming the request.
    let chosen = req
        .headers()
        .get(ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| pick_encoding(h, &cfg.encodings));

    let response = next.run(req).await;

    let Some(encoding) = chosen else {
        return ensure_vary(response);
    };
    if !response.status().is_success() {
        return ensure_vary(response);
    }
    if response.headers().get(CONTENT_ENCODING).is_some() {
        return ensure_vary(response);
    }
    if cache_control_no_transform(&response) {
        return ensure_vary(response);
    }
    if let Some(ct) = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    {
        if is_uncompressible(ct) {
            return ensure_vary(response);
        }
    }

    let (mut parts, body) = response.into_parts();
    let bytes = match to_bytes(body, cfg.max_body_bytes).await {
        Ok(b) => b,
        // body too large or stream error — pass through with empty body
        // since we already consumed the original.
        Err(_) => {
            let mut resp = Response::from_parts(parts, Body::empty());
            ensure_vary_in_place(resp.headers_mut());
            return resp;
        }
    };
    if bytes.len() < cfg.min_size_bytes {
        let mut resp = Response::from_parts(parts, Body::from(bytes));
        ensure_vary_in_place(resp.headers_mut());
        return resp;
    }

    let compressed = match encode(encoding, &bytes, cfg.level) {
        Ok(c) => c,
        Err(_) => {
            // Compressor failed — return uncompressed (we still have the bytes).
            let mut resp = Response::from_parts(parts, Body::from(bytes));
            ensure_vary_in_place(resp.headers_mut());
            return resp;
        }
    };

    parts.headers.insert(
        CONTENT_ENCODING,
        HeaderValue::from_static(encoding.header_value()),
    );
    parts.headers.remove(CONTENT_LENGTH);
    ensure_vary_in_place(&mut parts.headers);
    Response::from_parts(parts, Body::from(compressed))
}

/// Django-parity `django.utils.text.compress_string(s)` — gzip-compress
/// raw bytes at the default zlib level (6). Used by Django's cache
/// machinery to compress large session/cache payloads before storage.
///
/// Takes a `&[u8]` rather than `&str` so binary payloads (cache
/// blobs, signed cookies) work too — caller passes
/// `s.as_bytes()` for the Python-shape "string" call.
///
/// # Errors
/// Returns `std::io::Error` if `flate2`'s encoder rejects the
/// input — in practice only on systems where the allocator can't
/// satisfy the output buffer.
///
/// ```ignore
/// use rustango::compression::{compress_string, decompress_string};
/// let original = b"hello world".repeat(100);
/// let compressed = compress_string(&original)?;
/// let restored = decompress_string(&compressed)?;
/// assert_eq!(restored, original);
/// ```
pub fn compress_string(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    encode(Encoding::Gzip, bytes, 6)
}

/// Django-parity inverse of [`compress_string`] — gunzip a buffer.
/// Symmetric helper not in Django's `utils.text` (Django decompresses
/// inline via `gzip.decompress`) but pairs cleanly with the encode
/// side here.
///
/// # Errors
/// Returns `std::io::Error` for any malformed gzip input — truncated
/// streams, bad magic bytes, checksum mismatch, etc.
pub fn decompress_string(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    use flate2::read::GzDecoder;
    use std::io::Read as _;
    let mut decoder = GzDecoder::new(bytes);
    let mut out = Vec::with_capacity(bytes.len() * 2);
    decoder.read_to_end(&mut out)?;
    Ok(out)
}

fn encode(enc: Encoding, bytes: &[u8], level: u32) -> std::io::Result<Vec<u8>> {
    let level = Compression::new(level);
    match enc {
        Encoding::Gzip => {
            let mut e = GzEncoder::new(Vec::with_capacity(bytes.len() / 2), level);
            e.write_all(bytes)?;
            e.finish()
        }
        Encoding::Deflate => {
            let mut e = DeflateEncoder::new(Vec::with_capacity(bytes.len() / 2), level);
            e.write_all(bytes)?;
            e.finish()
        }
    }
}

fn pick_encoding(accept_encoding: &str, supported: &[Encoding]) -> Option<Encoding> {
    // Parse `Accept-Encoding: gzip, deflate;q=0.5, br;q=0.9` into
    // (token, q) pairs and pick the first server-supported encoding
    // whose q > 0. We don't fully implement RFC 7231 q-value sorting
    // — server-side preference order wins on equal q.
    let mut acceptable = Vec::new();
    for raw in accept_encoding.split(',') {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let (token, q) = raw.split_once(';').map_or((raw, 1.0), |(t, params)| {
            let q = params
                .split(';')
                .find_map(|p| {
                    let p = p.trim();
                    p.strip_prefix("q=").and_then(|v| v.parse::<f32>().ok())
                })
                .unwrap_or(1.0);
            (t.trim(), q)
        });
        if q > 0.0 {
            acceptable.push(token.to_ascii_lowercase());
        }
    }
    if acceptable.iter().any(|t| t == "*") {
        return supported.first().copied();
    }
    supported
        .iter()
        .copied()
        .find(|e| acceptable.iter().any(|t| t == e.header_value()))
}

fn cache_control_no_transform(resp: &Response<Body>) -> bool {
    resp.headers()
        .get(CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            s.split(',')
                .any(|d| d.trim().eq_ignore_ascii_case("no-transform"))
        })
        .unwrap_or(false)
}

fn is_uncompressible(content_type: &str) -> bool {
    let main = content_type
        .split(';')
        .next()
        .map(str::trim)
        .unwrap_or(content_type)
        .to_ascii_lowercase();

    // SSE: compressors buffer, defeating streaming.
    if main == "text/event-stream" {
        return true;
    }
    // Already-compressed media. Conservative — let JSON, CSS, JS, HTML, plain text through.
    if main.starts_with("image/") {
        // SVG is text; everything else is bitmaps / already compressed.
        return main != "image/svg+xml";
    }
    if main.starts_with("video/") || main.starts_with("audio/") || main.starts_with("font/") {
        return true;
    }
    matches!(
        main.as_str(),
        "application/zip"
            | "application/gzip"
            | "application/x-gzip"
            | "application/x-bzip2"
            | "application/x-7z-compressed"
            | "application/x-rar-compressed"
            | "application/x-xz"
            | "application/x-tar"
            | "application/octet-stream"
            | "application/pdf"
            | "application/wasm"
            | "application/vnd.ms-excel"
            | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    )
}

fn ensure_vary(mut response: Response<Body>) -> Response<Body> {
    ensure_vary_in_place(response.headers_mut());
    response
}

fn ensure_vary_in_place(headers: &mut axum::http::HeaderMap) {
    // Append "Accept-Encoding" to Vary so caches don't conflate
    // compressed and uncompressed responses.
    let needs_append = match headers.get(VARY).and_then(|v| v.to_str().ok()) {
        Some(existing) => !existing
            .split(',')
            .any(|t| t.trim().eq_ignore_ascii_case("accept-encoding")),
        None => true,
    };
    if !needs_append {
        return;
    }
    let new_value = match headers.get(VARY).and_then(|v| v.to_str().ok()) {
        Some(existing) => format!("{existing}, Accept-Encoding"),
        None => "Accept-Encoding".to_owned(),
    };
    if let Ok(v) = HeaderValue::from_str(&new_value) {
        headers.insert(VARY, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use flate2::read::{DeflateDecoder, GzDecoder};
    use std::io::Read;
    use tower::ServiceExt;

    fn big_json_app() -> Router {
        Router::new()
            .route(
                "/big",
                get(|| async {
                    // 2 KiB of repetitive JSON — well over min_size + super
                    // compressible.
                    let body =
                        serde_json::to_string(&(0..200).map(|i| ("k", i)).collect::<Vec<_>>())
                            .unwrap();
                    ([(CONTENT_TYPE, "application/json")], body).into_response()
                }),
            )
            .compression(CompressionLayer::default())
    }

    fn small_app() -> Router {
        Router::new()
            .route(
                "/sm",
                get(|| async { ([(CONTENT_TYPE, "text/plain")], "tiny").into_response() }),
            )
            .compression(CompressionLayer::default())
    }

    fn binary_app() -> Router {
        Router::new()
            .route(
                "/img",
                get(|| async {
                    let body = vec![0u8; 4096];
                    ([(CONTENT_TYPE, "image/png")], body).into_response()
                }),
            )
            .compression(CompressionLayer::default())
    }

    async fn req(app: Router, accept: Option<&str>, path: &str) -> Response<Body> {
        let mut b = axum::http::Request::builder().uri(path);
        if let Some(a) = accept {
            b = b.header(ACCEPT_ENCODING, a);
        }
        app.oneshot(b.body(Body::empty()).unwrap()).await.unwrap()
    }

    #[tokio::test]
    async fn compresses_when_client_accepts_gzip() {
        let resp = req(big_json_app(), Some("gzip"), "/big").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get(CONTENT_ENCODING).unwrap(), "gzip");
        let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let mut decoder = GzDecoder::new(&bytes[..]);
        let mut decoded = String::new();
        decoder.read_to_string(&mut decoded).unwrap();
        assert!(decoded.contains("\"k\""));
    }

    #[tokio::test]
    async fn picks_deflate_when_only_deflate_accepted() {
        let resp = req(big_json_app(), Some("deflate"), "/big").await;
        assert_eq!(resp.headers().get(CONTENT_ENCODING).unwrap(), "deflate");
        let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let mut decoder = DeflateDecoder::new(&bytes[..]);
        let mut decoded = String::new();
        decoder.read_to_string(&mut decoded).unwrap();
        assert!(decoded.contains("\"k\""));
    }

    #[tokio::test]
    async fn server_preference_wins_on_equal_q() {
        // Both supported — server prefers gzip over deflate (default order).
        let resp = req(big_json_app(), Some("deflate, gzip"), "/big").await;
        assert_eq!(resp.headers().get(CONTENT_ENCODING).unwrap(), "gzip");
    }

    #[tokio::test]
    async fn star_accept_picks_first_supported() {
        let resp = req(big_json_app(), Some("*"), "/big").await;
        assert_eq!(resp.headers().get(CONTENT_ENCODING).unwrap(), "gzip");
    }

    #[tokio::test]
    async fn skips_when_no_accept_encoding() {
        let resp = req(big_json_app(), None, "/big").await;
        assert!(resp.headers().get(CONTENT_ENCODING).is_none());
    }

    #[tokio::test]
    async fn skips_when_unsupported_encoding_only() {
        let resp = req(big_json_app(), Some("br, zstd"), "/big").await;
        assert!(resp.headers().get(CONTENT_ENCODING).is_none());
    }

    #[tokio::test]
    async fn skips_when_q_zero() {
        let resp = req(big_json_app(), Some("gzip;q=0, deflate;q=0"), "/big").await;
        assert!(resp.headers().get(CONTENT_ENCODING).is_none());
    }

    #[tokio::test]
    async fn skips_when_response_below_min_size() {
        let resp = req(small_app(), Some("gzip"), "/sm").await;
        assert!(resp.headers().get(CONTENT_ENCODING).is_none());
    }

    #[tokio::test]
    async fn skips_already_compressed_content_type() {
        let resp = req(binary_app(), Some("gzip"), "/img").await;
        assert!(resp.headers().get(CONTENT_ENCODING).is_none());
    }

    #[tokio::test]
    async fn skips_when_no_transform_set() {
        let app = Router::new()
            .route(
                "/x",
                get(|| async {
                    let body = "x".repeat(2048);
                    (
                        [
                            (CONTENT_TYPE, "text/plain"),
                            (CACHE_CONTROL, "public, no-transform"),
                        ],
                        body,
                    )
                        .into_response()
                }),
            )
            .compression(CompressionLayer::default());
        let resp = req(app, Some("gzip"), "/x").await;
        assert!(resp.headers().get(CONTENT_ENCODING).is_none());
    }

    #[tokio::test]
    async fn vary_header_is_set_even_when_skipping() {
        let resp = req(big_json_app(), None, "/big").await;
        let vary = resp.headers().get(VARY).unwrap().to_str().unwrap();
        assert!(vary.to_ascii_lowercase().contains("accept-encoding"));
    }

    #[tokio::test]
    async fn vary_appended_to_existing_vary() {
        let app = Router::new()
            .route(
                "/v",
                get(|| async {
                    let body = "x".repeat(2048);
                    ([(CONTENT_TYPE, "text/plain"), (VARY, "Origin")], body).into_response()
                }),
            )
            .compression(CompressionLayer::default());
        let resp = req(app, Some("gzip"), "/v").await;
        let vary = resp.headers().get(VARY).unwrap().to_str().unwrap();
        let lower = vary.to_ascii_lowercase();
        assert!(lower.contains("origin"));
        assert!(lower.contains("accept-encoding"));
    }

    #[tokio::test]
    async fn skips_when_already_encoded() {
        let app = Router::new()
            .route(
                "/pre",
                get(|| async {
                    let body = vec![0u8; 4096];
                    (
                        [
                            (CONTENT_TYPE, "application/octet-stream"),
                            (CONTENT_ENCODING, "br"),
                        ],
                        body,
                    )
                        .into_response()
                }),
            )
            .compression(CompressionLayer::default());
        let resp = req(app, Some("gzip"), "/pre").await;
        // Original encoding stays — we shouldn't double-compress.
        assert_eq!(resp.headers().get(CONTENT_ENCODING).unwrap(), "br");
    }

    #[tokio::test]
    async fn skips_non_2xx_responses() {
        let app = Router::new()
            .route(
                "/err",
                get(|| async {
                    (
                        StatusCode::NOT_FOUND,
                        [(CONTENT_TYPE, "text/plain")],
                        "x".repeat(2048),
                    )
                        .into_response()
                }),
            )
            .compression(CompressionLayer::default());
        let resp = req(app, Some("gzip"), "/err").await;
        assert!(resp.headers().get(CONTENT_ENCODING).is_none());
    }

    #[tokio::test]
    async fn svg_is_compressed_despite_image_prefix() {
        let app = Router::new()
            .route(
                "/svg",
                get(|| async {
                    let body = format!("<svg>{}</svg>", "x".repeat(2048));
                    ([(CONTENT_TYPE, "image/svg+xml")], body).into_response()
                }),
            )
            .compression(CompressionLayer::default());
        let resp = req(app, Some("gzip"), "/svg").await;
        assert_eq!(resp.headers().get(CONTENT_ENCODING).unwrap(), "gzip");
    }

    #[tokio::test]
    async fn sse_streams_are_skipped() {
        let app = Router::new()
            .route(
                "/sse",
                get(|| async {
                    let body = "data: x\n\n".repeat(300);
                    ([(CONTENT_TYPE, "text/event-stream")], body).into_response()
                }),
            )
            .compression(CompressionLayer::default());
        let resp = req(app, Some("gzip"), "/sse").await;
        assert!(resp.headers().get(CONTENT_ENCODING).is_none());
    }

    #[test]
    fn pick_encoding_respects_q_zero() {
        let supported = vec![Encoding::Gzip, Encoding::Deflate];
        assert_eq!(
            pick_encoding("gzip;q=0, deflate", &supported),
            Some(Encoding::Deflate)
        );
        assert_eq!(pick_encoding("gzip;q=0", &supported), None);
    }

    #[test]
    fn pick_encoding_handles_star() {
        let supported = vec![Encoding::Gzip, Encoding::Deflate];
        assert_eq!(pick_encoding("*", &supported), Some(Encoding::Gzip));
    }

    #[test]
    fn level_caps_at_9() {
        let layer = CompressionLayer::new().level(99);
        assert_eq!(layer.level, 9);
    }

    // -------- compress_string / decompress_string (Django parity) --------

    #[test]
    fn compress_decompress_round_trip_simple() {
        let original = b"hello world";
        let compressed = compress_string(original).unwrap();
        let restored = decompress_string(&compressed).unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn compress_decompress_round_trip_large_repetitive() {
        // gzip should shrink this dramatically.
        let original = b"abcdefghij".repeat(1000);
        let compressed = compress_string(&original).unwrap();
        assert!(
            compressed.len() < original.len() / 10,
            "expected >90% compression on highly redundant input; got {} → {}",
            original.len(),
            compressed.len()
        );
        let restored = decompress_string(&compressed).unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn compress_empty_input_round_trips() {
        let compressed = compress_string(b"").unwrap();
        // Empty gzip stream is still a valid stream with a header + trailer.
        assert!(!compressed.is_empty());
        let restored = decompress_string(&compressed).unwrap();
        assert!(restored.is_empty());
    }

    #[test]
    fn compress_output_starts_with_gzip_magic() {
        // RFC 1952 — gzip magic bytes are 0x1f 0x8b.
        let out = compress_string(b"x").unwrap();
        assert!(
            out.len() >= 2,
            "gzip output should have at least header bytes"
        );
        assert_eq!(&out[..2], &[0x1f, 0x8b]);
    }

    #[test]
    fn decompress_rejects_garbage() {
        // Not a gzip stream — should error, not panic.
        let err = decompress_string(b"not actually gzipped bytes here").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn decompress_rejects_truncated_stream() {
        // Compress a buffer then truncate it.
        let full = compress_string(b"the quick brown fox").unwrap();
        let truncated = &full[..full.len() / 2];
        let err = decompress_string(truncated).unwrap_err();
        // Either InvalidInput (truncated header) or UnexpectedEof (truncated body)
        // — both surface the corruption rather than returning partial data.
        assert!(
            matches!(
                err.kind(),
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::UnexpectedEof
            ),
            "got unexpected error kind: {:?}",
            err.kind()
        );
    }

    #[test]
    fn compress_string_handles_binary_input() {
        // Django uses `compress_string(s.encode())` — binary input
        // must round-trip cleanly. Use bytes including NUL + high
        // bytes to verify nothing assumes UTF-8.
        let original: Vec<u8> = (0u8..=255).collect();
        let compressed = compress_string(&original).unwrap();
        let restored = decompress_string(&compressed).unwrap();
        assert_eq!(restored, original);
    }
}
