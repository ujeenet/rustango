//! Table-driven HTTP redirects — `django.contrib.redirects`.
//!
//! Hand-roll a [`RedirectMap`] (or load it from a CSV / hard-coded
//! list) and mount [`redirects_middleware`] on your axum router.
//! Matching requests are short-circuited with a `301 Moved Permanently`
//! or `302 Found` response and the canonical URL in the `Location`
//! header. Non-matching requests pass through to the rest of the
//! router untouched.
//!
//! ```ignore
//! use axum::Router;
//! use axum::middleware::from_fn_with_state;
//! use std::sync::Arc;
//! use rustango::redirects::{RedirectMap, redirects_middleware};
//!
//! let map = Arc::new(
//!     RedirectMap::new()
//!         .add_permanent("/old-blog", "/blog")
//!         .add("/foo", "/bar"),
//! );
//!
//! let app = Router::new()
//!     .route("/", get(|| async { "home" }))
//!     .layer(from_fn_with_state(Arc::clone(&map), redirects_middleware));
//! ```
//!
//! ## Match shape
//!
//! Matching is on the request path **including trailing slash
//! variance**. `/old` and `/old/` are distinct entries — register
//! both if you want both to redirect. Query strings are ignored
//! during matching but preserved on the redirect:
//! `/old?ref=ad → /new?ref=ad`. This mirrors Django's behaviour.
//!
//! ## Why not 404-fallthrough?
//!
//! Django's redirects framework hooks into the 404 handler so a
//! redirect only fires if no other view matches. The rustango
//! version runs as middleware *before* routing — same semantic when
//! the redirect entries are for URLs that don't exist on the new
//! site (the common case). For overlap-with-live-routes, register
//! the redirect under a path that doesn't collide.
//!
//! Issue #57 (smaller contrib apps).

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::header::LOCATION;
use axum::http::{HeaderValue, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

// ------------------------------------------------------------------ RedirectRule

/// One redirect entry. `permanent = true` emits 301; `false` emits 302.
#[derive(Debug, Clone)]
pub struct RedirectRule {
    pub to: String,
    pub permanent: bool,
}

// ------------------------------------------------------------------ RedirectMap

/// Path → [`RedirectRule`] map. Cheaply clonable; share via `Arc`
/// when mounted as middleware state.
#[derive(Debug, Default, Clone)]
pub struct RedirectMap {
    rules: HashMap<String, RedirectRule>,
}

impl RedirectMap {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a `302 Found` redirect from `from` → `to`. Chains.
    #[must_use]
    pub fn add(mut self, from: impl Into<String>, to: impl Into<String>) -> Self {
        self.rules.insert(
            from.into(),
            RedirectRule {
                to: to.into(),
                permanent: false,
            },
        );
        self
    }

    /// Add a `301 Moved Permanently` redirect. Use for canonical URL
    /// changes — search engines treat 301 differently from 302.
    #[must_use]
    pub fn add_permanent(mut self, from: impl Into<String>, to: impl Into<String>) -> Self {
        self.rules.insert(
            from.into(),
            RedirectRule {
                to: to.into(),
                permanent: true,
            },
        );
        self
    }

    /// Look up a rule by request path (no query string).
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&RedirectRule> {
        self.rules.get(path)
    }

    /// Number of registered redirects.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Parse a CSV file with the shape `from,to,permanent` (the
    /// third field is `"true"`/`"false"`, treated case-insensitive,
    /// defaulting to `false` when omitted). Lines starting with `#`
    /// and blank lines are skipped.
    pub fn from_csv(csv: &str) -> Self {
        let mut map = Self::new();
        for (line_no, raw) in csv.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.splitn(3, ',').map(str::trim);
            let (Some(from), Some(to)) = (parts.next(), parts.next()) else {
                tracing::warn!(line_no = line_no + 1, "redirect CSV: missing from/to");
                continue;
            };
            let permanent = parts
                .next()
                .map(|s| matches!(s.to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
                .unwrap_or(false);
            map.rules.insert(
                from.to_owned(),
                RedirectRule {
                    to: to.to_owned(),
                    permanent,
                },
            );
        }
        map
    }
}

// ------------------------------------------------------------------ middleware

/// Build a redirect response for the given rule, preserving the
/// original request's query string. Helper for handler-side use; the
/// middleware below uses it internally.
#[must_use]
pub fn build_redirect_response(rule: &RedirectRule, query: Option<&str>) -> Response {
    let mut loc = rule.to.clone();
    if let Some(q) = query {
        if !q.is_empty() {
            loc.push('?');
            loc.push_str(q);
        }
    }
    let status = if rule.permanent {
        StatusCode::MOVED_PERMANENTLY
    } else {
        StatusCode::FOUND
    };
    let mut response = (status, ()).into_response();
    if let Ok(hv) = HeaderValue::from_str(&loc) {
        response.headers_mut().insert(LOCATION, hv);
    }
    response
}

/// axum middleware that consults the [`RedirectMap`] and short-
/// circuits matching requests with a 301/302. Mount via
/// `from_fn_with_state(Arc<RedirectMap>, redirects_middleware)`.
pub async fn redirects_middleware(
    State(map): State<Arc<RedirectMap>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let uri = req.uri().clone();
    if let Some(rule) = map.get(uri.path()) {
        return build_redirect_response(rule, uri.query());
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

    fn app(map: Arc<RedirectMap>) -> Router {
        Router::new()
            .route("/alive", get(|| async { "ok" }))
            .layer(from_fn_with_state(map, redirects_middleware))
    }

    async fn req(app: &Router, path: &str) -> Response {
        app.clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    fn loc(resp: &Response) -> &str {
        resp.headers()
            .get(LOCATION)
            .map(|v| v.to_str().unwrap())
            .unwrap_or("")
    }

    // ---------- RedirectMap surface ----------

    #[test]
    fn map_builder_chains() {
        let m = RedirectMap::new().add("/a", "/A").add_permanent("/b", "/B");
        assert_eq!(m.len(), 2);
        assert!(!m.is_empty());
        assert_eq!(m.get("/a").unwrap().to, "/A");
        assert!(!m.get("/a").unwrap().permanent);
        assert_eq!(m.get("/b").unwrap().to, "/B");
        assert!(m.get("/b").unwrap().permanent);
    }

    #[test]
    fn map_get_misses_return_none() {
        let m = RedirectMap::new().add("/x", "/y");
        assert!(m.get("/z").is_none());
        assert!(
            m.get("/x/").is_none(),
            "trailing-slash variants are distinct"
        );
    }

    #[test]
    fn map_from_csv_parses_lines() {
        let csv = "
# Header comment ignored
/old-blog,/blog,true
/foo,/bar
/baz,/qux,1
/quux,/quark,yes

# Trailing comment also ignored
";
        let m = RedirectMap::from_csv(csv);
        assert_eq!(m.len(), 4);
        assert!(m.get("/old-blog").unwrap().permanent);
        assert!(!m.get("/foo").unwrap().permanent); // default false
        assert!(m.get("/baz").unwrap().permanent); // "1" → true
        assert!(m.get("/quux").unwrap().permanent); // "yes" → true
    }

    #[test]
    fn map_from_csv_skips_malformed_lines() {
        let csv = "
/only-one-field
/good,/dest,true
";
        let m = RedirectMap::from_csv(csv);
        assert_eq!(m.len(), 1);
        assert!(m.get("/good").is_some());
    }

    // ---------- build_redirect_response ----------

    #[test]
    fn build_response_uses_status_and_location() {
        let r = build_redirect_response(
            &RedirectRule {
                to: "/new".into(),
                permanent: true,
            },
            None,
        );
        assert_eq!(r.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(loc(&r), "/new");
    }

    #[test]
    fn build_response_uses_302_for_non_permanent() {
        let r = build_redirect_response(
            &RedirectRule {
                to: "/new".into(),
                permanent: false,
            },
            None,
        );
        assert_eq!(r.status(), StatusCode::FOUND);
    }

    #[test]
    fn build_response_preserves_query_string() {
        let r = build_redirect_response(
            &RedirectRule {
                to: "/new".into(),
                permanent: false,
            },
            Some("ref=email&utm=2025"),
        );
        assert_eq!(loc(&r), "/new?ref=email&utm=2025");
    }

    #[test]
    fn build_response_omits_empty_query() {
        let r = build_redirect_response(
            &RedirectRule {
                to: "/new".into(),
                permanent: false,
            },
            Some(""),
        );
        assert_eq!(loc(&r), "/new", "empty query stripped");
    }

    // ---------- middleware integration ----------

    #[tokio::test]
    async fn middleware_redirects_matching_path() {
        let map = Arc::new(RedirectMap::new().add_permanent("/old", "/new"));
        let app = app(map);
        let r = req(&app, "/old").await;
        assert_eq!(r.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(loc(&r), "/new");
    }

    #[tokio::test]
    async fn middleware_passes_through_unmatched_path() {
        let map = Arc::new(RedirectMap::new().add("/old", "/new"));
        let app = app(map);
        let r = req(&app, "/alive").await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn middleware_preserves_query_on_redirect() {
        let map = Arc::new(RedirectMap::new().add("/old", "/new"));
        let app = app(map);
        let r = req(&app, "/old?utm=test&ref=mail").await;
        assert_eq!(r.status(), StatusCode::FOUND);
        assert_eq!(loc(&r), "/new?utm=test&ref=mail");
    }

    #[tokio::test]
    async fn empty_map_is_a_no_op() {
        let map = Arc::new(RedirectMap::new());
        let app = app(map);
        let r = req(&app, "/alive").await;
        assert_eq!(r.status(), StatusCode::OK);
    }
}
