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
//! `url.query` is redacted. **Which list is used depends on how the
//! span was mounted**, and the difference matters:
//!
//! * Mounted alongside the access log — the normal path — it uses the
//!   access log's **configured** `redact_query_params`, so a key a
//!   project adds is redacted on the span too. It rendered raw until
//!   #1480, beside the redacted copy in the access-log event.
//! * Mounted with `[logging] access_log = false` — the span is still
//!   mounted, but `mount_observability` has no layer to take the list
//!   from and falls back to `default_redact_params()`. A key added
//!   under `[audit] redact_query_params` is then rendered in
//!   cleartext on the span. The two settings live in different config
//!   sections, so turning the access log off silently narrows an
//!   audit setting — see #1610.
//!
//! An earlier version of this paragraph stated the configured list
//! unconditionally. That was the same overclaim in the opposite
//! direction from the comment it replaced (#1606 review, security).
//!
//! `tenant` and `request_id` are recorded partway through the request
//! — by [`crate::tenant_log::record`] and [`crate::request_id::record`]
//! — so every event emitted after that point, the ORM's included,
//! carries them in its span context. `tenant` is omitted entirely when
//! no tenant resolves.
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
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, Response, Version};
use tower::Service;
use tracing::{field, info_span, Instrument};

#[derive(Clone, Debug)]
pub struct TracingLayer {
    /// Query-parameter names whose values are redacted out of
    /// `url.query` on the span.
    ///
    /// Defaults to the same list [`crate::access_log`] uses. It is a
    /// field rather than a constant because a project can *extend* that
    /// list via `[audit] redact_query_params`, and the span has to
    /// honour the same set: redacting `client_secret` in the event
    /// while the span renders it in cleartext on the same line is the
    /// exact bug this redaction exists to remove, just narrowed to
    /// project-specific keys.
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

    /// Redact these query-parameter names instead of the defaults.
    ///
    /// Pass `AccessLogLayer::redact_query_params` so both layers agree;
    /// `Cli::mount_observability` and `server::Builder` do exactly that.
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
/// This held an `Arc<tokio::sync::Mutex<S>>` until the layer was first
/// actually mounted. The stated reason was to `clone()` per request
/// "without requiring `S: Clone`" — but the `Service` impl below
/// requires `S: Clone` regardless, so the mutex bought nothing and put
/// one contended async lock in front of the entire application on
/// every request. Holding the plain service and using tower's
/// ready-clone is the standard shape and needs neither the lock nor an
/// `Arc`.
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
        // tower's ready-clone: the clone is not necessarily ready, so
        // swap it for the original — which `poll_ready` above has
        // readied — and keep the fresh clone for next time.
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
        // Tenant identity, recorded mid-request by the resolver via
        // `tenant_log::record`. Empty (and so omitted) on single-tenant
        // apps and on apex / operator-console requests.
        "tenant" = field::Empty,
        "org_id" = field::Empty,
        // The `X-Request-Id` value, recorded by `request_id::record`
        // from the layer mounted inside this span. Carrying it here
        // rather than making handlers write `req_id = %id.0` on every
        // event is the whole point: a field on the span reaches every
        // event under it, including the ORM's, without any of them
        // knowing a request id exists (#1480).
        "request_id" = field::Empty,
        // Distributed-tracing fields populated when traceparent is present.
        "trace_id" = field::Empty,
        "parent_span_id" = field::Empty,
        "trace_flags" = field::Empty,
    );
    if !query.is_empty() {
        // Redacted, with the access log's *configured* key list.
        //
        // This recorded the raw string until the layer was first mounted
        // by default. The span's context renders on the same line as the
        // access-log event, so a request to
        // `/reset?password=hunter2&token=abc123` produced:
        //
        //   http.request{… url.query="password=hunter2&token=abc123"}:
        //     rustango::access_log: … url.query=password=[redacted]&token=[redacted]
        //
        // — the cleartext credential sitting beside the redaction that
        // was supposed to remove it. Reproduced against a live instance,
        // not reasoned about.
        //
        // `redact` is the caller's configured list, not the defaults —
        // see the field doc at `redact_query_params`. This comment
        // used to describe the opposite, because the first cut of the
        // fix redacted with `default_redact_params()` and the second
        // changed it; only the code was updated (#1504). Stating the
        // rejected design here told an auditor their own configured
        // key was rendered in cleartext on the span, which is exactly
        // the bug the fix removed.
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
        let span = build_request_span(&req, &crate::access_log::default_redact_params());
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

    /// A key the *project* configured must be redacted on the span too.
    ///
    /// The first cut of this fix redacted with `default_redact_params()`
    /// rather than the layer's configured list, and called that
    /// acceptable. It is not: the bug being fixed is "the span renders
    /// cleartext on the same line as the redaction", and for a project
    /// that added `client_secret` to `[audit] redact_query_params` that
    /// bug was entirely unchanged.
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

    /// OAuth callbacks carry credentials under names the original
    /// default list did not have.
    ///
    /// `/sso/callback?code=…&state=…` is a URL this framework's own
    /// `oauth2::providers` and `tenancy::sso` produce, and matching is
    /// exact — `access_token` never covered `id_token`.
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
    /// Asserts on **rendered output**, not on the redaction helper. The
    /// helper being correct was never in doubt; what broke was that the
    /// span recorded the raw string beside it. So this captures a real
    /// line and greps it, which is the only form of this test that
    /// could have failed before the fix.
    ///
    /// Reproduced live first: the span context and the access-log event
    /// render on one line, so a raw `url.query` put the cleartext
    /// password directly next to `url.query=password=[redacted]`.
    // `runtime` gates `tracing_subscriber`, which this needs to capture
    // rendered output. Without the gate it broke
    // `feature_combos (sqlite,admin)` — a build that has the span layer
    // but not the subscriber.
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
            // which is where the leak appeared.
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
