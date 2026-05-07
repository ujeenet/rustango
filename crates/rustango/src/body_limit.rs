//! Request body size limit middleware.
//!
//! axum's `DefaultBodyLimit` already caps incoming bodies at 2 MiB
//! per-extractor — this layer adds a router-wide cap that fires
//! BEFORE the body is read into memory by checking `Content-Length`
//! upfront, returning a structured `413 Payload Too Large` JSON
//! response instead of axum's default plain-text error.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::body_limit::{BodyLimitLayer, BodyLimitRouterExt};
//!
//! let app = axum::Router::new()
//!     .route("/api/upload", axum::routing::post(upload))
//!     .body_limit(BodyLimitLayer::new(10 * 1024 * 1024)); // 10 MiB
//! ```
//!
//! Per-route override: build a sub-router for the route(s) that need a
//! different cap and merge it after the global layer is applied.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use axum::Router;

#[derive(Clone, Debug)]
pub struct BodyLimitLayer {
    /// Maximum body size in bytes. Requests with a `Content-Length`
    /// above this get a `413 Payload Too Large` upfront.
    pub max_bytes: usize,
    /// Method names whose bodies we check. Default: POST, PUT, PATCH.
    /// GET / DELETE / HEAD typically have no body so we skip them.
    pub methods: Vec<axum::http::Method>,
}

impl Default for BodyLimitLayer {
    fn default() -> Self {
        Self::new(2 * 1024 * 1024)
    }
}

impl BodyLimitLayer {
    #[must_use]
    pub fn new(max_bytes: usize) -> Self {
        use axum::http::Method;
        Self {
            max_bytes,
            methods: vec![Method::POST, Method::PUT, Method::PATCH],
        }
    }

    /// Override the methods checked. Pass an empty vec to check every
    /// request regardless of method.
    #[must_use]
    pub fn methods(mut self, m: Vec<axum::http::Method>) -> Self {
        self.methods = m;
        self
    }
}

pub trait BodyLimitRouterExt {
    #[must_use]
    fn body_limit(self, layer: BodyLimitLayer) -> Self;
}

impl<S: Clone + Send + Sync + 'static> BodyLimitRouterExt for Router<S> {
    fn body_limit(self, layer: BodyLimitLayer) -> Self {
        let cfg = Arc::new(layer);
        self.layer(axum::middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let cfg = cfg.clone();
                async move { handle(cfg, req, next).await }
            },
        ))
    }
}

async fn handle(cfg: Arc<BodyLimitLayer>, req: Request<Body>, next: Next) -> Response {
    if !cfg.methods.is_empty() && !cfg.methods.contains(req.method()) {
        return next.run(req).await;
    }
    if let Some(declared) = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
    {
        if usize::try_from(declared).map_or(true, |n| n > cfg.max_bytes) {
            return too_large(cfg.max_bytes);
        }
    }
    next.run(req).await
}

fn too_large(limit: usize) -> Response {
    let body = format!(r#"{{"error":"payload too large","limit_bytes":{limit}}}"#);
    let mut resp = Response::builder()
        .status(StatusCode::PAYLOAD_TOO_LARGE)
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;
    use axum::routing::{get, post};
    use tower::ServiceExt;

    fn app(limit: usize) -> Router {
        Router::new()
            .route("/", post(|| async { "ok" }))
            .route("/get", get(|| async { "ok" }))
            .body_limit(BodyLimitLayer::new(limit))
    }

    #[tokio::test]
    async fn small_body_passes_through() {
        let resp = app(1024)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/")
                    .header(header::CONTENT_LENGTH, "10")
                    .body(Body::from("0123456789"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn body_at_exact_limit_passes() {
        let resp = app(10)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/")
                    .header(header::CONTENT_LENGTH, "10")
                    .body(Body::from("0123456789"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn body_over_limit_rejected_with_413_json() {
        let resp = app(10)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/")
                    .header(header::CONTENT_LENGTH, "100")
                    .body(Body::from("0".repeat(100)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "application/json"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"], "payload too large");
        assert_eq!(v["limit_bytes"], 10);
    }

    #[tokio::test]
    async fn get_requests_skipped_by_default() {
        // GET shouldn't have a body, but a malicious client can lie about
        // Content-Length. By default we don't check GET so handlers see
        // it for what it is.
        let resp = app(10)
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/get")
                    .header(header::CONTENT_LENGTH, "999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn missing_content_length_lets_request_through() {
        // Without Content-Length we can't enforce upfront; defer to
        // axum's per-extractor DefaultBodyLimit (or the user's choice).
        let resp = app(10)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/")
                    .body(Body::from("0".repeat(100)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn empty_methods_list_checks_every_method() {
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .body_limit(BodyLimitLayer::new(10).methods(Vec::new()));
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/")
                    .header(header::CONTENT_LENGTH, "100")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn default_layer_has_2mib_limit() {
        let l = BodyLimitLayer::default();
        assert_eq!(l.max_bytes, 2 * 1024 * 1024);
    }

    #[test]
    fn default_methods_are_post_put_patch() {
        let l = BodyLimitLayer::default();
        assert_eq!(l.methods.len(), 3);
        assert!(l.methods.contains(&Method::POST));
        assert!(l.methods.contains(&Method::PUT));
        assert!(l.methods.contains(&Method::PATCH));
    }
}
