//! Django-shape HTTP method restriction layer — mirrors
//! `django.views.decorators.http.{require_http_methods, require_GET,
//! require_POST, require_safe}`.
//!
//! Django uses these as decorators on view functions:
//!
//! ```python
//! @require_http_methods(["GET", "POST"])
//! def my_view(request):
//!     ...
//!
//! @require_POST
//! def submit(request):
//!     ...
//! ```
//!
//! axum routes already pick a method, so this looks redundant until
//! one handler must take GET and POST but not PUT, DELETE or PATCH.
//! Then `Router::route("/x", any(handler))` plus
//! `MethodRestrictLayer::any_of(["GET", "POST"])` does it without
//! splitting the route.
//!
//! A rejected method gets `405 Method Not Allowed` with an `Allow:`
//! header listing what is accepted, as RFC 7231 requires.
//!
//! ## Usage
//!
//! ```ignore
//! use axum::{Router, routing::any};
//! use rustango::http_methods::{MethodRestrictLayer, MethodRestrictRouterExt};
//!
//! let app: Router = Router::new()
//!     .route("/submit", any(handler))
//!     .require_methods(["POST"]);                 // POST only
//!
//! let mixed: Router = Router::new()
//!     .route("/feed", any(handler))
//!     .require_methods(["GET", "HEAD", "POST"]);  // multi-method view
//!
//! // Convenience shortcuts mirror Django's per-verb decorators.
//! let safe_only: Router = Router::new()
//!     .route("/health", any(handler))
//!     .require_safe();                            // GET / HEAD / OPTIONS
//! ```

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use axum::Router;

/// Gates a route on a fixed set of HTTP methods. Anything else gets a
/// `405` with an `Allow:` header. Build it with [`Self::any_of`] or
/// through [`MethodRestrictRouterExt`].
#[derive(Clone, Debug)]
pub struct MethodRestrictLayer {
    allowed: Arc<Vec<Method>>,
    /// Ready-made `Allow:` value, so a 405 allocates nothing.
    allow_header: Arc<String>,
}

impl MethodRestrictLayer {
    /// Accept only these methods. An empty list is allowed and
    /// rejects everything with a 405.
    #[must_use]
    pub fn any_of<I, M>(methods: I) -> Self
    where
        I: IntoIterator<Item = M>,
        M: Into<Method>,
    {
        let allowed: Vec<Method> = methods.into_iter().map(Into::into).collect();
        let allow_header = allowed
            .iter()
            .map(Method::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        Self {
            allowed: Arc::new(allowed),
            allow_header: Arc::new(allow_header),
        }
    }

    /// Django parity `@require_GET` — accept only GET.
    #[must_use]
    pub fn get_only() -> Self {
        Self::any_of([Method::GET])
    }

    /// Django parity `@require_POST` — accept only POST.
    #[must_use]
    pub fn post_only() -> Self {
        Self::any_of([Method::POST])
    }

    /// Like Django's `@require_safe`: GET, HEAD and OPTIONS, the
    /// methods RFC 7231 §4.2.1 calls safe.
    #[must_use]
    pub fn safe_only() -> Self {
        Self::any_of([Method::GET, Method::HEAD, Method::OPTIONS])
    }
}

/// `router.require_methods([…])` instead of
/// `.layer(MethodRestrictLayer::any_of([…]))`.
pub trait MethodRestrictRouterExt {
    /// Restrict this router to the given HTTP methods.
    #[must_use]
    fn require_methods<I, M>(self, methods: I) -> Self
    where
        I: IntoIterator<Item = M>,
        M: Into<Method>;

    /// Django parity `@require_GET`.
    #[must_use]
    fn require_get(self) -> Self;

    /// Django parity `@require_POST`.
    #[must_use]
    fn require_post(self) -> Self;

    /// GET, HEAD and OPTIONS only.
    #[must_use]
    fn require_safe(self) -> Self;
}

impl<S> MethodRestrictRouterExt for Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    fn require_methods<I, M>(self, methods: I) -> Self
    where
        I: IntoIterator<Item = M>,
        M: Into<Method>,
    {
        attach(self, MethodRestrictLayer::any_of(methods))
    }
    fn require_get(self) -> Self {
        attach(self, MethodRestrictLayer::get_only())
    }
    fn require_post(self) -> Self {
        attach(self, MethodRestrictLayer::post_only())
    }
    fn require_safe(self) -> Self {
        attach(self, MethodRestrictLayer::safe_only())
    }
}

fn attach<S: Clone + Send + Sync + 'static>(
    router: Router<S>,
    layer: MethodRestrictLayer,
) -> Router<S> {
    router.layer(axum::middleware::from_fn(
        move |req: Request<Body>, next: Next| {
            let layer = layer.clone();
            async move { handle(layer, req, next).await }
        },
    ))
}

async fn handle(layer: MethodRestrictLayer, req: Request<Body>, next: Next) -> Response {
    if layer.allowed.contains(req.method()) {
        return next.run(req).await;
    }
    let mut resp = Response::builder()
        .status(StatusCode::METHOD_NOT_ALLOWED)
        .body(Body::empty())
        .unwrap_or_else(|_| Response::new(Body::empty()));
    if let Ok(v) = HeaderValue::from_str(&layer.allow_header) {
        resp.headers_mut().insert(header::ALLOW, v);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::any;
    use tower::ServiceExt;

    fn app(layer: MethodRestrictLayer) -> Router {
        attach(Router::new().route("/r", any(|| async { "ok" })), layer)
    }

    fn req(method: Method) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri("/r")
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn require_methods_accepts_listed() {
        let res = app(MethodRestrictLayer::any_of([Method::GET, Method::POST]))
            .oneshot(req(Method::POST))
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    #[tokio::test]
    async fn require_methods_rejects_unlisted_with_405() {
        let res = app(MethodRestrictLayer::any_of([Method::GET]))
            .oneshot(req(Method::POST))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn rejection_includes_allow_header() {
        let res = app(MethodRestrictLayer::any_of([Method::GET, Method::POST]))
            .oneshot(req(Method::DELETE))
            .await
            .unwrap();
        let allow = res
            .headers()
            .get(header::ALLOW)
            .and_then(|v| v.to_str().ok())
            .unwrap()
            .to_owned();
        assert!(allow.contains("GET"));
        assert!(allow.contains("POST"));
        assert!(!allow.contains("DELETE"));
    }

    #[tokio::test]
    async fn get_only_rejects_post() {
        let res = app(MethodRestrictLayer::get_only())
            .oneshot(req(Method::POST))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn post_only_accepts_post() {
        let res = app(MethodRestrictLayer::post_only())
            .oneshot(req(Method::POST))
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    #[tokio::test]
    async fn safe_only_accepts_get_head_options() {
        for m in [Method::GET, Method::HEAD, Method::OPTIONS] {
            let res = app(MethodRestrictLayer::safe_only())
                .oneshot(req(m.clone()))
                .await
                .unwrap();
            assert_eq!(res.status(), 200, "method {m:?} should be accepted");
        }
    }

    #[tokio::test]
    async fn safe_only_rejects_post() {
        let res = app(MethodRestrictLayer::safe_only())
            .oneshot(req(Method::POST))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn router_ext_require_methods() {
        let app: Router = Router::new()
            .route("/r", any(|| async { "ok" }))
            .require_methods([Method::POST]);
        let res = app.oneshot(req(Method::GET)).await.unwrap();
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn router_ext_require_get() {
        let app: Router = Router::new()
            .route("/r", any(|| async { "ok" }))
            .require_get();
        let ok = app.clone().oneshot(req(Method::GET)).await.unwrap();
        assert_eq!(ok.status(), 200);
        let bad = app.oneshot(req(Method::POST)).await.unwrap();
        assert_eq!(bad.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn router_ext_require_post() {
        let app: Router = Router::new()
            .route("/r", any(|| async { "ok" }))
            .require_post();
        let bad = app.clone().oneshot(req(Method::GET)).await.unwrap();
        assert_eq!(bad.status(), StatusCode::METHOD_NOT_ALLOWED);
        let ok = app.oneshot(req(Method::POST)).await.unwrap();
        assert_eq!(ok.status(), 200);
    }

    #[tokio::test]
    async fn empty_methods_list_rejects_everything() {
        // An empty allowlist: 405 everywhere, empty Allow header.
        let app = app(MethodRestrictLayer::any_of(Vec::<Method>::new()));
        let res = app.oneshot(req(Method::GET)).await.unwrap();
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
        let allow = res
            .headers()
            .get(header::ALLOW)
            .and_then(|v| v.to_str().ok())
            .unwrap();
        assert!(allow.is_empty());
    }

    #[test]
    fn allow_header_lists_methods_in_construction_order() {
        let layer = MethodRestrictLayer::any_of([Method::POST, Method::GET, Method::DELETE]);
        assert_eq!(layer.allow_header.as_str(), "POST, GET, DELETE");
    }
}
