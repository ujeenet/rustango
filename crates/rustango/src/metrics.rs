//! Prometheus-format metrics — counters + histograms exposed at
//! `/metrics`.
//!
//! Pure-Rust, no Prometheus client crate. Sufficient for the 90%
//! case: a few labeled counters + a fixed-bucket histogram per
//! endpoint, scraped every 10–60 s.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::metrics::{MetricsRegistry, metrics_router};
//!
//! let reg = MetricsRegistry::new();
//!
//! // Wire the scrape endpoint.
//! let app = axum::Router::new()
//!     .merge(metrics_router(reg.clone()))
//!     .route("/api/posts", axum::routing::get(list_posts));
//!
//! // From handlers / background work, increment counters:
//! reg.counter("http_requests_total", &[("path", "/api/posts"), ("status", "200")])
//!    .inc();
//!
//! // Or observe latencies into a histogram with default buckets:
//! reg.histogram("http_request_duration_seconds", &[("path", "/api/posts")])
//!    .observe(0.024);
//! ```
//!
//! ## What's NOT here
//!
//! - Gauges that decrement over time (use a Counter and report the
//!   delta).
//! - Summary type. Histograms are nicer for aggregation across
//!   replicas; if you need quantiles, render them client-side.
//! - Push gateway. Prometheus pulls; we expose the endpoint.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// Default histogram bucket boundaries (seconds). Matches the
/// Prometheus convention for HTTP latency: covers 5 ms → 10 s with
/// reasonable resolution.
pub const DEFAULT_BUCKETS_S: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Shared registry of metrics. Cheap to clone — internal `Arc<RwLock>`.
#[derive(Clone, Default)]
pub struct MetricsRegistry {
    inner: Arc<RwLock<Inner>>,
}

#[derive(Default)]
struct Inner {
    counters: BTreeMap<MetricKey, Arc<CounterInner>>,
    histograms: BTreeMap<MetricKey, Arc<HistogramInner>>,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct MetricKey {
    name: String,
    labels: Vec<(String, String)>,
}

impl MetricKey {
    fn new(name: &str, labels: &[(&str, &str)]) -> Self {
        let mut sorted: Vec<(String, String)> = labels
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        sorted.sort();
        Self {
            name: name.to_owned(),
            labels: sorted,
        }
    }

    /// Render `name{k="v",k="v"}` for the text format. Labels are
    /// already sorted (by MetricKey::new). Bare metrics with no
    /// labels render without `{}`.
    fn render(&self) -> String {
        if self.labels.is_empty() {
            self.name.clone()
        } else {
            let mut s = String::with_capacity(self.name.len() + self.labels.len() * 16);
            s.push_str(&self.name);
            s.push('{');
            let mut first = true;
            for (k, v) in &self.labels {
                if !first {
                    s.push(',');
                }
                first = false;
                s.push_str(k);
                s.push('=');
                s.push('"');
                s.push_str(&escape_label_value(v));
                s.push('"');
            }
            s.push('}');
            s
        }
    }
}

fn escape_label_value(v: &str) -> String {
    // Prometheus escapes: `\` → `\\`, `"` → `\"`, newline → `\n`.
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => out.push_str(r"\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

impl MetricsRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Get-or-create a counter. Cheap to call on every request.
    pub fn counter(&self, name: &str, labels: &[(&str, &str)]) -> Counter {
        let key = MetricKey::new(name, labels);
        let inner = {
            let read = self.inner.read().unwrap_or_else(|e| e.into_inner());
            read.counters.get(&key).cloned()
        };
        if let Some(i) = inner {
            return Counter { inner: i };
        }
        let mut write = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let i = write
            .counters
            .entry(key)
            .or_insert_with(|| Arc::new(CounterInner::default()))
            .clone();
        Counter { inner: i }
    }

    /// Get-or-create a histogram with the default buckets.
    pub fn histogram(&self, name: &str, labels: &[(&str, &str)]) -> Histogram {
        self.histogram_with_buckets(name, labels, DEFAULT_BUCKETS_S)
    }

    /// Get-or-create a histogram with custom bucket upper bounds (in
    /// units that match what you `observe`). Buckets must be sorted
    /// ascending; unsorted input is sorted before use.
    pub fn histogram_with_buckets(
        &self,
        name: &str,
        labels: &[(&str, &str)],
        buckets: &[f64],
    ) -> Histogram {
        let key = MetricKey::new(name, labels);
        let inner = {
            let read = self.inner.read().unwrap_or_else(|e| e.into_inner());
            read.histograms.get(&key).cloned()
        };
        if let Some(i) = inner {
            return Histogram { inner: i };
        }
        let mut write = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let i = write
            .histograms
            .entry(key)
            .or_insert_with(|| Arc::new(HistogramInner::new(buckets)))
            .clone();
        Histogram { inner: i }
    }

    /// Render every metric in the Prometheus text exposition format.
    #[must_use]
    pub fn render(&self) -> String {
        let r = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let mut out = String::new();

        // Counters first, then histograms. Both groups: write a single
        // `# TYPE` line per metric name (NOT per labelset).
        let mut emitted_types = std::collections::HashSet::new();

        for (key, c) in &r.counters {
            if emitted_types.insert(("counter", key.name.clone())) {
                out.push_str(&format!("# TYPE {} counter\n", key.name));
            }
            out.push_str(&format!("{} {}\n", key.render(), c.value()));
        }

        for (key, h) in &r.histograms {
            if emitted_types.insert(("histogram", key.name.clone())) {
                out.push_str(&format!("# TYPE {} histogram\n", key.name));
            }
            // Bucket lines: <name>_bucket{le="x"} <count>
            let buckets = &h.buckets;
            let counts = h.bucket_counts();
            for (i, le) in buckets.iter().enumerate() {
                let mut bucket_labels = key.labels.clone();
                bucket_labels.push(("le".into(), format!("{le}")));
                bucket_labels.sort();
                let bucket_key = MetricKey {
                    name: format!("{}_bucket", key.name),
                    labels: bucket_labels,
                };
                out.push_str(&format!("{} {}\n", bucket_key.render(), counts[i]));
            }
            // +Inf bucket
            let mut inf_labels = key.labels.clone();
            inf_labels.push(("le".into(), "+Inf".into()));
            inf_labels.sort();
            let inf_key = MetricKey {
                name: format!("{}_bucket", key.name),
                labels: inf_labels,
            };
            out.push_str(&format!("{} {}\n", inf_key.render(), h.total_count()));

            // _sum and _count
            let sum_key = MetricKey {
                name: format!("{}_sum", key.name),
                labels: key.labels.clone(),
            };
            let count_key = MetricKey {
                name: format!("{}_count", key.name),
                labels: key.labels.clone(),
            };
            out.push_str(&format!("{} {}\n", sum_key.render(), h.sum()));
            out.push_str(&format!("{} {}\n", count_key.render(), h.total_count()));
        }

        out
    }
}

#[derive(Default)]
struct CounterInner {
    value: AtomicU64,
}

impl CounterInner {
    fn value(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }
}

#[derive(Clone)]
pub struct Counter {
    inner: Arc<CounterInner>,
}

impl Counter {
    pub fn inc(&self) {
        self.inner.value.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_by(&self, n: u64) {
        self.inner.value.fetch_add(n, Ordering::Relaxed);
    }
    #[must_use]
    pub fn value(&self) -> u64 {
        self.inner.value()
    }
}

struct HistogramInner {
    buckets: Vec<f64>,
    counts: Vec<AtomicU64>,
    /// Total observation count (matches the +Inf bucket per the
    /// Prometheus spec).
    total: AtomicU64,
    /// Sum of observed values, scaled by 1e6 to sit in u64 atomics.
    /// We expose it back as f64 in render().
    sum_micro: AtomicU64,
}

impl HistogramInner {
    fn new(buckets: &[f64]) -> Self {
        let mut sorted: Vec<f64> = buckets.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let counts = (0..sorted.len()).map(|_| AtomicU64::new(0)).collect();
        Self {
            buckets: sorted,
            counts,
            total: AtomicU64::new(0),
            sum_micro: AtomicU64::new(0),
        }
    }

    fn observe(&self, v: f64) {
        // Cumulative buckets (Prometheus convention): increment every
        // bucket whose `le` is >= v.
        for (i, &le) in self.buckets.iter().enumerate() {
            if v <= le {
                self.counts[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        self.total.fetch_add(1, Ordering::Relaxed);
        let micro = (v * 1_000_000.0).max(0.0) as u64;
        self.sum_micro.fetch_add(micro, Ordering::Relaxed);
    }

    fn bucket_counts(&self) -> Vec<u64> {
        self.counts
            .iter()
            .map(|a| a.load(Ordering::Relaxed))
            .collect()
    }
    fn total_count(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
    fn sum(&self) -> f64 {
        self.sum_micro.load(Ordering::Relaxed) as f64 / 1_000_000.0
    }
}

#[derive(Clone)]
pub struct Histogram {
    inner: Arc<HistogramInner>,
}

impl Histogram {
    pub fn observe(&self, v: f64) {
        self.inner.observe(v);
    }
    /// Time the closure and observe its duration in seconds.
    pub fn time<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let start = std::time::Instant::now();
        let r = f();
        self.observe(start.elapsed().as_secs_f64());
        r
    }
}

// =====================================================================
// axum endpoint
// =====================================================================

#[cfg(feature = "admin")]
pub fn metrics_router(reg: MetricsRegistry) -> axum::Router {
    use axum::extract::State;
    use axum::http::header;
    use axum::response::Response;
    use axum::routing::get;
    use std::sync::Arc as StdArc;

    async fn handler(State(reg): State<StdArc<MetricsRegistry>>) -> Response {
        let body = reg.render();
        Response::builder()
            .status(200)
            .header(
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )
            .body(axum::body::Body::from(body))
            .unwrap_or_else(|_| Response::new(axum::body::Body::empty()))
    }

    axum::Router::new()
        .route("/metrics", get(handler))
        .with_state(StdArc::new(reg))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------- Counter

    #[test]
    fn counter_increments() {
        let r = MetricsRegistry::new();
        let c = r.counter("hits", &[]);
        c.inc();
        c.inc();
        c.inc_by(5);
        assert_eq!(c.value(), 7);
    }

    #[test]
    fn same_label_set_returns_same_counter_instance() {
        let r = MetricsRegistry::new();
        r.counter("hits", &[("path", "/x")]).inc_by(3);
        r.counter("hits", &[("path", "/x")]).inc_by(4);
        assert_eq!(r.counter("hits", &[("path", "/x")]).value(), 7);
    }

    #[test]
    fn different_label_sets_are_independent() {
        let r = MetricsRegistry::new();
        r.counter("hits", &[("path", "/a")]).inc();
        r.counter("hits", &[("path", "/b")]).inc_by(5);
        assert_eq!(r.counter("hits", &[("path", "/a")]).value(), 1);
        assert_eq!(r.counter("hits", &[("path", "/b")]).value(), 5);
    }

    #[test]
    fn label_order_does_not_create_separate_metrics() {
        let r = MetricsRegistry::new();
        r.counter("hits", &[("a", "1"), ("b", "2")]).inc();
        r.counter("hits", &[("b", "2"), ("a", "1")]).inc();
        // Both should have hit the same counter.
        assert_eq!(r.counter("hits", &[("a", "1"), ("b", "2")]).value(), 2);
    }

    // -------- Histogram

    #[test]
    fn histogram_observe_increments_buckets_and_count() {
        let r = MetricsRegistry::new();
        let h = r.histogram_with_buckets("dur", &[], &[0.1, 1.0, 10.0]);
        h.observe(0.05); // bucket 0 + 1 + 2
        h.observe(0.5); //  bucket 1 + 2
        h.observe(5.0); //  bucket 2
        h.observe(20.0); // none — only +Inf
        assert_eq!(h.inner.total_count(), 4);
        let counts = h.inner.bucket_counts();
        assert_eq!(counts, vec![1, 2, 3]);
    }

    #[test]
    fn histogram_sum_reflects_observed_values() {
        let r = MetricsRegistry::new();
        let h = r.histogram_with_buckets("dur", &[], &[0.1, 1.0, 10.0]);
        h.observe(0.5);
        h.observe(0.75);
        // 0.5 + 0.75 = 1.25 (within the 1e-3 floor of micro precision)
        assert!((h.inner.sum() - 1.25).abs() < 0.001);
    }

    #[test]
    fn histogram_unsorted_buckets_are_sorted() {
        let r = MetricsRegistry::new();
        let h = r.histogram_with_buckets("dur", &[], &[10.0, 0.1, 1.0]);
        // Internally sorted to [0.1, 1.0, 10.0]
        assert_eq!(h.inner.buckets, vec![0.1, 1.0, 10.0]);
    }

    #[test]
    fn histogram_time_observes_elapsed() {
        let r = MetricsRegistry::new();
        let h = r.histogram("op_duration_seconds", &[]);
        h.time(|| {
            std::thread::sleep(std::time::Duration::from_millis(5));
        });
        assert_eq!(h.inner.total_count(), 1);
        // Sleep was at least 5 ms.
        assert!(h.inner.sum() >= 0.004);
    }

    // -------- text format

    #[test]
    fn render_emits_counter_lines() {
        let r = MetricsRegistry::new();
        r.counter("requests_total", &[("status", "200")]).inc_by(3);
        r.counter("requests_total", &[("status", "500")]).inc();
        let s = r.render();
        assert!(s.contains("# TYPE requests_total counter"));
        assert!(s.contains(r#"requests_total{status="200"} 3"#));
        assert!(s.contains(r#"requests_total{status="500"} 1"#));
        // Only ONE `# TYPE` line per metric name even with multiple
        // labelsets.
        assert_eq!(s.matches("# TYPE requests_total").count(), 1);
    }

    #[test]
    fn render_emits_bare_counter_without_braces() {
        let r = MetricsRegistry::new();
        r.counter("uptime_seconds", &[]).inc_by(42);
        let s = r.render();
        assert!(s.contains("uptime_seconds 42"));
    }

    #[test]
    fn render_emits_histogram_buckets_sum_count_and_inf() {
        let r = MetricsRegistry::new();
        let h = r.histogram_with_buckets("dur", &[("op", "ping")], &[0.1, 1.0]);
        h.observe(0.05);
        h.observe(0.5);
        let s = r.render();
        assert!(s.contains("# TYPE dur histogram"));
        // Bucket 0.1 saw the 0.05 observation only.
        assert!(
            s.contains(r#"dur_bucket{le="0.1",op="ping"} 1"#),
            "got: {s}"
        );
        // Bucket 1 saw both.
        assert!(s.contains(r#"dur_bucket{le="1",op="ping"} 2"#));
        // +Inf bucket equals total_count.
        assert!(s.contains(r#"dur_bucket{le="+Inf",op="ping"} 2"#));
        assert!(s.contains(r#"dur_count{op="ping"} 2"#));
        // sum = 0.55
        assert!(s.contains("dur_sum"), "got: {s}");
    }

    #[test]
    fn render_escapes_label_values() {
        let r = MetricsRegistry::new();
        r.counter("custom", &[("path", r#"/a"b\c"#)]).inc();
        let s = r.render();
        // " -> \", \ -> \\
        assert!(s.contains(r#"custom{path="/a\"b\\c"} 1"#), "got: {s}");
    }

    // -------- axum endpoint

    #[cfg(feature = "admin")]
    #[tokio::test]
    async fn metrics_endpoint_returns_text_format() {
        use axum::body::{to_bytes, Body};
        use axum::http::Request;
        use tower::ServiceExt;

        let r = MetricsRegistry::new();
        r.counter("hits", &[]).inc_by(7);
        let app = metrics_router(r);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap(),
            "text/plain; version=0.0.4; charset=utf-8"
        );
        let bytes = to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(body.contains("hits 7"));
    }
}
