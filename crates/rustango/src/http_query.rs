//! HTTP `QUERY` method routing — RFC 10008 (issue #1108, epic #1107).
//!
//! `QUERY` is the "safe GET with a body": safe + idempotent like GET, but
//! the query payload travels in the request body, so complex search /
//! filter criteria don't have to squeeze into a querystring. axum 0.8
//! can't route it natively — [`axum::routing::MethodFilter`] is a closed
//! set (tokio-rs/axum#3799) — so this module wraps the existing
//! `MethodRouter` in an `any(…)` shim that peels `QUERY` off for its own
//! handler and delegates every other method back to the wrapped router.
//!
//! ```ignore
//! use rustango::http_query::{query, QueryRouterExt};
//! use axum::routing::get;
//!
//! let app = axum::Router::new()
//!     // QUERY-only route.
//!     .route("/search", query(search))
//!     // GET + QUERY on one path — same handler shape as axum's own
//!     // chaining. Chain `.query()` LAST (see below).
//!     .route("/products", get(list_products).query(search_products));
//! ```
//!
//! ## How it works
//!
//! [`QueryRouterExt::query`] wraps the *entire* existing [`MethodRouter`]
//! inside a fresh `any(…)` router. The shim receives every request:
//! `QUERY` goes to the query handler; anything else is delegated to the
//! wrapped router, which dispatches its registered methods (and its own
//! fallback / default 405) exactly as before. A 405 coming back from the
//! wrapped router gets `QUERY` appended to its `Allow` header, so
//! `POST /products` correctly answers `Allow: GET,HEAD,QUERY` (axum emits
//! the method list comma-separated with no spaces).
//!
//! ## Chain `.query()` last
//!
//! Methods chained *after* `.query()` register on the outer router, which
//! skips axum's `Allow` bookkeeping — `query(h).get(a)` routes correctly
//! but 405s report `Allow: QUERY` without `GET`. `get(a).query(h)` is
//! always right. When axum lands native QUERY support (axum#3799) the
//! internals here can switch to it without changing this call surface.
//!
//! ## Body semantics
//!
//! This module routes; it does not read bodies. Pair with the unified
//! params extractor (#1109) to serve `GET /x?a=1` and `QUERY /x` with
//! body `a=1` from one handler.

use std::future::Future;
use std::pin::Pin;
use std::sync::LazyLock;

use axum::extract::Request;
use axum::handler::Handler;
use axum::http::header::ALLOW;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::routing::{any, MethodRouter};

/// The `QUERY` method (RFC 10008). `http` 1.x has no built-in constant,
/// so this is the crate-wide extension-method singleton — compare with
/// `req.method() == &*http_query::QUERY`.
pub static QUERY: LazyLock<Method> =
    LazyLock::new(|| Method::from_bytes(b"QUERY").expect("QUERY is a valid method token"));

/// Route `QUERY` requests to the given handler; every other method gets
/// `405 Method Not Allowed` with `Allow: QUERY`.
///
/// The rustango equivalent of `axum::routing::get` for the QUERY method.
/// Additional methods can be attached with [`QueryRouterExt::query`] on
/// an existing router instead — `get(list).query(search)`.
pub fn query<H, T, S>(handler: H) -> MethodRouter<S>
where
    H: Handler<T, S>,
    T: 'static,
    S: Clone + Send + Sync + 'static,
{
    MethodRouter::new().query(handler)
}

/// Adds `.query(handler)` to [`MethodRouter`], mirroring axum's own
/// `.get(…)` / `.post(…)` chaining for the RFC 10008 `QUERY` method.
pub trait QueryRouterExt<S> {
    /// Route `QUERY` requests on this router to `handler`. All previously
    /// registered methods (and any custom fallback) keep working; 405
    /// responses gain `QUERY` in their `Allow` header. Chain this **last**
    /// — see the module docs.
    fn query<H, T>(self, handler: H) -> MethodRouter<S>
    where
        H: Handler<T, S>,
        T: 'static;
}

impl<S> QueryRouterExt<S> for MethodRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    fn query<H, T>(self, handler: H) -> MethodRouter<S>
    where
        H: Handler<T, S>,
        T: 'static,
    {
        // `any()` (rather than `.fallback()`) because it marks the outer
        // router's Allow bookkeeping as "skip": the shim fully owns the
        // response, including the corrected Allow header on 405s.
        any(QueryShim {
            inner: self,
            handler,
        })
    }
}

/// The `any(…)` shim installed by [`QueryRouterExt::query`]: dispatches
/// `QUERY` to the user handler and delegates every other method to the
/// wrapped router. Only ever cloned where `S: Clone` (the `Handler` impl
/// below requires it), so the derived bound is always satisfied.
#[derive(Clone)]
struct QueryShim<H, S> {
    inner: MethodRouter<S>,
    handler: H,
}

impl<H, T, S> Handler<T, S> for QueryShim<H, S>
where
    H: Handler<T, S>,
    T: 'static,
    S: Clone + Send + Sync + 'static,
{
    type Future = Pin<Box<dyn Future<Output = Response> + Send>>;

    fn call(self, req: Request, state: S) -> Self::Future {
        Box::pin(async move {
            // `&QUERY` deref-coerces the `LazyLock` to `&Method`.
            let query: &Method = &QUERY;
            if req.method() == query {
                return self.handler.call(req, state).await;
            }
            // Not QUERY — hand the request to the wrapped router. Binding
            // the state turns it into a ready-to-call service; its own
            // method dispatch, custom fallback, HEAD-from-GET handling,
            // and default-405 Allow bookkeeping all run unchanged.
            //
            // `with_state` re-boxes the wrapped router's handler endpoints
            // on each call. It can't be hoisted: `state` is only known
            // here, and caching a pre-stated router in the shim would alias
            // one state across routers cloned before `.with_state(...)` (a
            // supported axum pattern). The cost is a couple of allocations
            // per non-QUERY request to a `.query()` route (tracked in #1115);
            // it retires with the shim once axum routes QUERY natively
            // (axum#3799).
            let svc: MethodRouter<(), std::convert::Infallible> = self.inner.with_state(state);
            let mut resp = match tower::ServiceExt::oneshot(svc, req).await {
                Ok(resp) => resp,
                Err(never) => match never {},
            };
            if resp.status() == StatusCode::METHOD_NOT_ALLOWED {
                append_query_to_allow(resp.headers_mut());
            }
            resp
        })
    }
}

/// Add `QUERY` to a 405's `Allow` list, preserving what the wrapped
/// router reported for its native methods. Uses the same comma-no-space
/// separator axum emits (`method_routing::append_allow_header`).
fn append_query_to_allow(headers: &mut HeaderMap) {
    let Some(existing) = headers.get(ALLOW) else {
        // No Allow at all (e.g. a bare `query(...)` route's 405) — just
        // advertise QUERY.
        headers.insert(ALLOW, HeaderValue::from_static("QUERY"));
        return;
    };
    // The empty / already-listed checks only apply when the value reads
    // as text; a non-text value is never one axum produced.
    if let Ok(text) = existing.to_str() {
        if text.trim().is_empty() {
            headers.insert(ALLOW, HeaderValue::from_static("QUERY"));
            return;
        }
        if text
            .split(',')
            .any(|m| m.trim().eq_ignore_ascii_case("QUERY"))
        {
            return;
        }
    }
    // Append to the raw bytes so an existing value we can't read as text
    // (some custom layer's doing) is extended, never silently dropped.
    let mut bytes = existing.as_bytes().to_vec();
    bytes.extend_from_slice(b",QUERY");
    if let Ok(v) = HeaderValue::from_bytes(&bytes) {
        headers.insert(ALLOW, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    async fn send(app: Router, method: &str, path: &str, body: &str) -> Response {
        let req = Request::builder()
            .method(Method::from_bytes(method.as_bytes()).unwrap())
            .uri(path)
            .body(Body::from(body.to_owned()))
            .unwrap();
        app.oneshot(req).await.unwrap()
    }

    async fn body_string(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn allow_header(resp: &Response) -> Vec<String> {
        resp.headers()
            .get(ALLOW)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(',')
            .map(|m| m.trim().to_owned())
            .filter(|m| !m.is_empty())
            .collect()
    }

    async fn echo(body: String) -> String {
        format!("query:{body}")
    }

    async fn list() -> &'static str {
        "list"
    }

    #[tokio::test]
    async fn query_route_reaches_handler_with_body() {
        let app = Router::new().route("/search", query(echo));
        let resp = send(app, "QUERY", "/search", "a=1").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_string(resp).await, "query:a=1");
    }

    #[tokio::test]
    async fn get_and_query_coexist_on_one_route() {
        let app = Router::new().route("/products", get(list).query(echo));

        let resp = send(app.clone(), "GET", "/products", "").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_string(resp).await, "list");

        let resp = send(app, "QUERY", "/products", "q=x").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_string(resp).await, "query:q=x");
    }

    #[tokio::test]
    async fn head_still_served_from_get_without_body() {
        let app = Router::new().route("/products", get(list).query(echo));
        let resp = send(app, "HEAD", "/products", "").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_string(resp).await, "");
    }

    #[tokio::test]
    async fn unmatched_method_gets_405_with_full_allow_list() {
        let app = Router::new().route("/products", get(list).query(echo));
        let resp = send(app, "POST", "/products", "").await;
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        let allow = allow_header(&resp);
        assert!(allow.contains(&"GET".to_owned()), "allow: {allow:?}");
        assert!(allow.contains(&"HEAD".to_owned()), "allow: {allow:?}");
        assert!(allow.contains(&"QUERY".to_owned()), "allow: {allow:?}");
    }

    #[tokio::test]
    async fn query_only_route_405s_get_with_allow_query() {
        let app = Router::new().route("/search", query(echo));
        let resp = send(app, "GET", "/search", "").await;
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(allow_header(&resp), vec!["QUERY".to_owned()]);
    }

    #[tokio::test]
    async fn custom_fallback_on_wrapped_router_is_preserved() {
        async fn custom() -> (StatusCode, &'static str) {
            (StatusCode::IM_A_TEAPOT, "custom")
        }
        let app = Router::new().route("/x", get(list).fallback(custom).query(echo));

        // QUERY still wins over the wrapped router's fallback.
        let resp = send(app.clone(), "QUERY", "/x", "q").await;
        assert_eq!(body_string(resp).await, "query:q");

        // Other unmatched methods reach the custom fallback untouched.
        let resp = send(app, "POST", "/x", "").await;
        assert_eq!(resp.status(), StatusCode::IM_A_TEAPOT);
    }

    #[tokio::test]
    async fn works_with_router_state() {
        #[derive(Clone)]
        struct AppState(&'static str);

        async fn stateful(
            axum::extract::State(s): axum::extract::State<AppState>,
            body: String,
        ) -> String {
            format!("{}:{body}", s.0)
        }

        let app: Router = Router::new()
            .route("/s", get(list).query(stateful))
            .with_state(AppState("st"));
        let resp = send(app, "QUERY", "/s", "b").await;
        assert_eq!(body_string(resp).await, "st:b");
    }

    #[tokio::test]
    async fn query_method_singleton_parses() {
        assert_eq!(QUERY.as_str(), "QUERY");
    }

    #[test]
    fn append_allow_covers_absent_dedup_and_append() {
        let allow = |init: Option<&str>| {
            let mut h = HeaderMap::new();
            if let Some(v) = init {
                h.insert(ALLOW, HeaderValue::from_str(v).unwrap());
            }
            append_query_to_allow(&mut h);
            h.get(ALLOW)
                .map(|v| v.to_str().unwrap().to_owned())
                .unwrap_or_default()
        };

        assert_eq!(allow(None), "QUERY", "absent → advertise QUERY");
        assert_eq!(allow(Some("")), "QUERY", "empty → advertise QUERY");
        assert_eq!(
            allow(Some("GET,HEAD")),
            "GET,HEAD,QUERY",
            "append, no space"
        );
        assert_eq!(
            allow(Some("GET,QUERY")),
            "GET,QUERY",
            "already listed → no dup"
        );
        assert_eq!(
            allow(Some("GET, query")),
            "GET, query",
            "case-insensitive dedup, existing formatting preserved"
        );
    }
}
