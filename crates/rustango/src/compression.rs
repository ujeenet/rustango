//! Response compression middleware: gzip and deflate.
//!
//! A 2xx body is compressed only if all of these hold:
//! 1. `Accept-Encoding` lists an encoding we support.
//! 2. The body is at least `min_size_bytes` (default 1 KiB).
//! 3. The response has no `Content-Encoding` yet.
//! 4. The content type is not already-compressed binary (image, video,
//!    audio, zip, octet-stream, and so on).
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
//! Install [`crate::etag::EtagLayer`] first (closer to your handlers) so
//! the hash covers the uncompressed body. Otherwise it changes whenever
//! the compressor's output shifts.
//!
//! ## Security
//!
//! Compressing a response that mixes a secret (CSRF token, session id)
//! with attacker-controlled text leaks the secret through body size:
//! the BREACH/CRIME attack. Skip this layer on such responses, or send
//! `Cache-Control: no-transform`.
//!
//! ## Not compressed
//!
//! - `Cache-Control: no-transform` responses.
//! - SSE (`text/event-stream`) — the compressor buffers, which breaks
//!   live streaming.
//! - Bodies over `max_body_bytes`, since we buffer the whole body.

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
    /// Compression level 0..=9. Default: 4. Higher costs more CPU.
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

/// Adds `.compression(layer)` to `Router`.
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
        // Body too large, or the stream failed. We already consumed the
        // original, so all we can send is an empty body.
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
            // Compressor failed; we still hold the bytes, so send them raw.
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

/// Gzip raw bytes at zlib's default level (6).
///
/// Matches `django.utils.text.compress_string`. Takes bytes, not `&str`,
/// so binary payloads work too.
///
/// # Errors
/// Returns `std::io::Error` if the encoder fails, in practice only when
/// the output buffer cannot be allocated.
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

/// [`compress_string`] with an explicit level `0..=9`: `0` means no
/// compression, `1` is fastest and largest, `9` is slowest and smallest.
///
/// Pick `9` for assets you compress once and serve many times, `1`–`3`
/// on a hot request path, `6` otherwise. Values above `9` are clamped,
/// so out-of-range input never panics.
///
/// ```
/// use rustango::compression::{compress_string_with_level, decompress_string};
/// let payload = b"hello world".repeat(100);
/// let fast = compress_string_with_level(&payload, 1)?;
/// let best = compress_string_with_level(&payload, 9)?;
/// // Level 9 should be at least as small as level 1.
/// assert!(best.len() <= fast.len());
/// // Both decompress to the original.
/// assert_eq!(decompress_string(&fast)?, payload);
/// assert_eq!(decompress_string(&best)?, payload);
/// # Ok::<(), std::io::Error>(())
/// ```
///
/// # Errors
/// As [`compress_string`].
pub fn compress_string_with_level(bytes: &[u8], level: u32) -> std::io::Result<Vec<u8>> {
    encode(Encoding::Gzip, bytes, level.min(9))
}

/// Gunzip a buffer. The inverse of [`compress_string`].
///
/// # Errors
/// Returns `std::io::Error` for malformed gzip input: a truncated
/// stream, bad magic bytes, or a checksum mismatch.
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
    // Pick the first encoding we support whose q > 0. This is not full
    // RFC 7231 q sorting: our own preference order wins.
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

    // SSE: the compressor buffers, which breaks streaming.
    if main == "text/event-stream" {
        return true;
    }
    // Already-compressed media. JSON, CSS, JS, HTML and text still pass.
    if main.starts_with("image/") {
        // SVG is text; the rest are bitmaps or already compressed.
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
    // Vary on Accept-Encoding so caches keep compressed and
    // uncompressed responses apart.
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
                    // 2 KiB of repetitive JSON: over min_size and easy to shrink.
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
        // Both supported; the default order prefers gzip.
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
        // The original encoding stays; no double compression.
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

    // -------- compress_string / decompress_string --------

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
        // InvalidInput (truncated header) or UnexpectedEof (truncated body).
        // Both report the corruption instead of returning partial data.
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
        // NUL and high bytes prove nothing here assumes UTF-8.
        let original: Vec<u8> = (0u8..=255).collect();
        let compressed = compress_string(&original).unwrap();
        let restored = decompress_string(&compressed).unwrap();
        assert_eq!(restored, original);
    }

    // -------- compress_string_with_level --------

    #[test]
    fn compress_string_with_level_round_trips_at_all_levels() {
        let payload = b"the quick brown fox jumps over the lazy dog".repeat(50);
        for level in [0, 1, 3, 6, 9] {
            let compressed = compress_string_with_level(&payload, level).unwrap();
            let restored = decompress_string(&compressed).unwrap();
            assert_eq!(restored, payload, "round-trip failed at level {level}");
        }
    }

    #[test]
    fn compress_string_with_level_higher_levels_are_at_least_as_small() {
        // Very repetitive input makes the fast-vs-best gap visible.
        let payload = b"a".repeat(10_000);
        let fast = compress_string_with_level(&payload, 1).unwrap();
        let best = compress_string_with_level(&payload, 9).unwrap();
        assert!(
            best.len() <= fast.len(),
            "level 9 ({}) should be ≤ level 1 ({})",
            best.len(),
            fast.len()
        );
    }

    #[test]
    fn compress_string_with_level_clamps_out_of_range() {
        // Levels above 9 should clamp, not panic.
        let payload = b"hello world".to_vec();
        let compressed = compress_string_with_level(&payload, 99).unwrap();
        let restored = decompress_string(&compressed).unwrap();
        assert_eq!(restored, payload);
    }

    #[test]
    fn compress_string_with_level_matches_default_at_level_6() {
        // `compress_string` is level 6, so output must match exactly.
        let payload = b"hello world repeated content".repeat(10);
        let default = compress_string(&payload).unwrap();
        let explicit = compress_string_with_level(&payload, 6).unwrap();
        assert_eq!(default, explicit);
    }
}
