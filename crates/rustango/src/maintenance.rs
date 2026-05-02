//! Maintenance-mode middleware — return 503 with `Retry-After` from a
//! shared flag, so an orchestrator (or a sidecar) can drain traffic
//! before a deploy / migration without killing in-flight requests.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::maintenance::{MaintenanceFlag, MaintenanceLayer, MaintenanceRouterExt};
//! use std::time::Duration;
//!
//! let flag = MaintenanceFlag::new();
//!
//! let app = axum::Router::new()
//!     .route("/api/posts", axum::routing::get(list))
//!     .maintenance(MaintenanceLayer::new(flag.clone())
//!         .retry_after(Duration::from_secs(30))
//!         .allow_path("/health")
//!         .allow_path("/ready"));
//!
//! // Some other place (signal handler, control-plane endpoint, ...):
//! flag.enable();   // start serving 503
//! flag.disable();  // resume normal operation
//! ```
//!
//! ## What it does
//!
//! - When the flag is OFF, requests pass through unchanged.
//! - When the flag is ON, requests return `503 Service Unavailable`
//!   with a configurable JSON body, `Retry-After`, and the standard
//!   `Cache-Control: no-store` so caches don't sticky the maintenance
//!   page.
//! - Optional allow-list of exact paths that bypass the layer (almost
//!   always `/health` and `/ready` so orchestrators keep getting truth
//!   while you're under maintenance).
//!
//! Pair this with [`crate::body_limit::BodyLimitLayer`] and
//! [`crate::real_ip::RealIpLayer`] in the same router stack — order
//! doesn't matter much, but putting maintenance OUTERMOST means a
//! flipped flag short-circuits before any handler work runs.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderValue, Response, StatusCode};
use axum::middleware::Next;
use axum::Router;

/// Shared on/off flag. Cheap to clone; threadsafe.
#[derive(Clone, Default, Debug)]
pub struct MaintenanceFlag {
    inner: Arc<AtomicBool>,
}

impl MaintenanceFlag {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-configured with a starting state — handy for tests.
    #[must_use]
    pub fn with_state(on: bool) -> Self {
        Self {
            inner: Arc::new(AtomicBool::new(on)),
        }
    }

    /// Switch maintenance mode ON. New requests outside the allow-list
    /// will see 503.
    pub fn enable(&self) {
        self.inner.store(true, Ordering::SeqCst);
    }

    /// Switch maintenance mode OFF. Resumes normal operation.
    pub fn disable(&self) {
        self.inner.store(false, Ordering::SeqCst);
    }

    /// Atomically swap state, returning the previous value.
    pub fn swap(&self, on: bool) -> bool {
        self.inner.swap(on, Ordering::SeqCst)
    }

    #[must_use]
    pub fn is_on(&self) -> bool {
        self.inner.load(Ordering::SeqCst)
    }
}

/// Maintenance-mode middleware configuration. Cheap to clone.
#[derive(Clone)]
pub struct MaintenanceLayer {
    flag: MaintenanceFlag,
    retry_after: Duration,
    body: Arc<String>,
    allow_paths: Arc<HashSet<String>>,
}

impl MaintenanceLayer {
    /// New layer driven by `flag`. Default `Retry-After` is 60 s; default
    /// body is `{"error":"under maintenance"}`.
    #[must_use]
    pub fn new(flag: MaintenanceFlag) -> Self {
        Self {
            flag,
            retry_after: Duration::from_secs(60),
            body: Arc::new(r#"{"error":"under maintenance"}"#.to_owned()),
            allow_paths: Arc::new(HashSet::new()),
        }
    }

    #[must_use]
    pub fn retry_after(mut self, d: Duration) -> Self {
        self.retry_after = d;
        self
    }

    /// Override the JSON body returned during maintenance. Must be
    /// valid JSON (the layer doesn't validate).
    #[must_use]
    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = Arc::new(body.into());
        self
    }

    /// Add an exact path that bypasses maintenance mode. Almost always
    /// `/health` and `/ready` so orchestrators see truth.
    #[must_use]
    pub fn allow_path(mut self, path: impl Into<String>) -> Self {
        let mut set = (*self.allow_paths).clone();
        set.insert(path.into());
        self.allow_paths = Arc::new(set);
        self
    }
}

pub trait MaintenanceRouterExt {
    #[must_use]
    fn maintenance(self, layer: MaintenanceLayer) -> Self;
}

impl<S: Clone + Send + Sync + 'static> MaintenanceRouterExt for Router<S> {
    fn maintenance(self, layer: MaintenanceLayer) -> Self {
        let cfg = Arc::new(layer);
        self.layer(axum::middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let cfg = cfg.clone();
                async move { handle(cfg, req, next).await }
            },
        ))
    }
}

async fn handle(cfg: Arc<MaintenanceLayer>, req: Request<Body>, next: Next) -> Response<Body> {
    if !cfg.flag.is_on() {
        return next.run(req).await;
    }
    if cfg.allow_paths.contains(req.uri().path()) {
        return next.run(req).await;
    }
    build_503(&cfg)
}

fn build_503(cfg: &MaintenanceLayer) -> Response<Body> {
    let secs = cfg.retry_after.as_secs().to_string();
    let mut resp = Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .body(Body::from((*cfg.body).clone()))
        .unwrap_or_else(|_| Response::new(Body::empty()));
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Ok(v) = HeaderValue::from_str(&secs) {
        h.insert(header::RETRY_AFTER, v);
    }
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use tower::ServiceExt;

    fn app(flag: MaintenanceFlag) -> Router {
        Router::new()
            .route("/", get(|| async { "ok" }))
            .route("/health", get(|| async { "alive" }))
            .maintenance(MaintenanceLayer::new(flag).allow_path("/health"))
    }

    #[tokio::test]
    async fn flag_off_passes_through() {
        let flag = MaintenanceFlag::new();
        let resp = app(flag)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn flag_on_returns_503_with_retry_after() {
        let flag = MaintenanceFlag::with_state(true);
        let resp = app(flag)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(resp.headers().get(header::RETRY_AFTER).is_some());
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap().to_str().unwrap(),
            "application/json"
        );
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap().to_str().unwrap(),
            "no-store"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"], "under maintenance");
    }

    #[tokio::test]
    async fn allow_listed_path_bypasses_maintenance() {
        let flag = MaintenanceFlag::with_state(true);
        let resp = app(flag)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn flag_can_be_toggled_at_runtime() {
        let flag = MaintenanceFlag::new();
        let app = app(flag.clone());

        // OFF -> 200
        let r1 = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r1.status(), 200);

        // Flip ON -> 503
        flag.enable();
        let r2 = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r2.status(), 503);

        // Flip OFF -> 200 again
        flag.disable();
        let r3 = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r3.status(), 200);
    }

    #[tokio::test]
    async fn custom_body_is_returned_verbatim() {
        let flag = MaintenanceFlag::with_state(true);
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .maintenance(
                MaintenanceLayer::new(flag).body(r#"{"error":"deploying","eta_minutes":2}"#),
            );
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["eta_minutes"], 2);
    }

    #[tokio::test]
    async fn retry_after_header_value_matches_config() {
        let flag = MaintenanceFlag::with_state(true);
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .maintenance(
                MaintenanceLayer::new(flag).retry_after(Duration::from_secs(123)),
            );
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let v = resp.headers().get(header::RETRY_AFTER).unwrap().to_str().unwrap();
        assert_eq!(v, "123");
    }

    #[test]
    fn swap_returns_previous_value() {
        let f = MaintenanceFlag::new();
        assert!(!f.swap(true), "previous value was OFF");
        assert!(f.swap(false), "previous value was ON");
    }

    #[test]
    fn flag_clone_shares_state() {
        let a = MaintenanceFlag::new();
        let b = a.clone();
        a.enable();
        assert!(b.is_on(), "clone observes the same atomic");
    }
}
