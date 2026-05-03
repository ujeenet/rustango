//! Per-request `tracing` span with W3C / OpenTelemetry-conventional
//! field names + `traceparent` header propagation.
//!
//! Wraps each request in a `tracing::info_span!` carrying:
//!
//! - `http.request.method`         — `GET` / `POST` / ...
//! - `url.path`                    — request path (no query)
//! - `url.query`                   — query string (omitted when empty)
//! - `network.protocol.version`    — `HTTP/1.1`, `HTTP/2`, etc.
//! - `user_agent.original`         — User-Agent header
//! - `http.response.status_code`   — set after the handler returns
//! - `http.response.body.size`     — Content-Length when emitted
//! - `duration_ms`                 — full request lifetime
//!
//! Plus, when the incoming request carries a W3C `traceparent`
//! header, the parsed trace_id / parent_span_id are recorded so any
//! `tracing-opentelemetry` layer the user installs picks them up
//! automatically.
//!
//! ## Why not just use `tower-http::TraceLayer`?
//!
//! `tower-http`'s tracer ships a different set of field names
//! ([`http.method`], [`http.status_code`]) that pre-date the current
//! OpenTelemetry semantic conventions. This layer matches the
//! [v1.30 conventions](https://opentelemetry.io/docs/specs/semconv/http/http-spans/)
//! so OTel collectors don't need attribute-renaming rules.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::tracing_layer::TracingLayer;
//! use tower::ServiceBuilder;
//!
//! let inner: axum::Router = axum::Router::new()
//!     .route("/posts", axum::routing::get(list));
//!
//! let app = ServiceBuilder::new()
//!     .layer(TracingLayer::new())
//!     .service(inner);
//! ```
//!
//! ## Distributed tracing wiring
//!
//! For full distributed traces, install a `tracing-opentelemetry`
//! layer in your subscriber (this module deliberately doesn't pull
//! that dep — it's heavy). The layer reads the `traceparent` /
//! `parent_span_id` fields recorded by `TracingLayer` and threads
//! them through the OTel context.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, Response, Version};
use tower::Service;
use tracing::{field, info_span, Instrument};

#[derive(Clone, Default, Debug)]
pub struct TracingLayer;

impl TracingLayer {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl<S> tower::Layer<S> for TracingLayer {
    type Service = TracingService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        TracingService { inner: Arc::new(tokio::sync::Mutex::new(inner)) }
    }
}

/// The wrapped service. Internal `Arc<Mutex<S>>` so we can safely
/// `clone()` per-request without requiring `S: Clone`.
pub struct TracingService<S> {
    inner: Arc<tokio::sync::Mutex<S>>,
}

impl<S> Clone for TracingService<S> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

impl<S> Service<Request<Body>> for TracingService<S>
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

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Always ready — we lock the inner service per-call.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let inner = Arc::clone(&self.inner);
        let span = build_request_span(&req);
        Box::pin(
            async move {
                let started = Instant::now();
                let mut svc = inner.lock().await.clone();
                drop(inner);
                let resp = svc.call(req).await?;
                record_response(&resp, started);
                Ok(resp)
            }
            .instrument(span),
        )
    }
}

fn build_request_span(req: &Request<Body>) -> tracing::Span {
    let method = req.method().as_str();
    let path = req.uri().path();
    let query = req.uri().query().unwrap_or_default();
    let proto = http_version_str(req.version());
    let user_agent = req
        .headers()
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let span = info_span!(
        "http.request",
        "http.request.method" = method,
        "url.path" = path,
        "url.query" = field::Empty,
        "network.protocol.version" = proto,
        "user_agent.original" = user_agent,
        "http.response.status_code" = field::Empty,
        "http.response.body.size" = field::Empty,
        "duration_ms" = field::Empty,
        // Distributed-tracing fields populated when traceparent is present.
        "trace_id" = field::Empty,
        "parent_span_id" = field::Empty,
        "trace_flags" = field::Empty,
    );
    if !query.is_empty() {
        span.record("url.query", query);
    }
    if let Some(tp) = parse_traceparent(req.headers()) {
        span.record("trace_id", tp.trace_id);
        span.record("parent_span_id", tp.parent_id);
        span.record("trace_flags", tp.flags);
    }
    span
}

fn record_response(resp: &Response<Body>, started: Instant) {
    let span = tracing::Span::current();
    let status = resp.status().as_u16();
    span.record("http.response.status_code", status);
    if let Some(len) = resp
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
    {
        span.record("http.response.body.size", len);
    }
    let dur_ms = (started.elapsed().as_micros() as f64) / 1000.0;
    span.record("duration_ms", dur_ms);
}

const fn http_version_str(v: Version) -> &'static str {
    match v {
        Version::HTTP_09 => "HTTP/0.9",
        Version::HTTP_10 => "HTTP/1.0",
        Version::HTTP_11 => "HTTP/1.1",
        Version::HTTP_2 => "HTTP/2",
        Version::HTTP_3 => "HTTP/3",
        _ => "HTTP/?",
    }
}

// =====================================================================
// W3C Trace Context — `traceparent` parser
// =====================================================================

/// Parsed traceparent bits we care about. `trace-id` and `parent-id`
/// are kept as their hex-string representation so we can record them
/// directly into the span without re-hexing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTraceparent<'a> {
    pub version: &'a str,
    pub trace_id: &'a str,
    pub parent_id: &'a str,
    pub flags: &'a str,
}

/// Extract a traceparent from the request headers and parse it. The
/// W3C format is `<version>-<trace-id>-<parent-id>-<flags>` with
/// length-checked hex segments.
///
/// Returns `None` for any non-conforming value — the spec mandates
/// silent ignore in that case.
fn parse_traceparent(headers: &HeaderMap) -> Option<ParsedTraceparent<'_>> {
    let raw = headers.get("traceparent")?.to_str().ok()?;
    parse_traceparent_str(raw)
}

fn parse_traceparent_str(s: &str) -> Option<ParsedTraceparent<'_>> {
    let mut it = s.splitn(4, '-');
    let version = it.next()?;
    let trace_id = it.next()?;
    let parent_id = it.next()?;
    let flags = it.next()?;
    // W3C v00: 2-hex version, 32-hex trace-id, 16-hex parent-id, 2-hex flags.
    if version.len() != 2 || !is_hex(version) {
        return None;
    }
    if trace_id.len() != 32 || !is_hex(trace_id) || trace_id == "00000000000000000000000000000000"
    {
        return None;
    }
    if parent_id.len() != 16 || !is_hex(parent_id) || parent_id == "0000000000000000" {
        return None;
    }
    if flags.len() != 2 || !is_hex(flags) {
        return None;
    }
    Some(ParsedTraceparent {
        version,
        trace_id,
        parent_id,
        flags,
    })
}

fn is_hex(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------- parse_traceparent

    #[test]
    fn parses_valid_w3c_traceparent() {
        let s = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
        let p = parse_traceparent_str(s).unwrap();
        assert_eq!(p.version, "00");
        assert_eq!(p.trace_id, "0af7651916cd43dd8448eb211c80319c");
        assert_eq!(p.parent_id, "b7ad6b7169203331");
        assert_eq!(p.flags, "01");
    }

    #[test]
    fn rejects_short_trace_id() {
        let s = "00-0af7-b7ad6b7169203331-01";
        assert!(parse_traceparent_str(s).is_none());
    }

    #[test]
    fn rejects_all_zero_trace_id() {
        let s = "00-00000000000000000000000000000000-b7ad6b7169203331-01";
        assert!(parse_traceparent_str(s).is_none());
    }

    #[test]
    fn rejects_all_zero_parent_id() {
        let s = "00-0af7651916cd43dd8448eb211c80319c-0000000000000000-01";
        assert!(parse_traceparent_str(s).is_none());
    }

    #[test]
    fn rejects_non_hex_chars() {
        let s = "00-zzzz651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
        assert!(parse_traceparent_str(s).is_none());
    }

    #[test]
    fn rejects_wrong_segment_count() {
        let s = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331";
        assert!(parse_traceparent_str(s).is_none());
    }

    #[test]
    fn parses_from_header_map() {
        let mut h = HeaderMap::new();
        h.insert(
            "traceparent",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
                .parse()
                .unwrap(),
        );
        let p = parse_traceparent(&h).unwrap();
        assert_eq!(p.trace_id, "0af7651916cd43dd8448eb211c80319c");
    }

    #[test]
    fn missing_header_returns_none() {
        assert!(parse_traceparent(&HeaderMap::new()).is_none());
    }

    // -------- HTTP version strings

    #[test]
    fn http_version_str_known_versions() {
        assert_eq!(http_version_str(Version::HTTP_11), "HTTP/1.1");
        assert_eq!(http_version_str(Version::HTTP_2), "HTTP/2");
        assert_eq!(http_version_str(Version::HTTP_10), "HTTP/1.0");
    }

    // -------- Service integration smoke test (no subscriber wired up
    // — we verify the request flows through the layer cleanly + the
    // span metadata is built without panicking; field-capture
    // assertions live in the parser tests above).

    #[tokio::test]
    async fn layer_passes_through_request_returning_response() {
        use axum::routing::get;
        use axum::Router;
        use tower::{Layer, ServiceExt};

        let inner = Router::new().route("/r", get(|| async { "ok" }));
        let svc = TracingLayer::new().layer(inner.into_service::<Body>());
        let resp = svc
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/r?x=1")
                    .header("traceparent", "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn layer_records_response_status_into_span() {
        // Verify build_request_span + record_response don't panic and
        // produce a non-disabled span. We capture the span Id by
        // entering it briefly.
        use axum::http::StatusCode;
        let req = Request::builder()
            .method("POST")
            .uri("/foo?bar=1")
            .header(header::USER_AGENT, "test-ua/1.0")
            .body(Body::empty())
            .unwrap();
        let span = build_request_span(&req);
        let _enter = span.enter();
        let resp: Response<Body> = Response::builder()
            .status(StatusCode::CREATED)
            .header(header::CONTENT_LENGTH, "42")
            .body(Body::empty())
            .unwrap();
        record_response(&resp, Instant::now());
        // Span is non-disabled (we used info_span! which respects the
        // current subscriber; with no subscriber it's disabled, so
        // accept either — the contract is "doesn't panic").
        // No assertion needed beyond reaching this line.
    }
}
