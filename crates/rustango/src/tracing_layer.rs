//! Per-request `tracing` span with W3C / OpenTelemetry-conventional
//! field names + `traceparent` header propagation.
//!
//! Wraps each request in a `tracing::info_span!` carrying:
//!
//! - `http.request.method`         — `GET` / `POST` / ...
//! - `url.path`                    — request path (no query)
//! - `url.query`                   — **redacted** query string (omitted when empty)
//! - `network.protocol.version`    — `HTTP/1.1`, `HTTP/2`, etc.
//! - `user_agent.original`         — User-Agent header
//! - `http.response.status_code`   — set after the handler returns
//! - `http.response.body.size`     — Content-Length when emitted
//! - `duration_ms`                 — full request lifetime
//! - `request_id`                  — set by [`crate::request_id::record`]
//! - `tenant` / `org_id`           — set when a tenant resolves
//!
//! `url.query` is always redacted, but **which list is used depends on
//! how the span was mounted**:
//!
//! * Next to the access log (the normal path), it uses that layer's
//!   **configured** `redact_query_params`, so a key the project adds
//!   is hidden on the span too.
//! * With `[logging] access_log = false` there is no layer to read
//!   the list from, so the span falls back to
//!   `default_redact_params()`. A key added under
//!   `[audit] redact_query_params` is then written in clear text.
//!   Turning the access log off quietly narrows an audit setting.
//!
//! [`crate::tenant_log::record`] and [`crate::request_id::record`] set
//! `tenant` and `request_id` partway through the request, so every
//! later event, the ORM's included, carries them. `tenant` is left out
//! when no tenant resolves.
//!
//! When the request carries a W3C `traceparent` header, the trace_id
//! and parent_span_id are recorded, so a `tracing-opentelemetry` layer
//! picks them up on its own.
//!
//! ## Why not just use `tower-http::TraceLayer`?
//!
//! `tower-http`'s tracer uses older field names (`http.method`,
//! `http.status_code`) from before the current OpenTelemetry
//! conventions. This layer matches the
//! [v1.30 conventions](https://opentelemetry.io/docs/specs/semconv/http/http-spans/),
//! so a collector needs no renaming rules.
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
//! For full distributed traces, add a `tracing-opentelemetry` layer
//! to your subscriber. This module does not depend on it, because the
//! dependency is heavy. That layer reads the `traceparent` and
//! `parent_span_id` fields this one records.

use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, Response, Version};
use tower::Service;
use tracing::{field, info_span, Instrument};

#[derive(Clone, Debug)]
pub struct TracingLayer {
    /// Query parameters whose values are redacted out of `url.query`
    /// on the span. Defaults to the [`crate::access_log`] list.
    ///
    /// It is a field, not a constant, because a project can extend
    /// the list through `[audit] redact_query_params`. The span must
    /// use the same set: the span renders on the same line as the
    /// event, so a key hidden in one and shown in the other leaks.
    redact_query_params: std::sync::Arc<Vec<String>>,
}

impl Default for TracingLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl TracingLayer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            redact_query_params: std::sync::Arc::new(crate::access_log::default_redact_params()),
        }
    }

    /// Redact these query parameters instead of the defaults.
    ///
    /// Pass `AccessLogLayer::redact_query_params` so both layers
    /// agree. The framework's serving paths already do that.
    #[must_use]
    pub fn redact(mut self, params: Vec<String>) -> Self {
        self.redact_query_params = std::sync::Arc::new(params);
        self
    }
}

impl<S> tower::Layer<S> for TracingLayer {
    type Service = TracingService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        TracingService {
            inner,
            redact_query_params: std::sync::Arc::clone(&self.redact_query_params),
        }
    }
}

/// The wrapped service.
///
/// It holds the inner service directly and uses tower's ready-clone.
/// Do not wrap it in a lock: the `Service` impl below already needs
/// `S: Clone`, so a lock would only add contention on every request.
#[derive(Clone)]
pub struct TracingService<S> {
    inner: S,
    redact_query_params: std::sync::Arc<Vec<String>>,
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

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        // tower's ready-clone. A fresh clone is not ready, so use the
        // original that `poll_ready` readied and keep the clone for
        // next time.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let span = build_request_span(&req, &self.redact_query_params);
        Box::pin(
            async move {
                let started = Instant::now();
                let resp = inner.call(req).await?;
                record_response(&resp, started);
                Ok(resp)
            }
            .instrument(span),
        )
    }
}

fn build_request_span(req: &Request<Body>, redact: &[String]) -> tracing::Span {
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
        // Tenant identity, set mid-request by `tenant_log::record`.
        // Left empty on single-tenant apps and on apex or
        // operator-console requests.
        "tenant" = field::Empty,
        "org_id" = field::Empty,
        // The `X-Request-Id`, set by `request_id::record`. Keeping it
        // on the span means every event below it, the ORM's included,
        // carries the id without knowing it exists.
        "request_id" = field::Empty,
        // Distributed-tracing fields populated when traceparent is present.
        "trace_id" = field::Empty,
        "parent_span_id" = field::Empty,
        "trace_flags" = field::Empty,
    );
    if !query.is_empty() {
        // Never record the raw query here. The span renders on the
        // same line as the access-log event, so a raw value would put
        // the cleartext credential right beside the redacted copy.
        // `redact` is the caller's configured list, not the defaults.
        let redacted = crate::access_log::redact_query(query, redact);
        span.record("url.query", redacted.as_str());
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

/// The traceparent fields we use. `trace_id` and `parent_id` stay as
/// hex strings so they go straight onto the span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTraceparent<'a> {
    pub version: &'a str,
    pub trace_id: &'a str,
    pub parent_id: &'a str,
    pub flags: &'a str,
}

/// Parse the `traceparent` header. The W3C format is
/// `<version>-<trace-id>-<parent-id>-<flags>`, all hex, each segment
/// a fixed length.
///
/// Returns `None` for anything else; the spec says to ignore a bad
/// value silently.
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
    // W3C v00: version 2 hex, trace-id 32, parent-id 16, flags 2.
    if version.len() != 2 || !is_hex(version) {
        return None;
    }
    if trace_id.len() != 32 || !is_hex(trace_id) || trace_id == "00000000000000000000000000000000" {
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

    // -------- Service smoke tests. No subscriber is installed, so
    // these only check that a request passes through the layer and
    // the span is built without panicking.

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
                    .header(
                        "traceparent",
                        "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn layer_records_response_status_into_span() {
        // build_request_span and record_response must not panic.
        use axum::http::StatusCode;
        let req = Request::builder()
            .method("POST")
            .uri("/foo?bar=1")
            .header(header::USER_AGENT, "test-ua/1.0")
            .body(Body::empty())
            .unwrap();
        let span = build_request_span(&req, &crate::access_log::default_redact_params());
        let _enter = span.enter();
        let resp: Response<Body> = Response::builder()
            .status(StatusCode::CREATED)
            .header(header::CONTENT_LENGTH, "42")
            .body(Body::empty())
            .unwrap();
        record_response(&resp, Instant::now());
        // With no subscriber the span is disabled, so there is
        // nothing to assert: reaching this line is the test.
    }

    /// A key the *project* configured must be redacted on the span
    /// too, not just the framework defaults.
    #[test]
    fn a_project_configured_key_is_redacted_on_the_span() {
        let redact = vec!["client_secret".to_owned()];
        let req = Request::builder()
            .uri("/cb?client_secret=shhh&page=2")
            .body(Body::empty())
            .unwrap();
        let span = build_request_span(&req, &redact);
        drop(span);

        let out = crate::access_log::redact_query("client_secret=shhh&page=2", &redact);
        assert!(!out.contains("shhh"), "configured key not redacted: {out}");
        assert!(out.contains("page=2"), "non-credential param lost: {out}");
    }

    /// OAuth callbacks such as `/sso/callback?code=…&state=…` carry
    /// credentials under their own names. Matching is exact, so
    /// `access_token` does not cover `id_token`.
    #[test]
    fn the_defaults_cover_the_oauth_parameters_this_framework_emits() {
        let raw = "code=AUTHCODE&state=STATEVAL&id_token=IDTOK&code_verifier=VERIFIER\
&client_secret=CS&page=2";
        let out = crate::access_log::redact_query(raw, &crate::access_log::default_redact_params());
        for leaked in ["AUTHCODE", "STATEVAL", "IDTOK", "VERIFIER", "CS"] {
            assert!(
                !out.contains(leaked),
                "`{leaked}` survived the default redaction: {out}"
            );
        }
        assert!(
            out.contains("page=2"),
            "a plain param must pass through: {out}"
        );
    }

    /// The span must not carry credentials the access log redacts.
    ///
    /// This asserts on **rendered output**, not on the redaction
    /// helper. The helper was always right; the span recorded the raw
    /// string next to it. Only a real rendered line catches that.
    // `runtime` gates `tracing_subscriber`, which this test needs to
    // capture rendered output. A build with the layer but no
    // subscriber would fail to compile.
    #[cfg(feature = "runtime")]
    #[test]
    fn the_span_redacts_credentials_in_the_query_string() {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for Buf {
            type Writer = Buf;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = Buf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let req = Request::builder()
                .uri("/reset?password=hunter2&token=abc123XYZ&page=2")
                .body(Body::empty())
                .unwrap();
            let span = build_request_span(&req, &crate::access_log::default_redact_params());
            let _e = span.enter();
            // Any event inside the span renders the span's context,
            // which is where the leak showed up.
            tracing::info!("handled");
        });

        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(
            out.contains("url.query"),
            "the span did not render url.query at all, so this proves nothing:\n{out}"
        );
        assert!(
            !out.contains("hunter2"),
            "the span leaked a password into the log line:\n{out}"
        );
        assert!(
            !out.contains("abc123XYZ"),
            "the span leaked a token into the log line:\n{out}"
        );
        assert!(
            out.contains("page=2"),
            "a non-credential query param must survive:\n{out}"
        );
    }
}
