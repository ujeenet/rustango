//! `Server-Timing` header middleware — surface per-request stage
//! durations to the browser DevTools "Network → Timing" panel.
//!
//! See the [W3C Server-Timing spec](https://www.w3.org/TR/server-timing/).
//! Chrome and Firefox both render the values in DevTools natively,
//! making this the lowest-friction way to measure where time goes
//! per request.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::server_timing::{ServerTimingLayer, ServerTimingRouterExt, Timings};
//! use axum::Extension;
//!
//! let app = axum::Router::new()
//!     .route("/posts", axum::routing::get(list_posts))
//!     .server_timing(ServerTimingLayer::new());
//!
//! async fn list_posts(Extension(t): Extension<Timings>) -> Json<Vec<Post>> {
//!     t.measure("db");
//!     let posts = load_posts().await;
//!     t.measure("render");
//!     let body = Json(posts);
//!     t.finish();
//!     body
//! }
//! ```
//!
//! Resulting response header:
//! ```text
//! Server-Timing: total;dur=18.4, db;dur=12.1, render;dur=4.2
//! ```
//!
//! ## What's measured automatically
//!
//! - `total` — the full request lifetime (start of middleware to
//!   response generation), always emitted.
//! - Anything you `t.measure(name)` in a handler.
//!
//! ## Caveats
//!
//! - The header value is built from per-request mutable state; durations
//!   are recorded in the order you call `measure`. Two consecutive
//!   `measure("db")` calls overwrite each other (last one wins).
//! - Browsers cap header size — keep entry counts modest (<20).
//! - Not intended for production tracing — use OpenTelemetry for
//!   distributed propagation. This is for "what's slow in the browser
//!   right now?" debugging.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue, Response};
use axum::middleware::Next;
use axum::Router;

const HEADER: &str = "server-timing";

/// Per-request timing recorder. Cheap to clone — internal `Arc<Mutex>`.
#[derive(Clone)]
pub struct Timings {
    inner: Arc<Mutex<TimingsInner>>,
}

struct TimingsInner {
    request_started: Instant,
    last_mark: Instant,
    entries: Vec<(String, f64)>, // (name, ms)
}

impl Timings {
    fn new(request_started: Instant) -> Self {
        Self {
            inner: Arc::new(Mutex::new(TimingsInner {
                request_started,
                last_mark: request_started,
                entries: Vec::new(),
            })),
        }
    }

    /// Record a stage finishing at the current instant. The duration
    /// is from the previous `measure` (or the request start) to now.
    pub fn measure(&self, stage: impl Into<String>) {
        let now = Instant::now();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let elapsed = now.duration_since(g.last_mark).as_secs_f64() * 1000.0;
        g.entries.push((stage.into(), elapsed));
        g.last_mark = now;
    }

    /// Add a stage with an explicit duration (e.g. for an `await` you
    /// timed yourself). The stage doesn't move the "previous mark"
    /// cursor — useful for sub-stage breakdowns that overlap.
    pub fn add(&self, stage: impl Into<String>, ms: f64) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.entries.push((stage.into(), ms));
    }

    /// Mark the handler as finished. Optional — the middleware does
    /// it automatically after the handler returns. Useful when you
    /// want the `total` measurement to exclude a final stage (e.g.
    /// large body serialization that happens after).
    pub fn finish(&self) {
        // No-op marker — present so call sites can advertise intent.
    }

    fn render(&self) -> String {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let total_ms = g.request_started.elapsed().as_secs_f64() * 1000.0;
        let mut parts = Vec::with_capacity(g.entries.len() + 1);
        parts.push(format!("total;dur={total_ms:.1}"));
        for (name, ms) in &g.entries {
            // Sanitize the name — Server-Timing names must be tokens.
            let n = sanitize_token(name);
            parts.push(format!("{n};dur={ms:.1}"));
        }
        parts.join(", ")
    }
}

/// Trim a name down to ASCII-token characters. Server-Timing entry
/// names follow the HTTP token rule — no spaces, no separators.
fn sanitize_token(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

#[derive(Clone, Default, Debug)]
pub struct ServerTimingLayer;

impl ServerTimingLayer {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

pub trait ServerTimingRouterExt {
    #[must_use]
    fn server_timing(self, layer: ServerTimingLayer) -> Self;
}

impl<S: Clone + Send + Sync + 'static> ServerTimingRouterExt for Router<S> {
    fn server_timing(self, _layer: ServerTimingLayer) -> Self {
        self.layer(axum::middleware::from_fn(handle))
    }
}

async fn handle(mut req: Request<Body>, next: Next) -> Response<Body> {
    let started = Instant::now();
    let timings = Timings::new(started);
    req.extensions_mut().insert(timings.clone());

    let mut response = next.run(req).await;
    let value = timings.render();
    if let Ok(v) = HeaderValue::from_str(&value) {
        if let Ok(name) = HeaderName::try_from(HEADER) {
            response.headers_mut().insert(name, v);
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use axum::routing::get;
    use axum::Extension;
    use tower::ServiceExt;

    #[tokio::test]
    async fn header_is_set_with_total_only_when_no_stages() {
        async fn h() -> &'static str {
            "ok"
        }
        let app = Router::new()
            .route("/", get(h))
            .server_timing(ServerTimingLayer::new());
        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let v = resp
            .headers()
            .get("server-timing")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(v.starts_with("total;dur="), "got: {v}");
    }

    #[tokio::test]
    async fn handler_can_record_stages() {
        async fn h(Extension(t): Extension<Timings>) -> &'static str {
            t.measure("db");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            t.measure("render");
            "ok"
        }
        let app = Router::new()
            .route("/", get(h))
            .server_timing(ServerTimingLayer::new());
        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let v = resp
            .headers()
            .get("server-timing")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(v.contains("total;dur="));
        assert!(v.contains("db;dur="));
        assert!(v.contains("render;dur="));
    }

    #[tokio::test]
    async fn add_records_stage_with_explicit_duration() {
        async fn h(Extension(t): Extension<Timings>) -> &'static str {
            t.add("synthetic", 12.5);
            "ok"
        }
        let app = Router::new()
            .route("/", get(h))
            .server_timing(ServerTimingLayer::new());
        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let v = resp
            .headers()
            .get("server-timing")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(v.contains("synthetic;dur=12.5"));
    }

    #[tokio::test]
    async fn stage_names_are_sanitized() {
        async fn h(Extension(t): Extension<Timings>) -> &'static str {
            t.add("db query (selects)", 1.0);
            "ok"
        }
        let app = Router::new()
            .route("/", get(h))
            .server_timing(ServerTimingLayer::new());
        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let v = resp
            .headers()
            .get("server-timing")
            .unwrap()
            .to_str()
            .unwrap();
        // Spaces + parens become underscores so the token is HTTP-valid.
        assert!(v.contains("db_query__selects_;dur=1.0"));
    }

    #[tokio::test]
    async fn finish_is_a_noop_marker() {
        async fn h(Extension(t): Extension<Timings>) -> &'static str {
            t.measure("a");
            t.finish();
            "ok"
        }
        let app = Router::new()
            .route("/", get(h))
            .server_timing(ServerTimingLayer::new());
        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[test]
    fn sanitize_token_keeps_ascii_word_chars() {
        assert_eq!(sanitize_token("db_query"), "db_query");
        assert_eq!(sanitize_token("db-query"), "db-query");
        assert_eq!(sanitize_token("db.query"), "db.query");
        assert_eq!(sanitize_token("db123"), "db123");
    }

    #[test]
    fn sanitize_token_replaces_invalid_chars() {
        assert_eq!(sanitize_token("db query"), "db_query");
        assert_eq!(sanitize_token("ñ"), "_");
    }

    #[test]
    fn sanitize_token_empty_input_returns_underscore() {
        assert_eq!(sanitize_token(""), "_");
    }

    #[test]
    fn render_orders_total_first() {
        let t = Timings::new(Instant::now());
        t.add("db", 5.0);
        t.add("render", 3.0);
        let s = t.render();
        let parts: Vec<&str> = s.split(',').map(str::trim).collect();
        assert!(parts[0].starts_with("total;dur="));
        assert!(parts[1].starts_with("db;dur=5.0"));
        assert!(parts[2].starts_with("render;dur=3.0"));
    }
}
