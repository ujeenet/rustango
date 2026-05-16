//! Static "flat pages" — `django.contrib.flatpages`.
//!
//! Build a [`FlatPageMap`] (path → `FlatPage { title, body }`) and
//! mount [`flatpages_middleware`] on your axum router; matching
//! requests are served directly with `200 OK + text/html`, returning
//! the rendered HTML body. Non-matching requests pass through to the
//! rest of the router untouched. Pairs with
//! [`crate::redirects`] — the same axum-`from_fn_with_state` shape.
//!
//! ```ignore
//! use axum::Router;
//! use axum::middleware::from_fn_with_state;
//! use std::sync::Arc;
//! use rustango::flatpages::{FlatPage, FlatPageMap, flatpages_middleware};
//!
//! let pages = Arc::new(
//!     FlatPageMap::new()
//!         .add("/about", FlatPage::new("About us", "<h1>About</h1>"))
//!         .add("/privacy", FlatPage::new("Privacy", "<h1>Privacy</h1>")),
//! );
//!
//! let app = Router::new()
//!     .route("/", get(|| async { "home" }))
//!     .layer(from_fn_with_state(Arc::clone(&pages), flatpages_middleware));
//! ```
//!
//! ## Body is pre-rendered HTML
//!
//! Each [`FlatPage`] holds a fully-rendered HTML body. The
//! flatpages middleware does NO Tera wrapping — bring your own
//! base template by pre-rendering through Tera and storing the
//! result. Keeps the middleware Tera-free (no Arc<Tera> state
//! plumbing).
//!
//! Per-page `<title>` is exposed via [`FlatPage::title`] so callers
//! can opt into wrapping via a custom handler that reads the
//! `FlatPageMap` directly.
//!
//! Issue #57 (smaller contrib apps).

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderValue, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

// ------------------------------------------------------------------ FlatPage

/// One static page. Body is pre-rendered HTML.
#[derive(Debug, Clone)]
pub struct FlatPage {
    pub title: String,
    pub body: String,
    /// Override the response `Content-Type`. Defaults to
    /// `text/html; charset=utf-8` when `None`.
    pub content_type: Option<String>,
}

impl FlatPage {
    /// Build a flat page from title + body.
    #[must_use]
    pub fn new(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            content_type: None,
        }
    }

    /// Override the response `Content-Type`.
    #[must_use]
    pub fn with_content_type(mut self, ct: impl Into<String>) -> Self {
        self.content_type = Some(ct.into());
        self
    }
}

// ------------------------------------------------------------------ FlatPageMap

/// Path → [`FlatPage`] map. Cheaply clonable; share via `Arc` when
/// mounted as middleware state.
#[derive(Debug, Default, Clone)]
pub struct FlatPageMap {
    pages: HashMap<String, FlatPage>,
}

impl FlatPageMap {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a page at `path`. Replaces any existing page.
    #[must_use]
    pub fn add(mut self, path: impl Into<String>, page: FlatPage) -> Self {
        self.pages.insert(path.into(), page);
        self
    }

    #[must_use]
    pub fn get(&self, path: &str) -> Option<&FlatPage> {
        self.pages.get(path)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.pages.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// Iterate `(path, &FlatPage)` for every page. Handy for
    /// callers that want to render a sitemap of every flat page.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &FlatPage)> {
        self.pages.iter().map(|(k, v)| (k.as_str(), v))
    }
}

// ------------------------------------------------------------------ middleware

/// Build a `200 OK` response carrying the page's body + content-type.
#[must_use]
pub fn build_flatpage_response(page: &FlatPage) -> Response {
    let ct = page
        .content_type
        .as_deref()
        .unwrap_or("text/html; charset=utf-8");
    let mut response = (StatusCode::OK, page.body.clone()).into_response();
    if let Ok(hv) = HeaderValue::from_str(ct) {
        response.headers_mut().insert(CONTENT_TYPE, hv);
    }
    response
}

/// axum middleware that consults the [`FlatPageMap`] and short-
/// circuits matching requests with the page body. Mount via
/// `from_fn_with_state(Arc<FlatPageMap>, flatpages_middleware)`.
pub async fn flatpages_middleware(
    State(pages): State<Arc<FlatPageMap>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if let Some(page) = pages.get(req.uri().path()) {
        return build_flatpage_response(page);
    }
    next.run(req).await
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use axum::middleware::from_fn_with_state;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    fn app(pages: Arc<FlatPageMap>) -> Router {
        Router::new()
            .route("/alive", get(|| async { "ok" }))
            .layer(from_fn_with_state(pages, flatpages_middleware))
    }

    async fn req(app: &Router, path: &str) -> Response {
        app.clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn body_text(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    // ---------- FlatPage / FlatPageMap surface ----------

    #[test]
    fn page_builder_chains_with_content_type() {
        let p =
            FlatPage::new("About", "<h1>About</h1>").with_content_type("text/plain; charset=utf-8");
        assert_eq!(p.title, "About");
        assert_eq!(p.body, "<h1>About</h1>");
        assert_eq!(p.content_type.as_deref(), Some("text/plain; charset=utf-8"));
    }

    #[test]
    fn map_builder_chains() {
        let m = FlatPageMap::new()
            .add("/a", FlatPage::new("A", "body A"))
            .add("/b", FlatPage::new("B", "body B"));
        assert_eq!(m.len(), 2);
        assert!(!m.is_empty());
        assert_eq!(m.get("/a").unwrap().title, "A");
        assert_eq!(m.get("/b").unwrap().body, "body B");
    }

    #[test]
    fn map_iter_yields_every_entry() {
        let m = FlatPageMap::new()
            .add("/a", FlatPage::new("A", "body A"))
            .add("/b", FlatPage::new("B", "body B"));
        let paths: Vec<&str> = m.iter().map(|(p, _)| p).collect();
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&"/a"));
        assert!(paths.contains(&"/b"));
    }

    // ---------- build_flatpage_response ----------

    #[test]
    fn response_status_is_200() {
        let r = build_flatpage_response(&FlatPage::new("t", "b"));
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[test]
    fn response_default_content_type_is_html_utf8() {
        let r = build_flatpage_response(&FlatPage::new("t", "b"));
        let ct = r
            .headers()
            .get(CONTENT_TYPE)
            .map(|v| v.to_str().unwrap())
            .unwrap_or("");
        assert_eq!(ct, "text/html; charset=utf-8");
    }

    #[test]
    fn response_uses_custom_content_type() {
        let r = build_flatpage_response(
            &FlatPage::new("t", "b").with_content_type("application/xhtml+xml"),
        );
        let ct = r
            .headers()
            .get(CONTENT_TYPE)
            .map(|v| v.to_str().unwrap())
            .unwrap_or("");
        assert_eq!(ct, "application/xhtml+xml");
    }

    // ---------- middleware ----------

    #[tokio::test]
    async fn middleware_serves_matching_page() {
        let pages =
            Arc::new(FlatPageMap::new().add("/about", FlatPage::new("About", "<h1>About us</h1>")));
        let app = app(pages);
        let r = req(&app, "/about").await;
        assert_eq!(r.status(), StatusCode::OK);
        let ct = r
            .headers()
            .get(CONTENT_TYPE)
            .map(|v| v.to_str().unwrap())
            .unwrap_or("")
            .to_owned();
        assert_eq!(ct, "text/html; charset=utf-8");
        let body = body_text(r).await;
        assert_eq!(body, "<h1>About us</h1>");
    }

    #[tokio::test]
    async fn middleware_passes_through_unmatched_path() {
        let pages = Arc::new(FlatPageMap::new().add("/about", FlatPage::new("A", "B")));
        let app = app(pages);
        let r = req(&app, "/alive").await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(body_text(r).await, "ok");
    }

    #[tokio::test]
    async fn empty_map_is_a_no_op() {
        let pages = Arc::new(FlatPageMap::new());
        let app = app(pages);
        let r = req(&app, "/alive").await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(body_text(r).await, "ok");
    }

    #[tokio::test]
    async fn middleware_respects_custom_content_type() {
        let pages = Arc::new(
            FlatPageMap::new().add(
                "/robots.txt",
                FlatPage::new("Robots", "User-agent: *\nAllow: /\n")
                    .with_content_type("text/plain; charset=utf-8"),
            ),
        );
        let app = app(pages);
        let r = req(&app, "/robots.txt").await;
        let ct = r
            .headers()
            .get(CONTENT_TYPE)
            .map(|v| v.to_str().unwrap())
            .unwrap_or("")
            .to_owned();
        assert_eq!(ct, "text/plain; charset=utf-8");
        let body = body_text(r).await;
        assert_eq!(body, "User-agent: *\nAllow: /\n");
    }

    #[tokio::test]
    async fn middleware_query_string_does_not_match_bare_path() {
        // `/about?ref=email` should match `/about` (the query is
        // separate from the path component in axum's URI parser).
        let pages = Arc::new(FlatPageMap::new().add("/about", FlatPage::new("About", "X")));
        let app = app(pages);
        let r = req(&app, "/about?ref=email").await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(body_text(r).await, "X");
    }
}
