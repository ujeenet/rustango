//! Health check endpoints — `/health` (liveness) and `/ready` (readiness).
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::health::health_router;
//! use axum::Router;
//!
//! let app = Router::new()
//!     .merge(health_router(pool.clone()))
//!     .route("/api/posts", ...);
//! ```
//!
//! ## Endpoints
//!
//! - **`GET /health`** — liveness. Returns `200 OK` with `{"status":"ok"}`.
//!   Always succeeds — used by orchestrators to detect process crashes.
//! - **`GET /ready`** — readiness. Pings the database with `SELECT 1`,
//!   plus any extra probes you've registered. Returns `200 OK` only
//!   when every probe succeeds; otherwise `503 Service Unavailable`.
//!
//! Each probe runs under a per-check timeout (default 5s) so a single
//! hanging downstream can't pin the whole readiness handler. Each probe's
//! latency is reported in the response so you can spot creeping slowness
//! before it becomes a failure.
//!
//! ## Custom checks
//!
//! Add additional checks (Redis, S3, external APIs) via [`HealthRouter::check`]:
//!
//! ```ignore
//! use rustango::health::HealthRouter;
//!
//! let app = HealthRouter::new(pool.clone())
//!     .timeout(Duration::from_secs(2))
//!     .check("redis", || async {
//!         redis_ping().await.map_err(|e| e.to_string())
//!     })
//!     .into_router();
//! ```
//!
//! ## Built-in probes
//!
//! When the corresponding feature is on, factory helpers wire common
//! probes for you:
//!
//! - [`HealthRouter::cache_probe`] — `Cache::set`/`get` round-trip
//!   (requires `cache`).
//! - [`HealthRouter::http_probe`] — GET a URL, checking it returns 2xx
//!   (requires `http-client`).
//! - [`HealthRouter::tcp_probe`] — open a TCP connection. Always
//!   available.
//!
//! ## Sample response
//!
//! ```json
//! {
//!   "status": "ok",
//!   "checks": {
//!     "database": { "status": "ok", "latency_ms": 4 },
//!     "redis":    { "status": "ok", "latency_ms": 2 }
//!   }
//! }
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::sql::sqlx::{self, PgPool};

/// Async health-check function: returns `Ok(())` when healthy,
/// `Err(message)` otherwise. Each check has a name shown in the JSON
/// response.
pub type CheckFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send + Sync>;

/// Builder for the health router.
pub struct HealthRouter {
    pool: PgPool,
    extra_checks: Vec<(String, CheckFn)>,
    per_check_timeout: Duration,
    /// Should the built-in `database` probe run? Set to false if your
    /// app has a non-Postgres backend or you want to register your own
    /// DB probe with a different connectivity sentinel.
    include_db_probe: bool,
}

#[derive(Clone)]
struct HealthState {
    pool: PgPool,
    extra_checks: Arc<Vec<(String, CheckFn)>>,
    per_check_timeout: Duration,
    include_db_probe: bool,
}

impl HealthRouter {
    /// Create a health router with the default `/health` + `/ready`
    /// endpoints. `/ready` pings `pool` with `SELECT 1`. Default
    /// per-check timeout: 5 seconds.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            extra_checks: Vec::new(),
            per_check_timeout: Duration::from_secs(5),
            include_db_probe: true,
        }
    }

    /// Override the per-check timeout. Lower = faster failure-mode
    /// signaling, higher = tolerant of slow downstreams.
    #[must_use]
    pub fn timeout(mut self, t: Duration) -> Self {
        self.per_check_timeout = t;
        self
    }

    /// Disable the built-in `database` probe. Useful when you want to
    /// register your own with a different sentinel query, or when the
    /// pool you passed isn't actually expected to be ready (e.g. in
    /// tests).
    #[must_use]
    pub fn skip_db_probe(mut self) -> Self {
        self.include_db_probe = false;
        self
    }

    /// Register an additional check that runs as part of `/ready`.
    ///
    /// `name` appears in the JSON response. `check` returns `Ok(())` on
    /// success or `Err(message)` on failure. Errors longer than 200
    /// chars are truncated in the response.
    #[must_use]
    pub fn check<F, Fut>(mut self, name: &str, check: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        let boxed: CheckFn = Arc::new(move || Box::pin(check()));
        self.extra_checks.push((name.to_owned(), boxed));
        self
    }

    /// Built-in probe — opens a TCP connection to `addr` and closes it.
    /// `addr` is anything `tokio::net::TcpStream::connect` accepts:
    /// `"redis.svc:6379"`, `"127.0.0.1:5432"`, etc. Useful when the
    /// service you depend on doesn't expose an HTTP health endpoint.
    #[must_use]
    pub fn tcp_probe(self, name: &str, addr: impl Into<String>) -> Self {
        let addr = addr.into();
        self.check(name, move || {
            let addr = addr.clone();
            async move {
                tokio::net::TcpStream::connect(&addr)
                    .await
                    .map(|_| ())
                    .map_err(|e| format!("{addr}: {e}"))
            }
        })
    }

    /// Built-in probe — exercises the [`Cache`](crate::cache::Cache) by
    /// writing a value, reading it back, and asserting it round-trips.
    /// Catches both connection failures and read-after-write
    /// inconsistencies.
    #[cfg(feature = "cache")]
    #[must_use]
    pub fn cache_probe(self, name: &str, cache: crate::cache::BoxedCache) -> Self {
        let probe_name = name.to_owned();
        self.check(name, move || {
            let cache = cache.clone();
            let probe_name = probe_name.clone();
            async move {
                let key = format!("rustango:health:{probe_name}");
                let value = format!(
                    "{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_nanos())
                );
                cache
                    .set(&key, &value, Some(Duration::from_secs(10)))
                    .await
                    .map_err(|e| format!("set: {e}"))?;
                let got = cache
                    .get(&key)
                    .await
                    .map_err(|e| format!("get: {e}"))?
                    .ok_or_else(|| "set succeeded but get returned None".to_owned())?;
                if got != value {
                    return Err(format!(
                        "read-after-write mismatch: wrote {value}, read {got}"
                    ));
                }
                let _ = cache.delete(&key).await;
                Ok(())
            }
        })
    }

    /// Built-in probe — issues a GET via [`HttpClient`] and checks the
    /// response is 2xx. Useful for probing a downstream API.
    #[cfg(feature = "http-client")]
    #[must_use]
    pub fn http_probe(
        self,
        name: &str,
        client: crate::http_client::HttpClient,
        url: impl Into<String>,
    ) -> Self {
        let url = url.into();
        self.check(name, move || {
            let client = client.clone();
            let url = url.clone();
            async move {
                let resp = client
                    .get(url.as_str())
                    .send()
                    .await
                    .map_err(|e| format!("{url}: {e}"))?;
                let status = resp.status();
                if status.is_success() {
                    Ok(())
                } else {
                    Err(format!("{url}: status {status}"))
                }
            }
        })
    }

    /// Build the axum router.
    #[must_use]
    pub fn into_router(self) -> Router {
        let state = HealthState {
            pool: self.pool,
            extra_checks: Arc::new(self.extra_checks),
            per_check_timeout: self.per_check_timeout,
            include_db_probe: self.include_db_probe,
        };
        Router::new()
            .route("/health", get(handle_live))
            .route("/ready", get(handle_ready))
            .with_state(state)
    }
}

/// Convenience — `HealthRouter::new(pool).into_router()` in one call.
#[must_use]
pub fn health_router(pool: PgPool) -> Router {
    HealthRouter::new(pool).into_router()
}

async fn handle_live() -> Response {
    (StatusCode::OK, Json(json!({"status": "ok"}))).into_response()
}

async fn handle_ready(State(state): State<HealthState>) -> Response {
    let mut checks = serde_json::Map::new();
    let mut all_ok = true;

    if state.include_db_probe {
        let pool = state.pool.clone();
        let outcome = run_with_timeout(state.per_check_timeout, async move {
            sqlx::query("SELECT 1")
                .execute(&pool)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
        .await;
        if record(&mut checks, "database", outcome) {
            all_ok = false;
        }
    }

    for (name, check) in state.extra_checks.iter() {
        let outcome = run_with_timeout(state.per_check_timeout, check()).await;
        if record(&mut checks, name, outcome) {
            all_ok = false;
        }
    }

    let body = json!({
        "status": if all_ok { "ok" } else { "error" },
        "checks": Value::Object(checks),
    });
    let status = if all_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(body)).into_response()
}

/// Run `fut` under `timeout`, capturing the elapsed time alongside the
/// outcome.
async fn run_with_timeout<F>(timeout: Duration, fut: F) -> (Result<(), String>, Duration)
where
    F: Future<Output = Result<(), String>>,
{
    let start = Instant::now();
    let outcome = match tokio::time::timeout(timeout, fut).await {
        Ok(r) => r,
        Err(_) => Err(format!("timed out after {}ms", timeout.as_millis())),
    };
    (outcome, start.elapsed())
}

/// Insert a check result into the response map. Returns `true` if the
/// check FAILED (so the caller can flip the global `all_ok`).
fn record(
    checks: &mut serde_json::Map<String, Value>,
    name: &str,
    (outcome, elapsed): (Result<(), String>, Duration),
) -> bool {
    let latency_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    let (status_str, error_field) = match outcome {
        Ok(()) => ("ok", None),
        Err(e) => ("error", Some(truncate(&e, 200))),
    };
    let mut entry = serde_json::Map::new();
    entry.insert("status".into(), Value::String(status_str.into()));
    entry.insert("latency_ms".into(), Value::Number(latency_ms.into()));
    if let Some(err) = error_field {
        entry.insert("error".into(), Value::String(err));
    }
    checks.insert(name.to_owned(), Value::Object(entry));
    matches!(status_str, "error")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn lazy_pool() -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://localhost:1/none")
            .unwrap()
    }

    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn health_endpoint_always_returns_ok() {
        let app = HealthRouter::new(lazy_pool()).skip_db_probe().into_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v = body_json(resp).await;
        assert_eq!(v["status"], "ok");
    }

    #[tokio::test]
    async fn ready_passes_with_only_passing_extra_checks() {
        let app = HealthRouter::new(lazy_pool())
            .skip_db_probe()
            .check("always_ok", || async { Ok(()) })
            .into_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v = body_json(resp).await;
        assert_eq!(v["status"], "ok");
        assert_eq!(v["checks"]["always_ok"]["status"], "ok");
        // latency_ms must be present + numeric.
        assert!(v["checks"]["always_ok"]["latency_ms"].is_number());
    }

    #[tokio::test]
    async fn ready_returns_503_when_one_check_fails() {
        let app = HealthRouter::new(lazy_pool())
            .skip_db_probe()
            .check("ok", || async { Ok(()) })
            .check("broken", || async { Err("nope".into()) })
            .into_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 503);
        let v = body_json(resp).await;
        assert_eq!(v["status"], "error");
        assert_eq!(v["checks"]["broken"]["status"], "error");
        assert_eq!(v["checks"]["broken"]["error"], "nope");
        assert_eq!(v["checks"]["ok"]["status"], "ok");
    }

    #[tokio::test]
    async fn slow_check_is_killed_by_timeout() {
        let app = HealthRouter::new(lazy_pool())
            .skip_db_probe()
            .timeout(Duration::from_millis(50))
            .check("slow", || async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                Ok(())
            })
            .into_router();
        let start = std::time::Instant::now();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let elapsed = start.elapsed();
        assert_eq!(resp.status(), 503);
        assert!(
            elapsed < Duration::from_millis(500),
            "timeout didn't fire — took {elapsed:?}"
        );
        let v = body_json(resp).await;
        assert_eq!(v["checks"]["slow"]["status"], "error");
        assert!(v["checks"]["slow"]["error"]
            .as_str()
            .unwrap()
            .contains("timed out"));
    }

    #[tokio::test]
    async fn skip_db_probe_omits_database_check() {
        let app = HealthRouter::new(lazy_pool()).skip_db_probe().into_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let v = body_json(resp).await;
        assert!(v["checks"].get("database").is_none());
    }

    #[tokio::test]
    async fn long_error_message_is_truncated() {
        let huge = "x".repeat(500);
        let app = HealthRouter::new(lazy_pool())
            .skip_db_probe()
            .check("verbose", move || {
                let huge = huge.clone();
                async move { Err(huge) }
            })
            .into_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let v = body_json(resp).await;
        let err = v["checks"]["verbose"]["error"].as_str().unwrap();
        assert!(
            err.chars().count() <= 201,
            "got {} chars",
            err.chars().count()
        );
        assert!(err.ends_with('…'));
    }

    #[tokio::test]
    async fn tcp_probe_failure_reports_addr_in_error() {
        // 127.0.0.1:1 should reliably refuse on every CI machine.
        let app = HealthRouter::new(lazy_pool())
            .skip_db_probe()
            .tcp_probe("nothing-listening", "127.0.0.1:1")
            .timeout(Duration::from_millis(500))
            .into_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 503);
        let v = body_json(resp).await;
        assert_eq!(v["checks"]["nothing-listening"]["status"], "error");
        let err = v["checks"]["nothing-listening"]["error"].as_str().unwrap();
        assert!(err.contains("127.0.0.1:1"), "expected addr in error: {err}");
    }

    #[cfg(feature = "cache")]
    #[tokio::test]
    async fn cache_probe_succeeds_with_in_memory_cache() {
        use crate::cache::{BoxedCache, InMemoryCache};
        let cache: BoxedCache = Arc::new(InMemoryCache::new());
        let app = HealthRouter::new(lazy_pool())
            .skip_db_probe()
            .cache_probe("memory", cache)
            .into_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v = body_json(resp).await;
        assert_eq!(v["checks"]["memory"]["status"], "ok");
    }

    #[test]
    fn truncate_preserves_short_strings() {
        assert_eq!(truncate("hi", 10), "hi");
    }

    #[test]
    fn truncate_appends_ellipsis_when_over_max() {
        let t = truncate(&"a".repeat(300), 50);
        assert_eq!(t.chars().count(), 51);
        assert!(t.ends_with('…'));
    }
}
