//! Routing for the HTTP `QUERY` method (RFC 10008).
//!
//! `QUERY` is a GET that carries a body, so long search or filter
//! criteria do not have to fit in a querystring. axum 0.8 cannot route
//! it: [`axum::routing::MethodFilter`] is a closed set
//! (tokio-rs/axum#3799). This module wraps the existing `MethodRouter`
//! in an `any(…)` shim that takes `QUERY` for its own handler and passes
//! every other method back to the wrapped router.
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
//! [`QueryRouterExt::query`] puts the whole existing [`MethodRouter`]
//! inside a new `any(…)` router. The shim sees every request. `QUERY`
//! goes to the query handler. Anything else goes to the wrapped router,
//! which keeps its own methods, fallback and 405 behavior. On a 405 the
//! shim adds `QUERY` to the `Allow` header, so `POST /products` answers
//! `Allow: GET,HEAD,QUERY`.
//!
//! ## Chain `.query()` last
//!
//! A method chained after `.query()` lands on the outer router, which
//! has no `Allow` bookkeeping. `query(h).get(a)` still routes, but its
//! 405 reports `Allow: QUERY` and drops `GET`. Write `get(a).query(h)`.
//!
//! ## Body semantics
//!
//! This module only routes; it does not read bodies. Use
//! `rustango::params::Params` to serve `GET /x?a=1` and `QUERY /x` with
//! body `a=1` from one handler.
//!
//! [`QueryRouterExt::query`]: crate::http_query::QueryRouterExt::query
//! [`MethodRouter`]: axum::routing::MethodRouter

use std::future::Future;
use std::pin::Pin;
use std::sync::LazyLock;

use axum::extract::Request;
use axum::handler::Handler;
use axum::http::header::ALLOW;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::routing::{any, MethodRouter};

/// The `QUERY` method (RFC 10008). `http` 1.x has no constant for it,
/// so use this one: `req.method() == &*http_query::QUERY`.
pub static QUERY: LazyLock<Method> =
    LazyLock::new(|| Method::from_bytes(b"QUERY").expect("QUERY is a valid method token"));

/// Route `QUERY` to `handler`. Other methods get `405` with
/// `Allow: QUERY`.
///
/// This is `axum::routing::get` for the QUERY method. To add QUERY to a
/// route that already has methods, use [`QueryRouterExt::query`], as in
/// `get(list).query(search)`.
pub fn query<H, T, S>(handler: H) -> MethodRouter<S>
where
    H: Handler<T, S>,
    T: 'static,
    S: Clone + Send + Sync + 'static,
{
    MethodRouter::new().query(handler)
}

/// Adds `.query(handler)` to [`MethodRouter`], like axum's `.get(…)`
/// and `.post(…)`.
pub trait QueryRouterExt<S> {
    /// Route `QUERY` on this router to `handler`. Methods and fallbacks
    /// already set keep working, and 405 responses gain `QUERY` in
    /// `Allow`. Chain this **last**; see the module docs.
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
        // `any()`, not `.fallback()`: it turns off the outer router's Allow
        // bookkeeping, so the shim owns the whole response, 405 included.
        any(QueryShim {
            inner: self,
            handler,
        })
    }
}

/// The `any(…)` shim from [`QueryRouterExt::query`]: `QUERY` goes to the
/// user handler, everything else to the wrapped router. It is only
/// cloned where `S: Clone`, which the `Handler` impl below requires.
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
            // Not QUERY: pass it to the wrapped router. Binding the state
            // makes it callable, and its method dispatch, fallback,
            // HEAD-from-GET and 405 handling all run as usual.
            //
            // `with_state` re-boxes the wrapped endpoints on every call.
            // It cannot be hoisted, because `state` is only known here and
            // a cached pre-stated router would share one state across
            // routers cloned before `.with_state(...)`. The cost is a few
            // allocations per non-QUERY request to a `.query()` route.
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

/// Add `QUERY` to a 405's `Allow` list, keeping the methods the wrapped
/// router listed. Uses axum's comma-with-no-space separator.
fn append_query_to_allow(headers: &mut HeaderMap) {
    let Some(existing) = headers.get(ALLOW) else {
        // No Allow header, as on a bare `query(...)` route's 405.
        headers.insert(ALLOW, HeaderValue::from_static("QUERY"));
        return;
    };
    // The empty and duplicate checks need readable text. A value that is
    // not text did not come from axum.
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
    // Append to the raw bytes, so a value set by some other layer that is
    // not readable text is extended rather than dropped.
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

        // QUERY still beats the wrapped router's fallback.
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
