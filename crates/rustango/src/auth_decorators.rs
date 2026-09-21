//! Django-shape access decorators: `login_required` middleware and
//! `?next=` round-trip helpers.
//!
//! An anonymous request is redirected to a login URL with the original
//! URL kept in `?next=`. After the user signs in, the login handler
//! reads `?next=` and sends them back where they were going.
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
//! Or scope the gate to a sub-router, which is usually cleaner:
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
//! The login handler reads `?next=` to know where to send the user.
//! Read it with [`extract_next`], then pass it through [`safe_next`].
//! Skipping `safe_next` turns your login handler into an open
//! redirect an attacker can use for phishing.
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
//! ## Scope
//!
//! Every gate here reads `SessionUser`, so they need the `tenancy`
//! feature and session cookies. Apps on JWT or basic auth should
//! write their own gate and reuse [`redirect_to_login`] and
//! [`safe_next`].
//!
//! [`redirect_to_login`]: crate::auth_decorators::redirect_to_login
//! [`safe_next`]: crate::auth_decorators::safe_next
//! [`extract_next`]: crate::auth_decorators::extract_next

use std::collections::HashMap;
#[cfg(feature = "tenancy")]
use std::sync::Arc;

use axum::body::Body;
#[cfg(feature = "tenancy")]
use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
#[cfg(feature = "tenancy")]
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

/// Middleware layer that redirects anonymous requests to `login_url`,
/// keeping the original URL in `?next=`. Django's
/// `@login_required(login_url=...)`.
///
/// "Anonymous" means `SessionUser` resolved to `None`: no session
/// cookie, or a cookie for a different tenant.
///
/// See the module docs for wiring and login-handler examples.
#[cfg(feature = "tenancy")]
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

/// Predicate-based access gate. Django's
/// `@user_passes_test(test_func, login_url=...)`.
///
/// `predicate` runs against the [`crate::tenancy::auth::User`] row, so
/// it can read any field. On `true` the request continues. On `false`,
/// or when the request is anonymous, it redirects to `login_url` with
/// `?next=`, like [`login_required`].
///
/// ```ignore
/// use rustango::auth_decorators::user_passes_test;
///
/// // Superuser-only sub-router:
/// let admin_only = Router::new()
///     .route("/admin/dashboard", get(dashboard))
///     .layer(user_passes_test("/login", |u| u.is_superuser));
/// ```
///
/// Anonymous requests never reach the predicate: they redirect
/// straight away. You do not need to add [`login_required`] as well.
#[cfg(feature = "tenancy")]
pub fn user_passes_test<F>(
    login_url: impl Into<String>,
    predicate: F,
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
> + Clone
where
    F: Fn(&crate::tenancy::auth::User) -> bool + Send + Sync + 'static,
{
    let cfg = Arc::new(LoginRequiredConfig {
        login_url: login_url.into(),
        ..Default::default()
    });
    let pred = Arc::new(predicate);
    axum::middleware::from_fn(move |req: Request<Body>, next: Next| {
        let cfg = cfg.clone();
        let pred = pred.clone();
        async move { handle_user_passes_test(cfg, pred, req, next).await }
    })
}

#[cfg(feature = "tenancy")]
async fn handle_user_passes_test<F>(
    cfg: Arc<LoginRequiredConfig>,
    pred: Arc<F>,
    req: Request<Body>,
    next: Next,
) -> Response
where
    F: Fn(&crate::tenancy::auth::User) -> bool + Send + Sync + 'static,
{
    use axum::extract::FromRequestParts as _;
    let (mut parts, body) = req.into_parts();
    let user = crate::extractors::SessionUser::from_request_parts(&mut parts, &())
        .await
        .unwrap_or(crate::extractors::SessionUser(None));
    if let Some(u) = user.0.as_ref() {
        if pred(u) {
            let req = Request::from_parts(parts, body);
            return next.run(req).await;
        }
    }
    // Anonymous, or the predicate said no: redirect to login.
    let original = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "/".to_owned());
    redirect_to_login(&cfg.login_url, &cfg.redirect_field, &original)
}

/// Like [`user_passes_test`], but returns `403 Forbidden` instead of
/// redirecting. Use it for JSON APIs, where a 302 to an HTML login
/// page is no use to the client.
///
/// An anonymous request gets 401 so the client can tell "sign in
/// first" apart from "signed in, but not allowed".
///
/// ```ignore
/// use rustango::auth_decorators::user_passes_test_or_403;
///
/// let api = Router::new()
///     .route("/api/admin/stats", get(stats))
///     .layer(user_passes_test_or_403(|u| u.is_superuser));
/// ```
#[cfg(feature = "tenancy")]
pub fn user_passes_test_or_403<F>(
    predicate: F,
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
> + Clone
where
    F: Fn(&crate::tenancy::auth::User) -> bool + Send + Sync + 'static,
{
    let pred = Arc::new(predicate);
    axum::middleware::from_fn(move |req: Request<Body>, next: Next| {
        let pred = pred.clone();
        async move { handle_user_passes_test_or_403(pred, req, next).await }
    })
}

#[cfg(feature = "tenancy")]
async fn handle_user_passes_test_or_403<F>(pred: Arc<F>, req: Request<Body>, next: Next) -> Response
where
    F: Fn(&crate::tenancy::auth::User) -> bool + Send + Sync + 'static,
{
    use axum::extract::FromRequestParts as _;
    let (mut parts, body) = req.into_parts();
    let user = crate::extractors::SessionUser::from_request_parts(&mut parts, &())
        .await
        .unwrap_or(crate::extractors::SessionUser(None));
    match user.0.as_ref() {
        None => Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body(Body::empty())
            .expect("401 + empty body is always valid"),
        Some(u) if pred(u) => {
            let req = Request::from_parts(parts, body);
            next.run(req).await
        }
        Some(_) => Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Body::empty())
            .expect("403 + empty body is always valid"),
    }
}

/// Gate that returns 401 for anonymous requests instead of
/// redirecting. The API counterpart to [`login_required`].
#[cfg(feature = "tenancy")]
pub fn login_required_or_401() -> impl tower::Layer<
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
    user_passes_test_or_403(|_| true)
}

/// Gate a route to active superusers only, that is
/// `is_superuser && active`. Using this instead of a hand-written
/// predicate keeps every call site agreeing that a deactivated
/// superuser is locked out.
///
/// ```ignore
/// use rustango::auth_decorators::superuser_required;
///
/// let admin_routes = Router::new()
///     .route("/admin/dashboard", get(dashboard))
///     .layer(superuser_required("/login"));
/// ```
#[cfg(feature = "tenancy")]
pub fn superuser_required(
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
    user_passes_test(login_url, |u| u.is_superuser && u.active)
}

/// API variant of [`superuser_required`]: 401 for anonymous, 403 for
/// anyone who is not an active superuser.
#[cfg(feature = "tenancy")]
pub fn superuser_required_or_403() -> impl tower::Layer<
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
    user_passes_test_or_403(|u| u.is_superuser && u.active)
}

/// Gate a route to active users only. Anonymous sessions and
/// deactivated accounts (`active = false`) are redirected to
/// `login_url`.
///
/// The `SessionUser` extractor already drops inactive accounts, so
/// this is a second layer. Use it when a route must state the
/// active-only rule at the call site.
#[cfg(feature = "tenancy")]
pub fn active_required(
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
    user_passes_test(login_url, |u| u.active)
}

/// API variant of [`active_required`]: 401 for anonymous, 403 for a
/// deactivated account.
#[cfg(feature = "tenancy")]
pub fn active_required_or_403() -> impl tower::Layer<
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
    user_passes_test_or_403(|u| u.active)
}

/// Permission-codename access gate.
///
/// Like [`user_passes_test`], but the check is a permission lookup in
/// the tenant's perm engine instead of a closure over the `User` row.
/// Anonymous requests redirect to `login_url` with `?next=`. A signed-in
/// user without the codename gets `403 Forbidden`. Superusers skip the
/// codename check: the bypass lives in
/// [`crate::tenancy::permissions::has_perm_pool`].
///
/// ```ignore
/// use rustango::auth_decorators::permission_required;
/// use rustango::tenancy::permissions::ACCESS_ADMIN_CODENAME;
///
/// // Gate the framework admin behind the reserved codename. Existing
/// // superusers keep working (bypass is automatic); non-superusers
/// // need an explicit grant via `set_user_perm_pool(uid,
/// // ACCESS_ADMIN_CODENAME, true, ...)`.
/// let admin = Router::new()
///     .route("/admin", get(dashboard))
///     .layer(permission_required("/login", ACCESS_ADMIN_CODENAME));
/// ```
///
/// **Tenant resolution**: the gate extracts [`crate::extractors::Tenant`]
/// for the pool that `has_perm_pool` queries. Routes using this
/// middleware MUST be mounted under the tenant context. A route
/// without one gets a 500, never an allow.
///
/// **Cost**: every request pays two extractor calls and one
/// `has_perm_pool` query. There is no caching.
#[cfg(feature = "tenancy")]
pub fn permission_required(
    login_url: impl Into<String>,
    codename: &'static str,
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
        async move { handle_permission_required(cfg, codename, req, next).await }
    })
}

/// API variant of [`permission_required`]: 401 for anonymous, 403 for
/// a signed-in user without the codename. Same codename rules and
/// superuser bypass.
///
/// ```ignore
/// use rustango::auth_decorators::permission_required_or_403;
///
/// let api = Router::new()
///     .route("/api/admin/stats", get(stats))
///     .layer(permission_required_or_403("auth.access_admin"));
/// ```
#[cfg(feature = "tenancy")]
pub fn permission_required_or_403(
    codename: &'static str,
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
    axum::middleware::from_fn(move |req: Request<Body>, next: Next| async move {
        handle_permission_required_or_403(codename, req, next).await
    })
}

#[cfg(feature = "tenancy")]
async fn handle_permission_required(
    cfg: Arc<LoginRequiredConfig>,
    codename: &'static str,
    req: Request<Body>,
    next: Next,
) -> Response {
    use axum::extract::FromRequestParts as _;
    let (mut parts, body) = req.into_parts();
    let user = crate::extractors::SessionUser::from_request_parts(&mut parts, &())
        .await
        .unwrap_or(crate::extractors::SessionUser(None));
    let Some(u) = user.0.as_ref() else {
        // Anonymous: redirect to login with ?next=.
        let original = parts
            .uri
            .path_and_query()
            .map(|p| p.as_str().to_owned())
            .unwrap_or_else(|| "/".to_owned());
        return redirect_to_login(&cfg.login_url, &cfg.redirect_field, &original);
    };
    let uid = match u.id.get().copied() {
        Some(id) => id,
        None => {
            // A session pointing at an unsaved user is a framework
            // bug. Fail with 500 rather than guess.
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .expect("500 + empty body is always valid");
        }
    };
    // The perm engine needs the tenant pool. If the tenant cannot be
    // resolved we cannot tell whether the user is authorised, so fail
    // closed with 500.
    let tenant =
        match crate::extractors::Tenant::<crate::tenancy::DefaultTenantDb>::from_request_parts(
            &mut parts,
            &(),
        )
        .await
        {
            Ok(t) => t,
            Err(_) => {
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::empty())
                    .expect("500 + empty body is always valid");
            }
        };
    let has = crate::tenancy::permissions::has_perm_pool(uid, codename, tenant.pool())
        .await
        .unwrap_or(false);
    if has {
        let req = Request::from_parts(parts, body);
        return next.run(req).await;
    }
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .body(Body::empty())
        .expect("403 + empty body is always valid")
}

#[cfg(feature = "tenancy")]
async fn handle_permission_required_or_403(
    codename: &'static str,
    req: Request<Body>,
    next: Next,
) -> Response {
    use axum::extract::FromRequestParts as _;
    let (mut parts, body) = req.into_parts();
    let user = crate::extractors::SessionUser::from_request_parts(&mut parts, &())
        .await
        .unwrap_or(crate::extractors::SessionUser(None));
    let Some(u) = user.0.as_ref() else {
        // Anonymous: 401 means "sign in first".
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body(Body::empty())
            .expect("401 + empty body is always valid");
    };
    let uid = match u.id.get().copied() {
        Some(id) => id,
        None => {
            // A session pointing at an unsaved user is a framework
            // bug. Fail with 500 rather than guess.
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .expect("500 + empty body is always valid");
        }
    };
    let tenant =
        match crate::extractors::Tenant::<crate::tenancy::DefaultTenantDb>::from_request_parts(
            &mut parts,
            &(),
        )
        .await
        {
            Ok(t) => t,
            Err(_) => {
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::empty())
                    .expect("500 + empty body is always valid");
            }
        };
    let has = crate::tenancy::permissions::has_perm_pool(uid, codename, tenant.pool())
        .await
        .unwrap_or(false);
    if has {
        let req = Request::from_parts(parts, body);
        return next.run(req).await;
    }
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .body(Body::empty())
        .expect("403 + empty body is always valid")
}

#[cfg(feature = "tenancy")]
async fn handle_login_required(
    cfg: Arc<LoginRequiredConfig>,
    req: Request<Body>,
    next: Next,
) -> Response {
    use axum::extract::FromRequestParts as _;
    // Run the SessionUser extractor by hand: as a `from_fn` argument
    // it would need `FromRequest`, not just `FromRequestParts`.
    let (mut parts, body) = req.into_parts();
    let user = crate::extractors::SessionUser::from_request_parts(&mut parts, &())
        .await
        .unwrap_or(crate::extractors::SessionUser(None));
    if user.0.is_some() {
        let req = Request::from_parts(parts, body);
        return next.run(req).await;
    }
    // Anonymous: build the 302 target from the original URL.
    let original = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "/".to_owned());
    redirect_to_login(&cfg.login_url, &cfg.redirect_field, &original)
}

/// Build the 302 to `login_url` with the URL-encoded `original` path
/// in `?next=`. Public so hand-written gates can reuse it.
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

/// Read the `next` query parameter from a query map. Returns `None`
/// when it is missing or empty, so the caller can fall back to `"/"`.
#[must_use]
pub fn extract_next(query: &HashMap<String, String>) -> Option<String> {
    extract_next_named(query, "next")
}

/// Like [`extract_next`], but with a custom field name matching
/// [`LoginRequiredConfig::redirect_field`].
#[must_use]
pub fn extract_next_named(query: &HashMap<String, String>, field: &str) -> Option<String> {
    query
        .get(field)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Open-redirect defence. Returns `Some(next)` only for a
/// same-origin, root-relative path. Rejects:
///
/// - URLs with a scheme (`http://evil.example/`, `//evil.example/x`)
/// - Backslash forms a browser turns into a host (`/\evil.example/x`)
/// - Percent-encoded forms of those
///   (`%2F%2Fevil.example/x` decodes to `//evil.example/x`)
/// - Control characters, empty and whitespace-only values
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
/// It does **not** check that the path is a real route. That is
/// intended: a 404 after login is far better than a phishing
/// redirect.
#[must_use]
pub fn safe_next(next: &str) -> Option<String> {
    let trimmed = next.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Raw control characters are the dangerous form. A browser strips
    // a raw TAB while parsing, so `/<TAB>/evil` leaves as the
    // protocol-relative `//evil`; a raw CR or LF breaks `HeaderValue`.
    // An encoded one is inert, since this returns the still-encoded
    // value.
    if trimmed.chars().any(char::is_control) {
        return None;
    }
    // Only the first two bytes decide the shape, so decode a bounded
    // prefix. Decoding the whole caller-supplied value to read two
    // bytes is costly on a pre-auth path.
    const SHAPE_PREFIX: usize = 24;
    let head: String = trimmed.chars().take(SHAPE_PREFIX).collect();
    if !is_safe_path(&crate::url_codec::url_decode(&head)) {
        return None;
    }
    // The raw form must be path-shaped too: decoding can only reveal
    // a `//`, never hide one.
    if !is_safe_path(trimmed) {
        return None;
    }
    Some(trimmed.to_owned())
}

/// Shared check for the raw and the decoded form of `next`. The path
/// must start with `/` and must NOT start with `//` (scheme-relative)
/// or `/\` (backslash host).
fn is_safe_path(s: &str) -> bool {
    // Control characters first. A browser strips TAB, CR and LF while
    // parsing a URL, so `/<TAB>/evil.example/x` looks path-shaped here
    // and leaves as the protocol-relative `//evil.example/x`.
    // `HeaderValue` accepts TAB, so nothing downstream catches it.
    // Other callers rely on this check, so do not move it.
    if s.chars().any(char::is_control) {
        return false;
    }
    // Compare bytes instead of normalising a copy: the decision needs
    // only two bytes. A browser rewrites `\` to `/`, so `//` and `/\`
    // are both protocol-relative once the request leaves.
    let b = s.as_bytes();
    if b.first() != Some(&b'/') {
        return false;
    }
    !matches!(b.get(1), Some(b'/' | b'\\'))
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
        // CRLF in the original URL is a response-splitting vector.
        // The value is percent-encoded first, so no raw CRLF reaches
        // the header. That invariant is what is asserted, not the
        // exact encoded form.
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
        // Scheme-relative is a phishing vector: a browser routes
        // this to <host>/x.
        assert_eq!(safe_next("//evil.example/x"), None);
    }

    #[test]
    fn safe_next_rejects_backslash_variant() {
        // A browser rewrites the backslash to `/`, giving
        // `//evil.example/x`, which routes to the attacker's host.
        assert_eq!(safe_next("/\\evil.example/x"), None);
    }

    #[test]
    fn safe_next_rejects_percent_encoded_bypass() {
        // These start with `/` but decode to `//evil.example/x`, a
        // phishing redirect. The fix is to decode before checking.
        assert_eq!(safe_next("%2F%2Fevil.example/x"), None);
        assert_eq!(safe_next("%2f%2fevil.example/x"), None);
        // Double-slash with the second slash encoded.
        assert_eq!(safe_next("/%2Fevil.example/x"), None);
        // Backslash variant percent-encoded.
        assert_eq!(safe_next("/%5Cevil.example/x"), None);
    }

    #[test]
    fn safe_next_accepts_legitimate_percent_encodes_in_path() {
        // Only percent-encodes that decode to a host pattern are
        // rejected. A literal `%20` in the path stays valid.
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

    #[cfg(feature = "tenancy")]
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

        // No TenantContext, so SessionUser is None and the gate
        // redirects to /login?next=/profile.
        assert_eq!(res.status(), StatusCode::FOUND);
        let loc = res
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap();
        assert_eq!(loc, "/login?next=%2Fprofile");
    }

    #[cfg(feature = "tenancy")]
    #[tokio::test]
    async fn user_passes_test_redirects_anonymous_to_login_url() {
        use axum::body::Body;
        use axum::routing::get;
        use axum::Router;
        use tower::ServiceExt as _;

        async fn staff_only() -> &'static str {
            "staff zone"
        }

        // The predicate never runs for an anonymous request.
        let app = Router::new()
            .route("/admin/dashboard", get(staff_only))
            .layer(user_passes_test("/login", |u| u.is_superuser));

        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/admin/dashboard")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::FOUND);
        let loc = res
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap();
        assert_eq!(loc, "/login?next=%2Fadmin%2Fdashboard");
    }

    #[cfg(feature = "tenancy")]
    #[test]
    fn user_passes_test_signature_compiles_with_closure_predicate() {
        // Compile-only: pin the signature so the predicate can be a
        // closure over locals. The body never runs.
        let _ = || {
            let _layer = user_passes_test("/login", |u: &crate::tenancy::auth::User| {
                u.is_superuser && u.active
            });
        };
    }

    #[cfg(feature = "tenancy")]
    #[tokio::test]
    async fn user_passes_test_or_403_returns_401_for_anonymous() {
        use axum::body::Body;
        use axum::routing::get;
        use axum::Router;
        use tower::ServiceExt as _;

        async fn admin_only() -> &'static str {
            "ok"
        }

        let app = Router::new()
            .route("/api/admin/stats", get(admin_only))
            .layer(user_passes_test_or_403(|u| u.is_superuser));

        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/admin/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // No SessionUser means 401, not the 403 a signed-in but
        // unauthorised user would get.
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[cfg(feature = "tenancy")]
    #[test]
    fn login_required_or_401_signature_compiles() {
        // Compile-only: pin the no-arg variant's signature.
        let _ = || {
            let _layer = login_required_or_401();
        };
    }

    #[cfg(feature = "tenancy")]
    #[tokio::test]
    async fn superuser_required_redirects_anonymous_to_login() {
        use axum::body::Body;
        use axum::routing::get;
        use axum::Router;
        use tower::ServiceExt as _;

        async fn admin_only() -> &'static str {
            "admin zone"
        }

        let app = Router::new()
            .route("/admin/dashboard", get(admin_only))
            .layer(superuser_required("/login"));

        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/admin/dashboard")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::FOUND);
        let loc = res
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap();
        assert_eq!(loc, "/login?next=%2Fadmin%2Fdashboard");
    }

    #[cfg(feature = "tenancy")]
    #[tokio::test]
    async fn superuser_required_or_403_returns_401_for_anonymous() {
        use axum::body::Body;
        use axum::routing::get;
        use axum::Router;
        use tower::ServiceExt as _;

        async fn admin_api() -> &'static str {
            "ok"
        }

        let app = Router::new()
            .route("/api/admin", get(admin_api))
            .layer(superuser_required_or_403());

        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[cfg(feature = "tenancy")]
    #[tokio::test]
    async fn active_required_redirects_anonymous_to_login() {
        use axum::body::Body;
        use axum::routing::get;
        use axum::Router;
        use tower::ServiceExt as _;

        async fn dashboard() -> &'static str {
            "dashboard"
        }

        let app = Router::new()
            .route("/dashboard", get(dashboard))
            .layer(active_required("/login"));

        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/dashboard")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::FOUND);
        let loc = res
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap();
        assert_eq!(loc, "/login?next=%2Fdashboard");
    }

    #[cfg(feature = "tenancy")]
    #[tokio::test]
    async fn active_required_or_403_returns_401_for_anonymous() {
        use axum::body::Body;
        use axum::routing::get;
        use axum::Router;
        use tower::ServiceExt as _;

        async fn me_api() -> &'static str {
            "ok"
        }

        let app = Router::new()
            .route("/api/me", get(me_api))
            .layer(active_required_or_403());

        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/me")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
}
