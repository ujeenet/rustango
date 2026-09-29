//! Request body size limit middleware.
//!
//! An unbounded request body is a denial-of-service: one client can
//! make the server buffer gigabytes. axum's `DefaultBodyLimit` caps
//! 2 MiB per extractor; this layer adds a router-wide cap. A declared
//! `Content-Length` over it gets a JSON `413 Payload Too Large` upfront.
//! Any other body is capped as it streams, and a body extractor that
//! reads past the cap answers `413`.
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
//! For a different cap on one route, put that route in a sub-router and
//! merge it after the global layer.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use axum::Router;

#[derive(Clone, Debug)]
pub struct BodyLimitLayer {
    /// Maximum body size in bytes, declared or streamed.
    pub max_bytes: usize,
    /// Methods whose bodies are checked. Default: POST, PUT, PATCH and
    /// QUERY. GET, DELETE and HEAD usually have no body, so they are
    /// skipped.
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
            methods: vec![
                Method::POST,
                Method::PUT,
                Method::PATCH,
                crate::http_query::QUERY.clone(),
            ],
        }
    }

    /// Choose which methods to check. An empty vec checks every
    /// request.
    #[must_use]
    pub fn methods(mut self, m: Vec<axum::http::Method>) -> Self {
        self.methods = m;
        self
    }

    /// Build the layer from [`crate::config::ServerSettings`]. Returns
    /// `None` when `max_body_bytes` is unset, so the layer is opt-in.
    /// Methods stay at the default.
    ///
    /// ```ignore
    /// let cfg = rustango::config::Settings::load_from_env()?;
    /// if let Some(layer) = BodyLimitLayer::from_settings(&cfg.server) {
    ///     app = app.body_limit(layer);
    /// }
    /// ```
    #[cfg(feature = "config")]
    #[must_use]
    pub fn from_settings(s: &crate::config::ServerSettings) -> Option<Self> {
        let max = s.max_body_bytes?;
        // On 32-bit targets a huge config value saturates instead of
        // wrapping, which is the safe way to fail.
        let max = usize::try_from(max).unwrap_or(usize::MAX);
        Some(Self::new(max))
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
    // Chunked and HTTP/2 bodies carry no Content-Length: cap the stream.
    let (parts, body) = req.into_parts();
    let body = Body::new(http_body_util::Limited::new(body, cfg.max_bytes));
    next.run(Request::from_parts(parts, body)).await
}

/// Whether a body read failed on a size cap, not on the stream.
// Its callers (ViewSet, idempotency) are behind their own features.
#[cfg_attr(not(feature = "admin"), allow(dead_code))]
pub(crate) fn over_cap(e: &axum::Error) -> bool {
    let mut err: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(e) = err {
        if e.is::<http_body_util::LengthLimitError>() {
            return true;
        }
        err = e.source();
    }
    false
}

fn too_large(limit: usize) -> Response {
    crate::api_errors::ApiError::from_status(StatusCode::PAYLOAD_TOO_LARGE, "payload too large")
        .with_details(serde_json::json!({ "limit_bytes": limit }))
        .into_response()
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
        assert_eq!(v["error"], "payload_too_large");
        assert_eq!(v["message"], "payload too large");
        assert_eq!(v["details"]["limit_bytes"], 10);
    }

    #[tokio::test]
    async fn get_requests_skipped_by_default() {
        // GET is not checked by default, even with a Content-Length.
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

    /// A body with no `Content-Length`, as chunked or HTTP/2 sends it.
    fn streamed(method: Method, n: usize) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri("/")
            .body(Body::from("0".repeat(n)))
            .unwrap()
    }

    fn reading_app(limit: usize) -> Router {
        Router::new()
            .route("/", post(|b: String| async move { b.len().to_string() }))
            .route("/q", crate::http_query::query(|b: String| async move { b }))
            .body_limit(BodyLimitLayer::new(limit))
    }

    #[tokio::test]
    async fn streamed_body_over_limit_gets_413() {
        let resp = reading_app(1024)
            .oneshot(streamed(Method::POST, 1025))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn streamed_body_under_limit_passes() {
        let resp = reading_app(1024)
            .oneshot(streamed(Method::POST, 1024))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn query_body_is_limited_by_default() {
        let mut req = streamed(crate::http_query::QUERY.clone(), 200 * 1024);
        *req.uri_mut() = "/q".parse().unwrap();
        let resp = reading_app(1024).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
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
    fn default_methods_are_post_put_patch_query() {
        let l = BodyLimitLayer::default();
        assert_eq!(l.methods.len(), 4);
        assert!(l.methods.contains(&Method::POST));
        assert!(l.methods.contains(&Method::PUT));
        assert!(l.methods.contains(&Method::PATCH));
        assert!(l.methods.contains(&crate::http_query::QUERY));
    }

    /// Unset → None, so the caller skips mounting (opt-in).
    #[cfg(feature = "config")]
    #[test]
    fn from_settings_unset_returns_none() {
        let s = crate::config::ServerSettings::default();
        assert!(BodyLimitLayer::from_settings(&s).is_none());
    }

    /// Configured value lands in `max_bytes`.
    #[cfg(feature = "config")]
    #[test]
    fn from_settings_sets_max_bytes() {
        let mut s = crate::config::ServerSettings::default();
        s.max_body_bytes = Some(10_000_000); // 10 MB
        let layer = BodyLimitLayer::from_settings(&s).expect("Some");
        assert_eq!(layer.max_bytes, 10_000_000);
        // Methods preserved at default.
        assert_eq!(layer.methods, BodyLimitLayer::default().methods);
    }
}
