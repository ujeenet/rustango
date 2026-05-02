//! HTTP method override — rewrite POST → PUT / PATCH / DELETE based on
//! a hidden form field or header.
//!
//! HTML `<form>` only emits GET or POST — no PUT, PATCH, or DELETE.
//! The standard Laravel / Express convention is a hidden `_method`
//! field (or `X-HTTP-Method-Override` header) carrying the intended
//! verb; the server rewrites the request before routing.
//!
//! ## Why a tower Layer (not Router::layer)
//!
//! axum 0.8's `Router::layer(...)` runs the middleware AROUND the
//! selected handler — after routing. To rewrite the method BEFORE
//! routing dispatches, the layer must wrap the entire Router. Use
//! [`tower::ServiceBuilder`] (or call `MethodOverrideLayer::layer`
//! directly) to compose:
//!
//! ```ignore
//! use rustango::method_override::MethodOverrideLayer;
//! use tower::ServiceBuilder;
//!
//! let inner: axum::Router = axum::Router::new()
//!     .route("/posts/{id}", axum::routing::delete(destroy));
//!
//! let app = ServiceBuilder::new()
//!     .layer(MethodOverrideLayer::default())
//!     .service(inner);
//!
//! // app is a Service — pass to axum::serve(listener, app.into_make_service()).
//! ```
//!
//! ```html
//! <form method="POST" action="/posts/42">
//!     <input type="hidden" name="_method" value="DELETE">
//!     <button>Delete</button>
//! </form>
//! ```
//!
//! ## Strategies
//!
//! Tried in this order (whichever fires first wins):
//!
//! 1. `X-HTTP-Method-Override` header (preferred — works for any
//!    Content-Type, cheaper than parsing the body).
//! 2. `_method` form field, when the request is
//!    `application/x-www-form-urlencoded` AND the body is small enough.
//!    Body-parse limit is configurable (default 64 KiB).
//!
//! ## Safety
//!
//! - Only POST requests are rewritten — never GET / HEAD / OPTIONS,
//!   regardless of what the client claims.
//! - The override target must be in the configured allow-list. Default:
//!   PUT, PATCH, DELETE. Anything else is ignored.
//! - The body is read once into memory when checking the form-field
//!   strategy — large uploads should use the header strategy and skip
//!   the body parse entirely.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::{to_bytes, Body};
use axum::http::{header, HeaderMap, Method, Request, Response};
use tower::Service;

const DEFAULT_HEADER: &str = "x-http-method-override";
const DEFAULT_FORM_FIELD: &str = "_method";
const DEFAULT_BODY_LIMIT: usize = 64 * 1024;

/// Configuration for [`MethodOverrideService`]. Implements
/// [`tower::Layer`] so it composes with `ServiceBuilder`.
#[derive(Clone)]
pub struct MethodOverrideLayer {
    cfg: Arc<MethodOverrideConfig>,
}

#[derive(Clone)]
struct MethodOverrideConfig {
    header_name: &'static str,
    form_field: &'static str,
    body_limit: usize,
    allowed: Vec<Method>,
}

impl Default for MethodOverrideLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl MethodOverrideLayer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cfg: Arc::new(MethodOverrideConfig {
                header_name: DEFAULT_HEADER,
                form_field: DEFAULT_FORM_FIELD,
                body_limit: DEFAULT_BODY_LIMIT,
                allowed: vec![Method::PUT, Method::PATCH, Method::DELETE],
            }),
        }
    }

    fn with<F: FnOnce(&mut MethodOverrideConfig)>(mut self, edit: F) -> Self {
        let inner = Arc::make_mut(&mut self.cfg);
        edit(inner);
        self
    }

    #[must_use]
    pub fn header(self, name: &'static str) -> Self {
        self.with(|c| c.header_name = name)
    }

    #[must_use]
    pub fn form_field(self, name: &'static str) -> Self {
        self.with(|c| c.form_field = name)
    }

    #[must_use]
    pub fn body_limit(self, n: usize) -> Self {
        self.with(|c| c.body_limit = n)
    }

    #[must_use]
    pub fn allowed(self, methods: Vec<Method>) -> Self {
        self.with(|c| c.allowed = methods)
    }
}

impl<S> tower::Layer<S> for MethodOverrideLayer {
    type Service = MethodOverrideService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        MethodOverrideService {
            inner,
            cfg: Arc::clone(&self.cfg),
        }
    }
}

/// The wrapped service. Inspects each incoming POST for an override
/// strategy and rewrites the method before forwarding to `inner`.
#[derive(Clone)]
pub struct MethodOverrideService<S> {
    inner: S,
    cfg: Arc<MethodOverrideConfig>,
}

impl<S> Service<Request<Body>> for MethodOverrideService<S>
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
        let cfg = Arc::clone(&self.cfg);
        let mut inner = self.inner.clone();
        Box::pin(async move {
            let req = maybe_rewrite(req, &cfg).await;
            inner.call(req).await
        })
    }
}

async fn maybe_rewrite(req: Request<Body>, cfg: &MethodOverrideConfig) -> Request<Body> {
    if req.method() != Method::POST {
        return req;
    }

    // 1. Header strategy — cheap, no body parse.
    if let Some(target) = header_method(req.headers(), cfg.header_name) {
        if cfg.allowed.contains(&target) {
            return swap_method(req, target);
        }
    }

    // 2. Form-field strategy — parse the body if it's a form payload.
    if is_form_content_type(req.headers()) {
        let (parts, body) = req.into_parts();
        let bytes = match to_bytes(body, cfg.body_limit).await {
            Ok(b) => b,
            Err(_) => {
                // Body too large or stream error — pass through with
                // an empty body since we already consumed it.
                return Request::from_parts(parts, Body::empty());
            }
        };
        if let Some(target) = form_method(&bytes, cfg.form_field) {
            if cfg.allowed.contains(&target) {
                let mut parts = parts;
                parts.method = target;
                return Request::from_parts(parts, Body::from(bytes));
            }
        }
        return Request::from_parts(parts, Body::from(bytes));
    }

    req
}

fn header_method(headers: &HeaderMap, name: &str) -> Option<Method> {
    let raw = headers.get(name)?.to_str().ok()?.trim();
    Method::from_bytes(raw.to_ascii_uppercase().as_bytes()).ok()
}

fn is_form_content_type(headers: &HeaderMap) -> bool {
    let Some(ct) = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let main = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    main == "application/x-www-form-urlencoded"
}

fn form_method(bytes: &[u8], field: &str) -> Option<Method> {
    let body = std::str::from_utf8(bytes).ok()?;
    for pair in body.split('&') {
        let (k, v) = pair.split_once('=')?;
        if percent_decode_eq(k, field) {
            let decoded = percent_decode_string(v);
            return Method::from_bytes(decoded.to_ascii_uppercase().as_bytes()).ok();
        }
    }
    None
}

fn percent_decode_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut bytes = s.bytes().peekable();
    while let Some(b) = bytes.next() {
        if b == b'+' {
            out.push(' ');
        } else if b == b'%' {
            if let (Some(hi), Some(lo)) = (bytes.next(), bytes.next()) {
                if let (Some(h), Some(l)) = (hex(hi), hex(lo)) {
                    out.push(char::from(h * 16 + l));
                    continue;
                }
                out.push('%');
                out.push(hi as char);
                out.push(lo as char);
            } else {
                out.push('%');
            }
        } else {
            out.push(char::from(b));
        }
    }
    out
}

fn percent_decode_eq(encoded: &str, expected: &str) -> bool {
    percent_decode_string(encoded) == expected
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn swap_method(req: Request<Body>, target: Method) -> Request<Body> {
    let (mut parts, body) = req.into_parts();
    parts.method = target;
    Request::from_parts(parts, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::{delete, patch, post, put};
    use axum::Router;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc as StdArc;
    use tower::{Layer, ServiceExt};

    /// Wrap an inner Router with the layer (so routing sees the
    /// rewritten method).
    fn wrap(
        inner: Router,
        layer: MethodOverrideLayer,
    ) -> MethodOverrideService<axum::routing::RouterIntoService<Body>> {
        layer.layer(inner.into_service::<Body>())
    }

    fn router_with_flags() -> (
        Router,
        StdArc<AtomicBool>,
        StdArc<AtomicBool>,
        StdArc<AtomicBool>,
        StdArc<AtomicBool>,
    ) {
        let post_seen = StdArc::new(AtomicBool::new(false));
        let put_seen = StdArc::new(AtomicBool::new(false));
        let patch_seen = StdArc::new(AtomicBool::new(false));
        let delete_seen = StdArc::new(AtomicBool::new(false));

        let p = post_seen.clone();
        let pu = put_seen.clone();
        let pa = patch_seen.clone();
        let d = delete_seen.clone();

        let r = Router::new().route(
            "/r",
            post(move || {
                let p = p.clone();
                async move {
                    p.store(true, Ordering::SeqCst);
                    "post"
                }
            })
            .put({
                let pu = pu.clone();
                move || {
                    let pu = pu.clone();
                    async move {
                        pu.store(true, Ordering::SeqCst);
                        "put"
                    }
                }
            })
            .patch({
                let pa = pa.clone();
                move || {
                    let pa = pa.clone();
                    async move {
                        pa.store(true, Ordering::SeqCst);
                        "patch"
                    }
                }
            })
            .delete({
                let d = d.clone();
                move || {
                    let d = d.clone();
                    async move {
                        d.store(true, Ordering::SeqCst);
                        "delete"
                    }
                }
            }),
        );
        (r, post_seen, put_seen, patch_seen, delete_seen)
    }

    fn req_with_header(method: Method, header: Option<(&str, &str)>) -> Request<Body> {
        let mut b = Request::builder().method(method).uri("/r");
        if let Some((k, v)) = header {
            b = b.header(k, v);
        }
        b.body(Body::empty()).unwrap()
    }

    fn req_form_body(form_body: &str) -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri("/r")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(form_body.to_owned()))
            .unwrap()
    }

    #[tokio::test]
    async fn header_override_post_to_delete() {
        let (router, p, _, _, d) = router_with_flags();
        let svc = wrap(router, MethodOverrideLayer::default());
        let resp = svc
            .oneshot(req_with_header(Method::POST, Some(("x-http-method-override", "DELETE"))))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert!(!p.load(Ordering::SeqCst), "post handler should NOT fire");
        assert!(d.load(Ordering::SeqCst), "delete handler should fire");
    }

    #[tokio::test]
    async fn header_override_post_to_put() {
        let (router, _, pu, _, _) = router_with_flags();
        let svc = wrap(router, MethodOverrideLayer::default());
        let resp = svc
            .oneshot(req_with_header(Method::POST, Some(("x-http-method-override", "PUT"))))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert!(pu.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn header_override_post_to_patch_case_insensitive() {
        let (router, _, _, pa, _) = router_with_flags();
        let svc = wrap(router, MethodOverrideLayer::default());
        let resp = svc
            .oneshot(req_with_header(Method::POST, Some(("x-http-method-override", "patch"))))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert!(pa.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn header_override_to_get_is_ignored() {
        let (router, p, _, _, _) = router_with_flags();
        let svc = wrap(router, MethodOverrideLayer::default());
        let resp = svc
            .oneshot(req_with_header(Method::POST, Some(("x-http-method-override", "GET"))))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert!(p.load(Ordering::SeqCst), "POST handler still fires");
    }

    #[tokio::test]
    async fn form_field_override() {
        let (router, _, _, _, d) = router_with_flags();
        let svc = wrap(router, MethodOverrideLayer::default());
        let resp = svc.oneshot(req_form_body("_method=DELETE&id=42")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert!(d.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn form_field_lowercase_value() {
        let (router, _, _, pa, _) = router_with_flags();
        let svc = wrap(router, MethodOverrideLayer::default());
        let resp = svc.oneshot(req_form_body("_method=patch")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert!(pa.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn form_field_url_encoded_value() {
        let (router, _, pu, _, _) = router_with_flags();
        let svc = wrap(router, MethodOverrideLayer::default());
        let resp = svc.oneshot(req_form_body("_method=P%55T")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert!(pu.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn form_field_unsupported_method_passes_through_as_post() {
        let (router, p, _, _, _) = router_with_flags();
        let svc = wrap(router, MethodOverrideLayer::default());
        let resp = svc.oneshot(req_form_body("_method=GET")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert!(p.load(Ordering::SeqCst), "fall back to POST");
    }

    #[tokio::test]
    async fn header_takes_precedence_over_form() {
        let (router, _, _, pa, d) = router_with_flags();
        let svc = wrap(router, MethodOverrideLayer::default());
        let req = Request::builder()
            .method(Method::POST)
            .uri("/r")
            .header("x-http-method-override", "PATCH")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("_method=DELETE"))
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert!(pa.load(Ordering::SeqCst), "header wins");
        assert!(!d.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn get_is_never_overridden() {
        let r = Router::new().route(
            "/r",
            axum::routing::get(|| async { "get" }).delete(|| async { "delete" }),
        );
        let svc = wrap(r, MethodOverrideLayer::default());
        let resp = svc
            .oneshot(req_with_header(Method::GET, Some(("x-http-method-override", "DELETE"))))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        assert_eq!(&bytes[..], b"get");
    }

    #[tokio::test]
    async fn body_above_limit_passes_through_as_post() {
        let p = StdArc::new(AtomicBool::new(false));
        let pc = p.clone();
        let r = Router::new().route(
            "/r",
            post(move || {
                let pc = pc.clone();
                async move {
                    pc.store(true, Ordering::SeqCst);
                    "post"
                }
            }),
        );
        let svc = wrap(r, MethodOverrideLayer::default().body_limit(8));

        let req = Request::builder()
            .method(Method::POST)
            .uri("/r")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("_method=DELETE&pad=".to_owned() + &"x".repeat(100)))
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert!(p.load(Ordering::SeqCst), "body too large -> handler stays POST");
    }

    #[tokio::test]
    async fn non_form_body_does_not_get_parsed() {
        let p = StdArc::new(AtomicBool::new(false));
        let pc = p.clone();
        let r = Router::new().route(
            "/r",
            post(move || {
                let pc = pc.clone();
                async move {
                    pc.store(true, Ordering::SeqCst);
                    "post"
                }
            }),
        );
        let svc = wrap(r, MethodOverrideLayer::default());
        let req = Request::builder()
            .method(Method::POST)
            .uri("/r")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"_method":"DELETE"}"#))
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert!(p.load(Ordering::SeqCst));
    }

    // -------- pure helpers

    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode_string("hello"), "hello");
        assert_eq!(percent_decode_string("hello%20world"), "hello world");
        assert_eq!(percent_decode_string("hello+world"), "hello world");
        assert_eq!(percent_decode_string("a%21"), "a!");
    }

    #[test]
    fn percent_decode_handles_bad_escape() {
        assert_eq!(percent_decode_string("100%"), "100%");
        assert_eq!(percent_decode_string("a%ZZ"), "a%ZZ");
    }
}
