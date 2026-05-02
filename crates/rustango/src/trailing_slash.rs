//! Trailing-slash redirect middleware — canonicalize URL paths.
//!
//! Returns `301 Moved Permanently` (or `308`, configurable) to the
//! canonical form of the URL when the request path doesn't match it.
//! Same shape as Django's `APPEND_SLASH` and Rails' `trailing_slash`
//! routing.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::trailing_slash::{TrailingSlashLayer, TrailingSlashRouterExt, SlashStyle};
//!
//! // Force every URL to end with `/` (Django default)
//! let app = axum::Router::new()
//!     .route("/posts/", axum::routing::get(list))
//!     .trailing_slash(TrailingSlashLayer::new(SlashStyle::Append));
//!
//! // OR: strip trailing slashes
//! let app = axum::Router::new()
//!     .route("/posts", axum::routing::get(list))
//!     .trailing_slash(TrailingSlashLayer::new(SlashStyle::Strip));
//! ```
//!
//! ## What's NOT touched
//!
//! - The root path `/` is always preserved.
//! - Non-GET / non-HEAD requests pass through (a 308 from a POST is
//!   technically allowed but most clients don't replay the body
//!   reliably — better to surface a 405 from the routing layer).
//! - Paths that already match the canonical form pass through.
//! - Query strings are preserved on the redirect target.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderValue, Method, Response, StatusCode};
use axum::middleware::Next;
use axum::Router;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlashStyle {
    /// Add `/` to paths that lack one (Django `APPEND_SLASH = True`).
    Append,
    /// Remove the trailing `/` (common for REST APIs).
    Strip,
}

#[derive(Clone, Debug)]
pub struct TrailingSlashLayer {
    pub style: SlashStyle,
    /// 301 (default — caches the redirect, ideal for SEO) or 308
    /// (preserves the method + body — only matters for POST/PUT, but
    /// we don't redirect those by default anyway).
    pub status: StatusCode,
    /// Methods that get redirected. Default: `[GET, HEAD]`.
    pub methods: Vec<Method>,
}

impl TrailingSlashLayer {
    #[must_use]
    pub fn new(style: SlashStyle) -> Self {
        Self {
            style,
            status: StatusCode::MOVED_PERMANENTLY,
            methods: vec![Method::GET, Method::HEAD],
        }
    }

    #[must_use]
    pub fn status(mut self, s: StatusCode) -> Self {
        self.status = s;
        self
    }

    /// Override the methods that get redirected. Pass an empty vec to
    /// redirect every method.
    #[must_use]
    pub fn methods(mut self, m: Vec<Method>) -> Self {
        self.methods = m;
        self
    }
}

pub trait TrailingSlashRouterExt {
    #[must_use]
    fn trailing_slash(self, layer: TrailingSlashLayer) -> Self;
}

impl<S: Clone + Send + Sync + 'static> TrailingSlashRouterExt for Router<S> {
    fn trailing_slash(self, layer: TrailingSlashLayer) -> Self {
        let cfg = Arc::new(layer);
        self.layer(axum::middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let cfg = cfg.clone();
                async move { handle(cfg, req, next).await }
            },
        ))
    }
}

async fn handle(cfg: Arc<TrailingSlashLayer>, req: Request<Body>, next: Next) -> Response<Body> {
    if !cfg.methods.is_empty() && !cfg.methods.contains(req.method()) {
        return next.run(req).await;
    }
    let path = req.uri().path();
    if path == "/" {
        return next.run(req).await;
    }
    let Some(canonical) = canonical_path(path, cfg.style) else {
        return next.run(req).await;
    };
    let location = with_query(&canonical, req.uri().query());
    redirect(cfg.status, &location)
}

/// Returns `Some(canonical)` when `path` differs from canonical form,
/// `None` when it's already canonical.
fn canonical_path(path: &str, style: SlashStyle) -> Option<String> {
    match style {
        SlashStyle::Append => {
            if path.ends_with('/') {
                None
            } else {
                Some(format!("{path}/"))
            }
        }
        SlashStyle::Strip => {
            if path.ends_with('/') {
                Some(path.trim_end_matches('/').to_owned())
            } else {
                None
            }
        }
    }
}

fn with_query(path: &str, query: Option<&str>) -> String {
    match query {
        Some(q) if !q.is_empty() => format!("{path}?{q}"),
        _ => path.to_owned(),
    }
}

fn redirect(status: StatusCode, location: &str) -> Response<Body> {
    let mut resp = Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap_or_else(|_| Response::new(Body::empty()));
    if let Ok(v) = HeaderValue::from_str(location) {
        resp.headers_mut().insert(header::LOCATION, v);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use axum::routing::{get, post};
    use tower::ServiceExt;

    fn append_app() -> Router {
        Router::new()
            .route("/foo/", get(|| async { "ok" }))
            .trailing_slash(TrailingSlashLayer::new(SlashStyle::Append))
    }

    fn strip_app() -> Router {
        Router::new()
            .route("/foo", get(|| async { "ok" }))
            .trailing_slash(TrailingSlashLayer::new(SlashStyle::Strip))
    }

    #[tokio::test]
    async fn append_redirects_when_slash_missing() {
        let resp = append_app()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/foo")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap().to_str().unwrap(),
            "/foo/"
        );
    }

    #[tokio::test]
    async fn append_passes_through_when_already_canonical() {
        let resp = append_app()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/foo/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn strip_redirects_when_slash_present() {
        let resp = strip_app()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/foo/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap().to_str().unwrap(),
            "/foo"
        );
    }

    #[tokio::test]
    async fn root_path_never_redirects() {
        let app = Router::new()
            .route("/", get(|| async { "root" }))
            .trailing_slash(TrailingSlashLayer::new(SlashStyle::Strip));
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn query_string_is_preserved_on_redirect() {
        let resp = append_app()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/foo?page=2&sort=desc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap().to_str().unwrap(),
            "/foo/?page=2&sort=desc"
        );
    }

    #[tokio::test]
    async fn post_passes_through_by_default() {
        let app = Router::new()
            .route("/foo", post(|| async { "created" }))
            .route("/foo/", post(|| async { "created" }))
            .trailing_slash(TrailingSlashLayer::new(SlashStyle::Append));
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/foo")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // No redirect — handler runs.
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn empty_methods_list_redirects_every_method() {
        let app = Router::new()
            .route("/foo/", post(|| async { "ok" }))
            .trailing_slash(
                TrailingSlashLayer::new(SlashStyle::Append).methods(Vec::new()),
            );
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/foo")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::MOVED_PERMANENTLY);
    }

    #[tokio::test]
    async fn status_308_preserves_method() {
        let resp = Router::new()
            .route("/foo/", get(|| async { "ok" }))
            .trailing_slash(
                TrailingSlashLayer::new(SlashStyle::Append)
                    .status(StatusCode::PERMANENT_REDIRECT),
            )
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/foo")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
    }

    #[test]
    fn canonical_path_append_logic() {
        assert_eq!(canonical_path("/foo", SlashStyle::Append), Some("/foo/".into()));
        assert_eq!(canonical_path("/foo/", SlashStyle::Append), None);
        assert_eq!(canonical_path("/a/b/c", SlashStyle::Append), Some("/a/b/c/".into()));
    }

    #[test]
    fn canonical_path_strip_logic() {
        assert_eq!(canonical_path("/foo/", SlashStyle::Strip), Some("/foo".into()));
        assert_eq!(canonical_path("/foo", SlashStyle::Strip), None);
        // Multiple trailing slashes get collapsed.
        assert_eq!(canonical_path("/foo///", SlashStyle::Strip), Some("/foo".into()));
    }

    #[test]
    fn with_query_handles_missing_and_empty() {
        assert_eq!(with_query("/foo", None), "/foo");
        assert_eq!(with_query("/foo", Some("")), "/foo");
        assert_eq!(with_query("/foo", Some("a=1")), "/foo?a=1");
    }
}
