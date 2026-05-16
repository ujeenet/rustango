//! Django-shape access decorators — `login_required` middleware +
//! `?next=` round-trip helpers. Issue #11.
//!
//! Gates a handler so anonymous requests get 302'd to a login URL
//! with the original URL preserved in `?next=`. After the user
//! authenticates, the login handler reads `?next=` and 302s back —
//! the canonical "click-through-then-resume" UX every Django app
//! ships.
//!
//! ## Wiring
//!
//! ```ignore
//! use rustango::auth_decorators::login_required;
//!
//! let app = Router::new()
//!     .route("/", get(home))                         // public
//!     .route("/profile", get(profile))               // gated
//!     .route("/settings", get(settings))             // gated
//!     .layer(login_required("/login"))               // protects every route below
//!     .route("/about", get(about));                  // public (added after layer)
//! ```
//!
//! Or scope the gate to a sub-router (the more idiomatic shape):
//!
//! ```ignore
//! let private = Router::new()
//!     .route("/profile", get(profile))
//!     .route("/settings", get(settings))
//!     .layer(login_required("/login"));
//!
//! let app = Router::new()
//!     .route("/", get(home))
//!     .merge(private);
//! ```
//!
//! ## Login-handler side
//!
//! The login handler reads `?next=` to know where to send the user
//! after a successful auth. Use [`extract_next`] for the read +
//! [`safe_next`] to defend against open-redirect attacks:
//!
//! ```ignore
//! async fn login_post(Query(q): Query<HashMap<String, String>>, …) -> Response {
//!     // … verify credentials …
//!     let next = auth_decorators::extract_next(&q)
//!         .and_then(|n| auth_decorators::safe_next(&n))
//!         .unwrap_or_else(|| "/".to_owned());
//!     Redirect::to(&next).into_response()
//! }
//! ```
//!
//! ## Out of scope (queued as follow-ups)
//!
//! - `permission_required(perm)` — needs the tenancy permission
//!   registry; pairs naturally with `login_required` but warrants its
//!   own slice once the auth-flow API stabilizes.
//! - `user_passes_test(predicate)` — generic predicate gate; useful
//!   once we have a tighter `SessionUser` API.
//! - `LoginRequiredMixin` on the CBV surface — small wiring on
//!   `TemplateView` / `DetailView` etc.
//! - Single-tenant / non-tenancy variant — `login_required` today
//!   pulls `SessionUser` which is tenancy-shaped. Apps using
//!   non-session auth (JWT, basic auth) plug their own predicate.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

/// Configuration for [`login_required`]. Defaults match Django:
/// `login_url = "/login"`, `redirect_field = "next"`.
#[derive(Debug, Clone)]
pub struct LoginRequiredConfig {
    /// Where to send anonymous users. Default `"/login"`.
    pub login_url: String,
    /// Query-param name carrying the original URL. Default `"next"`.
    pub redirect_field: String,
}

impl Default for LoginRequiredConfig {
    fn default() -> Self {
        Self {
            login_url: "/login".into(),
            redirect_field: "next".into(),
        }
    }
}

/// Construct an [`axum::middleware::FromFnLayer`] that 302s
/// anonymous requests to `login_url` with the original URL preserved
/// in `?next=`. Equivalent to Django's
/// `@login_required(login_url=login_url)`. Issue #11.
///
/// "Anonymous" means `SessionUser.0.is_none()` — the existing tenancy
/// extractor returns `None` when no session cookie is present or the
/// cookie is for a different tenant. Apps using non-session auth
/// (JWT, basic) need a different gating shape (queued).
///
/// See the module-level docs for full wiring + login-handler examples.
#[cfg(all(feature = "tenancy", feature = "postgres"))]
pub fn login_required(
    login_url: impl Into<String>,
) -> impl tower::Layer<
    axum::routing::Route,
    Service = impl tower::Service<
        Request<Body>,
        Response = Response,
        Error = std::convert::Infallible,
        Future = impl Send + 'static,
    > + Clone
                  + Send
                  + Sync
                  + 'static,
> + Clone {
    let cfg = Arc::new(LoginRequiredConfig {
        login_url: login_url.into(),
        ..Default::default()
    });
    axum::middleware::from_fn(move |req: Request<Body>, next: Next| {
        let cfg = cfg.clone();
        async move { handle_login_required(cfg, req, next).await }
    })
}

#[cfg(all(feature = "tenancy", feature = "postgres"))]
async fn handle_login_required(
    cfg: Arc<LoginRequiredConfig>,
    req: Request<Body>,
    next: Next,
) -> Response {
    use axum::extract::FromRequestParts as _;
    // Run the SessionUser extractor manually so we don't have to pass
    // it as a `from_fn` argument (which would require SessionUser to
    // implement `FromRequest`, not just `FromRequestParts`).
    let (mut parts, body) = req.into_parts();
    let user = crate::extractors::SessionUser::from_request_parts(&mut parts, &())
        .await
        .unwrap_or(crate::extractors::SessionUser(None));
    if user.0.is_some() {
        let req = Request::from_parts(parts, body);
        return next.run(req).await;
    }
    // Anonymous — build the 302 target from the original URL.
    let original = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "/".to_owned());
    redirect_to_login(&cfg.login_url, &cfg.redirect_field, &original)
}

/// Build the 302 response that points at `login_url` with `?next=`
/// carrying the URL-encoded `original` path. Surfaces as `pub` so
/// non-session auth flavors that gate handlers manually can reuse
/// the same redirect shape.
#[must_use]
pub fn redirect_to_login(login_url: &str, redirect_field: &str, original: &str) -> Response {
    let target = build_login_url(login_url, redirect_field, original);
    let mut res = Response::builder()
        .status(StatusCode::FOUND)
        .body(Body::empty())
        .expect("302 + empty body is always valid");
    if let Ok(v) = HeaderValue::from_str(&target) {
        res.headers_mut().insert(header::LOCATION, v);
    }
    res
}

fn build_login_url(login_url: &str, redirect_field: &str, original: &str) -> String {
    let encoded = crate::url_codec::url_encode(original);
    let sep = if login_url.contains('?') { '&' } else { '?' };
    format!("{login_url}{sep}{redirect_field}={encoded}")
}

// ------------------------------------------------------------------ login-handler helpers

/// Read the `next` query parameter out of a `Query<HashMap<...>>`-like
/// map. Returns `None` when absent or empty (callers fall back to a
/// default destination — typically `"/"`).
#[must_use]
pub fn extract_next(query: &HashMap<String, String>) -> Option<String> {
    extract_next_named(query, "next")
}

/// Same as [`extract_next`] but with a custom field name to match a
/// non-default [`LoginRequiredConfig::redirect_field`].
#[must_use]
pub fn extract_next_named(query: &HashMap<String, String>, field: &str) -> Option<String> {
    query
        .get(field)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Open-redirect defense: returns `Some(next)` only when `next` is
/// safe to redirect to — a same-origin, root-relative path. Rejects:
///
/// - Scheme-prefixed URLs (`http://evil.example/`, `//evil.example/x`)
/// - Backslash variants the browser normalizes to a host
///   (`/\evil.example/x`)
/// - Percent-encoded variants of the above
///   (`%2F%2Fevil.example/x` decodes to `//evil.example/x`)
/// - Empty / whitespace
///
/// ```rust
/// use rustango::auth_decorators::safe_next;
/// assert_eq!(safe_next("/profile"), Some("/profile".to_owned()));
/// assert_eq!(safe_next("http://evil.example/x"), None);
/// assert_eq!(safe_next("//evil.example/x"), None);
/// assert_eq!(safe_next("/\\evil.example/x"), None);
/// assert_eq!(safe_next("%2F%2Fevil.example/x"), None);  // decoded → //evil
/// assert_eq!(safe_next(""), None);
/// ```
///
/// **Does NOT validate** that the path resolves to a real route —
/// that's a feature, not a bug. A 404 after login is far better than
/// shipping a phishing redirect through your auth handler.
#[must_use]
pub fn safe_next(next: &str) -> Option<String> {
    let trimmed = next.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Percent-decode first so encoded bypass attempts
    // (`%2F%2Fevil.example/x` → `//evil.example/x`) are caught by the
    // string-shape checks below. We discard the decoded form after
    // validation and return the original trimmed input — that way
    // double-encoded sequences in legitimate paths survive the
    // redirect intact.
    let decoded = crate::url_codec::url_decode(trimmed);
    if !is_safe_path(&decoded) {
        return None;
    }
    // Original may differ from decoded if it contained percent-escapes;
    // both forms must be path-shaped.
    if !is_safe_path(trimmed) {
        return None;
    }
    Some(trimmed.to_owned())
}

/// Shared check used on both the raw and percent-decoded forms of
/// the `next` value. Path must start with `/` and must NOT start
/// with `//` (scheme-relative) or `/\` (backslash-host).
fn is_safe_path(s: &str) -> bool {
    if !s.starts_with('/') {
        return false;
    }
    if s.starts_with("//") {
        return false;
    }
    if s.starts_with("/\\") {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_login_url_appends_next_as_query() {
        let url = build_login_url("/login", "next", "/protected");
        assert_eq!(url, "/login?next=%2Fprotected");
    }

    #[test]
    fn build_login_url_uses_ampersand_when_login_already_has_query() {
        let url = build_login_url("/login?foo=1", "next", "/protected");
        assert_eq!(url, "/login?foo=1&next=%2Fprotected");
    }

    #[test]
    fn build_login_url_url_encodes_path_with_special_chars() {
        let url = build_login_url("/login", "next", "/posts/hello world?page=2");
        // Space → %20, `?` → %3F, `=` → %3D so the inner query is
        // safely captured as a single `next` value.
        assert!(url.contains("%20"));
        assert!(url.contains("%3F") || url.contains("%26") || url.contains("page%3D2"));
    }

    #[test]
    fn redirect_to_login_returns_302_with_location() {
        let res = redirect_to_login("/login", "next", "/profile");
        assert_eq!(res.status(), StatusCode::FOUND);
        let loc = res
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap();
        assert_eq!(loc, "/login?next=%2Fprofile");
    }

    #[test]
    fn redirect_to_login_drops_location_on_crlf_attempt() {
        // CRLF in original URL is a response-splitting vector. The
        // builder percent-encodes the value before insertion so no
        // raw CRLF reaches the header — that's the safety invariant
        // worth asserting (not the specific encoded form, which is
        // a url_encode implementation detail).
        let res = redirect_to_login("/login", "next", "/profile\r\nSet-Cookie: pwned=1");
        assert_eq!(res.status(), StatusCode::FOUND);
        let loc = res
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap();
        assert!(!loc.contains('\r'), "raw \\r in Location header: {loc}");
        assert!(!loc.contains('\n'), "raw \\n in Location header: {loc}");
    }

    #[test]
    fn extract_next_returns_value_when_present() {
        let mut q = HashMap::new();
        q.insert("next".to_owned(), "/profile".to_owned());
        assert_eq!(extract_next(&q), Some("/profile".to_owned()));
    }

    #[test]
    fn extract_next_returns_none_when_absent_or_empty() {
        assert_eq!(extract_next(&HashMap::new()), None);
        let mut q = HashMap::new();
        q.insert("next".to_owned(), "".to_owned());
        assert_eq!(extract_next(&q), None);
        q.insert("next".to_owned(), "   ".to_owned());
        assert_eq!(extract_next(&q), None);
    }

    #[test]
    fn extract_next_named_uses_custom_field() {
        let mut q = HashMap::new();
        q.insert("redirect_to".to_owned(), "/profile".to_owned());
        assert_eq!(extract_next(&q), None);
        assert_eq!(
            extract_next_named(&q, "redirect_to"),
            Some("/profile".to_owned())
        );
    }

    #[test]
    fn safe_next_accepts_root_relative_paths() {
        assert_eq!(safe_next("/profile"), Some("/profile".to_owned()));
        assert_eq!(safe_next("/posts/42"), Some("/posts/42".to_owned()));
        assert_eq!(
            safe_next("/search?q=hello"),
            Some("/search?q=hello".to_owned())
        );
    }

    #[test]
    fn safe_next_rejects_absolute_urls() {
        assert_eq!(safe_next("http://evil.example/x"), None);
        assert_eq!(safe_next("https://evil.example/x"), None);
        assert_eq!(safe_next("ftp://evil.example/"), None);
        // Scheme-relative (network-path reference) is a phishing
        // vector — browsers route this to <host>/x.
        assert_eq!(safe_next("//evil.example/x"), None);
    }

    #[test]
    fn safe_next_rejects_backslash_variant() {
        // `/\evil.example/x` — some browsers normalize the backslash
        // to a forward slash, producing `//evil.example/x` which
        // routes to the attacker's host.
        assert_eq!(safe_next("/\\evil.example/x"), None);
    }

    #[test]
    fn safe_next_rejects_percent_encoded_bypass() {
        // The raw `next` value passes the path-starts-with-`/` check
        // but percent-decodes to `//evil.example/x` — a phishing
        // redirect. Defense: decode before validating.
        assert_eq!(safe_next("%2F%2Fevil.example/x"), None);
        assert_eq!(safe_next("%2f%2fevil.example/x"), None);
        // Double-slash with the second slash encoded.
        assert_eq!(safe_next("/%2Fevil.example/x"), None);
        // Backslash variant percent-encoded.
        assert_eq!(safe_next("/%5Cevil.example/x"), None);
    }

    #[test]
    fn safe_next_accepts_legitimate_percent_encodes_in_path() {
        // A path containing literal `%20` (encoded space) should still
        // be valid — `safe_next` doesn't reject every percent-encode,
        // just the ones that decode to host-routing patterns.
        assert_eq!(
            safe_next("/profile/hello%20world"),
            Some("/profile/hello%20world".to_owned())
        );
    }

    #[test]
    fn safe_next_rejects_empty_and_whitespace() {
        assert_eq!(safe_next(""), None);
        assert_eq!(safe_next("   "), None);
        assert_eq!(safe_next("\t\n"), None);
    }

    #[test]
    fn safe_next_strips_surrounding_whitespace() {
        assert_eq!(safe_next("  /profile  "), Some("/profile".to_owned()));
    }

    #[cfg(all(feature = "tenancy", feature = "postgres"))]
    #[tokio::test]
    async fn login_required_layer_redirects_anonymous_to_login_url() {
        use axum::body::Body;
        use axum::routing::get;
        use axum::Router;
        use tower::ServiceExt as _;

        async fn protected() -> &'static str {
            "secret"
        }

        let app = Router::new()
            .route("/profile", get(protected))
            .layer(login_required("/login"));

        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/profile")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // No SessionUser → no TenantContext → extractor returns None
        // → middleware 302s to /login?next=/profile.
        assert_eq!(res.status(), StatusCode::FOUND);
        let loc = res
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap();
        assert_eq!(loc, "/login?next=%2Fprofile");
    }
}
