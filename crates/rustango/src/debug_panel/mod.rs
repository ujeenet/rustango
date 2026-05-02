//! Debug profiling panel — `/__debug__/` for development.
//!
//! Inspired by Django Debug Toolbar / Laravel Telescope. Captures
//! per-request telemetry (SQL queries + durations, cache hits, signals,
//! response time) and serves a UI to inspect them.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::debug_panel::{DebugPanelLayer, DebugPanelRouterExt, debug_router};
//!
//! let app = Router::new()
//!     .route("/api/...", ...)
//!     .merge(debug_router())                 // serves /__debug__/* — DEV ONLY
//!     .debug_panel();                        // captures per-request data
//!
//! // Browse http://localhost:8080/__debug__/
//! ```
//!
//! ## What gets captured
//!
//! Per request (keyed by request_id):
//! - **method, path, status, duration_ms**
//! - **query_count + total_query_ms** (when paired with sqlx logging)
//! - **cache_hits, cache_misses**
//! - **signals_fired**
//! - **timestamp**
//!
//! The capture buffer is in-memory and bounded (default: 100 most recent).
//!
//! ## Production warning
//!
//! **DO NOT mount in production** — the panel exposes request paths,
//! query patterns, and timing data that an attacker could use for
//! reconnaissance. Gate the mount on `RUSTANGO_ENV != "prod"`:
//!
//! ```ignore
//! let app = Router::new().route(...);
//! let app = if std::env::var("RUSTANGO_ENV").as_deref() != Ok("prod") {
//!     app.merge(debug_router()).debug_panel()
//! } else {
//!     app
//! };
//! ```

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use axum::body::Body;
use axum::extract::Request;
use axum::http::Response;
use axum::middleware::Next;
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use tokio::sync::RwLock;

const DEFAULT_BUFFER_SIZE: usize = 100;

// ------------------------------------------------------------------ Capture

/// Per-request telemetry record.
#[derive(Debug, Clone)]
pub struct RequestRecord {
    pub request_id: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub duration_ms: u64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub query_count: usize,
    pub total_query_ms: u64,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub signals_fired: usize,
}

/// Shared in-memory capture buffer.
#[derive(Default)]
struct Buffer {
    records: VecDeque<RequestRecord>,
    capacity: usize,
}

impl Buffer {
    fn new(capacity: usize) -> Self {
        Self {
            records: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, rec: RequestRecord) {
        if self.records.len() >= self.capacity {
            self.records.pop_front();
        }
        self.records.push_back(rec);
    }
}

/// Shared state — wrap in Arc to clone across handlers.
#[derive(Clone, Default)]
pub struct DebugPanelState {
    buffer: Arc<RwLock<Buffer>>,
    /// Counter — increment from your code via `state.bump_query()`, etc.
    pub queries: Arc<AtomicUsize>,
    pub cache_hits: Arc<AtomicUsize>,
    pub cache_misses: Arc<AtomicUsize>,
    pub signals: Arc<AtomicUsize>,
}

impl DebugPanelState {
    /// New state with default 100-record buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_BUFFER_SIZE)
    }

    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buffer: Arc::new(RwLock::new(Buffer::new(capacity))),
            ..Self::default()
        }
    }

    /// Manually push a record (used by the middleware).
    pub async fn push(&self, rec: RequestRecord) {
        self.buffer.write().await.push(rec);
    }

    /// Snapshot of all current records (most recent last).
    pub async fn records(&self) -> Vec<RequestRecord> {
        self.buffer.read().await.records.iter().cloned().collect()
    }

    /// Number of records currently held.
    pub async fn len(&self) -> usize {
        self.buffer.read().await.records.len()
    }

    pub fn bump_query(&self) {
        self.queries.fetch_add(1, Ordering::SeqCst);
    }

    pub fn bump_cache_hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::SeqCst);
    }

    pub fn bump_cache_miss(&self) {
        self.cache_misses.fetch_add(1, Ordering::SeqCst);
    }

    pub fn bump_signal(&self) {
        self.signals.fetch_add(1, Ordering::SeqCst);
    }
}

// ------------------------------------------------------------------ Layer

/// Configuration for the debug-panel middleware.
#[derive(Clone)]
pub struct DebugPanelLayer {
    state: DebugPanelState,
}

impl DebugPanelLayer {
    #[must_use]
    pub fn new() -> Self {
        Self { state: DebugPanelState::new() }
    }

    #[must_use]
    pub fn with_state(state: DebugPanelState) -> Self {
        Self { state }
    }

    #[must_use]
    pub fn state(&self) -> &DebugPanelState {
        &self.state
    }
}

impl Default for DebugPanelLayer {
    fn default() -> Self {
        Self::new()
    }
}

/// Extension trait — `.debug_panel()` on Router.
pub trait DebugPanelRouterExt {
    #[must_use]
    fn debug_panel(self) -> Self;

    #[must_use]
    fn debug_panel_with(self, layer: DebugPanelLayer) -> Self;
}

impl<S: Clone + Send + Sync + 'static> DebugPanelRouterExt for Router<S> {
    fn debug_panel(self) -> Self {
        self.debug_panel_with(DebugPanelLayer::new())
    }

    fn debug_panel_with(self, layer: DebugPanelLayer) -> Self {
        let state = layer.state.clone();
        self.layer(axum::middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let state = state.clone();
                async move { capture(state, req, next).await }
            },
        ))
        .layer(axum::Extension(layer.state))
    }
}

async fn capture(
    state: DebugPanelState,
    req: Request<Body>,
    next: Next,
) -> Response<Body> {
    let started = Instant::now();
    let method = req.method().to_string();
    let path = req.uri().path().to_owned();
    let request_id = req
        .extensions()
        .get::<crate::request_id::RequestId>()
        .map_or_else(|| "-".to_owned(), |id| id.0.clone());

    // Snapshot counters before
    let q_before = state.queries.load(Ordering::SeqCst);
    let h_before = state.cache_hits.load(Ordering::SeqCst);
    let m_before = state.cache_misses.load(Ordering::SeqCst);
    let s_before = state.signals.load(Ordering::SeqCst);

    let response = next.run(req).await;

    let q_after = state.queries.load(Ordering::SeqCst);
    let h_after = state.cache_hits.load(Ordering::SeqCst);
    let m_after = state.cache_misses.load(Ordering::SeqCst);
    let s_after = state.signals.load(Ordering::SeqCst);

    let rec = RequestRecord {
        request_id,
        method,
        path,
        status: response.status().as_u16(),
        duration_ms: started.elapsed().as_millis() as u64,
        timestamp: chrono::Utc::now(),
        query_count: q_after.saturating_sub(q_before),
        total_query_ms: 0, // not yet wired to sqlx logging
        cache_hits: h_after.saturating_sub(h_before),
        cache_misses: m_after.saturating_sub(m_before),
        signals_fired: s_after.saturating_sub(s_before),
    };
    state.push(rec).await;
    response
}

// ------------------------------------------------------------------ Router

/// Build the debug panel router. Mount under `/__debug__/`.
#[must_use]
pub fn debug_router() -> Router {
    Router::new()
        .route("/__debug__/", get(panel_index))
        .route("/__debug__/data", get(panel_json))
}

async fn panel_index(
    axum::Extension(state): axum::Extension<DebugPanelState>,
) -> Html<String> {
    let records = state.records().await;
    Html(render_panel(&records))
}

async fn panel_json(
    axum::Extension(state): axum::Extension<DebugPanelState>,
) -> axum::Json<Vec<serde_json::Value>> {
    let records = state.records().await;
    let json: Vec<_> = records.iter().map(record_to_json).collect();
    axum::Json(json)
}

fn record_to_json(r: &RequestRecord) -> serde_json::Value {
    serde_json::json!({
        "request_id": r.request_id,
        "method": r.method,
        "path": r.path,
        "status": r.status,
        "duration_ms": r.duration_ms,
        "timestamp": r.timestamp.to_rfc3339(),
        "query_count": r.query_count,
        "total_query_ms": r.total_query_ms,
        "cache_hits": r.cache_hits,
        "cache_misses": r.cache_misses,
        "signals_fired": r.signals_fired,
    })
}

fn render_panel(records: &[RequestRecord]) -> String {
    let mut rows = String::new();
    for r in records.iter().rev() {
        let status_class = match r.status {
            200..=299 => "ok",
            300..=399 => "redir",
            400..=499 => "client-err",
            _ => "server-err",
        };
        let slow_class = if r.duration_ms >= 500 { "slow" } else { "" };
        rows.push_str(&format!(
            r#"<tr>
                <td class="ts">{ts}</td>
                <td class="method">{method}</td>
                <td class="path">{path}</td>
                <td class="status {status_class}">{status}</td>
                <td class="dur {slow_class}">{dur}ms</td>
                <td>{q}</td>
                <td>{ch}/{cm}</td>
                <td>{sg}</td>
                <td class="rid">{rid}</td>
            </tr>"#,
            ts = r.timestamp.format("%H:%M:%S%.3f"),
            method = r.method,
            path = html_escape(&r.path),
            status = r.status,
            dur = r.duration_ms,
            q = r.query_count,
            ch = r.cache_hits,
            cm = r.cache_misses,
            sg = r.signals_fired,
            rid = html_escape(&r.request_id),
        ));
    }
    if rows.is_empty() {
        rows.push_str(r#"<tr><td colspan="9" class="empty">No requests captured yet — make some requests and refresh.</td></tr>"#);
    }
    let count = records.len();
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>rustango — debug panel</title>
<style>
:root {{ color-scheme: light dark; --slow: #d04040; --ok: #38a169; --err: #d04040; }}
body {{ font-family: ui-sans-serif, system-ui, -apple-system, sans-serif;
       margin: 1.5rem; line-height: 1.5; }}
h1 {{ margin: 0 0 .25rem; font-weight: 700; letter-spacing: -.01em; }}
.tag {{ color: #888; font-size: .9rem; margin-bottom: 1.5rem; }}
.warn {{ background: rgba(255, 200, 100, .15); border-left: 3px solid #d4a04a;
         padding: .5rem 1rem; margin-bottom: 1.5rem; font-size: .9rem; }}
table {{ width: 100%; border-collapse: collapse; font-size: .85rem; }}
th, td {{ text-align: left; padding: .35rem .75rem; border-bottom: 1px solid rgba(127,127,127,.2); }}
th {{ font-weight: 600; background: rgba(127,127,127,.08); position: sticky; top: 0; }}
tbody tr:hover {{ background: rgba(127,127,127,.06); }}
.ts {{ font-family: ui-monospace, Menlo, monospace; color: #888; white-space: nowrap; }}
.method {{ font-family: ui-monospace; font-weight: 600; }}
.path {{ font-family: ui-monospace; max-width: 400px; overflow: hidden; text-overflow: ellipsis; }}
.status {{ font-family: ui-monospace; text-align: right; }}
.status.ok {{ color: var(--ok); }}
.status.client-err, .status.server-err {{ color: var(--err); }}
.dur {{ font-family: ui-monospace; text-align: right; }}
.dur.slow {{ color: var(--slow); font-weight: 600; }}
.rid {{ font-family: ui-monospace; color: #999; font-size: .75rem; }}
.empty {{ text-align: center; color: #999; padding: 2rem; }}
.refresh {{ float: right; padding: .35rem .75rem; background: rgba(127,127,127,.15);
            border-radius: 4px; cursor: pointer; border: none; font: inherit; }}
</style>
</head>
<body>
<h1>rustango debug panel</h1>
<p class="tag">{count} request{plural} captured · auto-refresh every 5s</p>
<div class="warn"><strong>Dev only.</strong> Don't mount in production — exposes request patterns + timing data.</div>
<button class="refresh" onclick="location.reload()">Refresh now</button>
<table>
<thead>
<tr>
  <th>Time</th><th>Method</th><th>Path</th><th>Status</th><th>Dur</th>
  <th>Queries</th><th>Cache (h/m)</th><th>Signals</th><th>Request ID</th>
</tr>
</thead>
<tbody>{rows}</tbody>
</table>
<script>setTimeout(() => location.reload(), 5000);</script>
</body>
</html>"#,
        count = count,
        plural = if count == 1 { "" } else { "s" },
        rows = rows,
    )
}

fn html_escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".to_owned(),
            '<' => "&lt;".to_owned(),
            '>' => "&gt;".to_owned(),
            '"' => "&quot;".to_owned(),
            '\'' => "&#x27;".to_owned(),
            _ => c.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn buffer_evicts_oldest_when_full() {
        let state = DebugPanelState::with_capacity(3);
        for i in 0..5 {
            state.push(RequestRecord {
                request_id: format!("r{i}"),
                method: "GET".into(),
                path: "/".into(),
                status: 200,
                duration_ms: 0,
                timestamp: chrono::Utc::now(),
                query_count: 0,
                total_query_ms: 0,
                cache_hits: 0,
                cache_misses: 0,
                signals_fired: 0,
            }).await;
        }
        let records = state.records().await;
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].request_id, "r2");
        assert_eq!(records[2].request_id, "r4");
    }

    #[tokio::test]
    async fn bumpers_increment_counters() {
        let state = DebugPanelState::new();
        state.bump_query();
        state.bump_query();
        state.bump_cache_hit();
        state.bump_cache_miss();
        state.bump_cache_miss();
        state.bump_signal();
        assert_eq!(state.queries.load(Ordering::SeqCst), 2);
        assert_eq!(state.cache_hits.load(Ordering::SeqCst), 1);
        assert_eq!(state.cache_misses.load(Ordering::SeqCst), 2);
        assert_eq!(state.signals.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn render_panel_empty_shows_message() {
        let html = render_panel(&[]);
        assert!(html.contains("No requests captured"));
    }

    #[test]
    fn render_panel_includes_record_data() {
        let r = RequestRecord {
            request_id: "abc123".into(),
            method: "POST".into(),
            path: "/api/posts".into(),
            status: 201,
            duration_ms: 42,
            timestamp: chrono::Utc::now(),
            query_count: 3,
            total_query_ms: 0,
            cache_hits: 1,
            cache_misses: 2,
            signals_fired: 0,
        };
        let html = render_panel(&[r]);
        assert!(html.contains("/api/posts"));
        assert!(html.contains("POST"));
        assert!(html.contains("201"));
        assert!(html.contains("42ms"));
        assert!(html.contains("abc123"));
    }

    #[test]
    fn render_panel_marks_slow_request() {
        let r = RequestRecord {
            request_id: "x".into(),
            method: "GET".into(),
            path: "/slow".into(),
            status: 200,
            duration_ms: 800,
            timestamp: chrono::Utc::now(),
            query_count: 0,
            total_query_ms: 0,
            cache_hits: 0,
            cache_misses: 0,
            signals_fired: 0,
        };
        let html = render_panel(&[r]);
        assert!(html.contains("class=\"dur slow\""));
    }

    #[test]
    fn render_panel_html_escapes_path() {
        let r = RequestRecord {
            request_id: "x".into(),
            method: "GET".into(),
            path: "/<script>alert(1)</script>".into(),
            status: 200,
            duration_ms: 1,
            timestamp: chrono::Utc::now(),
            query_count: 0,
            total_query_ms: 0,
            cache_hits: 0,
            cache_misses: 0,
            signals_fired: 0,
        };
        let html = render_panel(&[r]);
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(html.contains("&lt;script&gt;"));
    }
}
