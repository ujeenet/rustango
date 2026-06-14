//! Django-shape view shortcuts. Issue #10.
//!
//! Four helpers that show up on every Django page render — folded into
//! a single rustango module so axum handlers don't reach into the
//! low-level status-code / tera / redirect primitives for routine
//! cases. Matches the Django [shortcuts module](https://docs.djangoproject.com/en/6.0/topics/http/shortcuts/).
//!
//! ```ignore
//! use rustango::shortcuts::{get_object_or_404, render, redirect, ShortcutError};
//!
//! async fn post_detail(
//!     State(state): State<AppState>,
//!     Path(id): Path<i64>,
//! ) -> Result<axum::response::Response, ShortcutError> {
//!     let post = get_object_or_404(
//!         Post::objects().where_(Post::id.eq(id)),
//!         &state.pool,
//!     )
//!     .await?;
//!     let mut ctx = tera::Context::new();
//!     ctx.insert("post", &post);
//!     Ok(render(&state.tera, "post_detail.html", &ctx))
//! }
//! ```
//!
//! Gated behind the `template_views` feature (which pulls in axum +
//! tera). Apps that wire axum themselves can import any of these
//! helpers directly.

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::core::Model;
use crate::query::QuerySet;
use crate::sql::{
    ExecError, FetcherPool, LoadRelated, MaybeMyFromRow, MaybeMyLoadRelated, MaybePgFromRow,
    MaybeSqliteFromRow, MaybeSqliteLoadRelated, Pool,
};

/// Error returned by [`get_object_or_404`] / [`get_list_or_404`].
/// Two-variant union so the `?` operator can propagate both the
/// "no row matched" case (→ 404) and any underlying driver error
/// (→ 500) without conflating them — a DB outage surfacing as a 404
/// would silently degrade observability.
///
/// Implements [`IntoResponse`] so handlers that return
/// `Result<_, ShortcutError>` map automatically to the right status
/// code:
///
/// - [`Self::NotFound`] → `404 Not Found` with the `message` as body.
/// - [`Self::Database`] → `500 Internal Server Error` with the
///   underlying [`ExecError`] in the body.
#[derive(Debug)]
pub enum ShortcutError {
    /// No row matched the queryset. Rendered as 404.
    NotFound { message: String },
    /// Driver / SQL failure on the underlying query. Rendered as 500.
    Database(ExecError),
}

impl ShortcutError {
    /// Construct a `NotFound` variant with a custom message.
    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ShortcutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { message } => write!(f, "{message}"),
            Self::Database(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for ShortcutError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotFound { .. } => None,
            Self::Database(e) => Some(e),
        }
    }
}

impl IntoResponse for ShortcutError {
    fn into_response(self) -> Response {
        match self {
            Self::NotFound { message } => (StatusCode::NOT_FOUND, message).into_response(),
            Self::Database(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("database error: {e}"),
            )
                .into_response(),
        }
    }
}

impl From<ExecError> for ShortcutError {
    /// Driver errors route to the `Database` variant — `IntoResponse`
    /// then renders them as `500`. The `?` operator in a handler
    /// returning `Result<_, ShortcutError>` propagates DB failures
    /// without quietly conflating them with 404s.
    fn from(e: ExecError) -> Self {
        Self::Database(e)
    }
}

/// Fetch the first row matching `qs` against `pool`, or return
/// [`ShortcutError::NotFound`] if none matched. Django's
/// [`get_object_or_404`](https://docs.djangoproject.com/en/6.0/topics/http/shortcuts/#get-object-or-404).
///
/// Builds the queryset with whatever filters / ordering you want
/// up-front; this helper just collapses the typical "fetch one or
/// 404" branch:
///
/// ```ignore
/// let post = get_object_or_404(
///     Post::objects().where_(Post::slug.eq("hello-world")),
///     &state.pool,
/// ).await?;
/// ```
///
/// Underlying [`ExecError`]s route to [`ShortcutError::Database`] and
/// render as `500` — distinct from the `404` of `NotFound`, so a DB
/// outage doesn't quietly show up as "not found" in user-facing logs.
///
/// # Errors
/// - [`ShortcutError::NotFound`] when no row matched the queryset.
/// - [`ShortcutError::Database`] wrapping any underlying [`ExecError`].
pub async fn get_object_or_404<T>(qs: QuerySet<T>, pool: &Pool) -> Result<T, ShortcutError>
where
    T: Model
        + Send
        + Unpin
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated,
{
    match qs.first(pool).await? {
        Some(row) => Ok(row),
        None => Err(ShortcutError::not_found(format!(
            "no {} matches",
            T::SCHEMA.name
        ))),
    }
}

/// Fetch every row matching `qs`. If the result is empty, return
/// [`ShortcutError::NotFound`]. Django's
/// [`get_list_or_404`](https://docs.djangoproject.com/en/6.0/topics/http/shortcuts/#get-list-or-404).
///
/// ```ignore
/// let comments = get_list_or_404(
///     Comment::objects().where_(Comment::post_id.eq(post.id)),
///     &state.pool,
/// ).await?;
/// ```
///
/// # Errors
/// - [`ShortcutError::NotFound`] when the result set is empty.
/// - [`ShortcutError::Database`] wrapping any underlying [`ExecError`].
pub async fn get_list_or_404<T>(qs: QuerySet<T>, pool: &Pool) -> Result<Vec<T>, ShortcutError>
where
    T: Model
        + Send
        + Unpin
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated,
{
    let rows = qs.fetch(pool).await?;
    if rows.is_empty() {
        Err(ShortcutError::not_found(format!(
            "no {} matches",
            T::SCHEMA.name
        )))
    } else {
        Ok(rows)
    }
}

/// Render a Tera template and wrap the result in an HTML axum
/// response. Django's
/// [`render(request, template, context)`](https://docs.djangoproject.com/en/6.0/topics/http/shortcuts/#render).
///
/// On template-render failure, returns a `500 Internal Server Error`
/// with the Tera error in the body. Apps that want a structured
/// error page should render their own error template via the same
/// helper.
///
/// ```ignore
/// let mut ctx = tera::Context::new();
/// ctx.insert("post", &post);
/// render(&state.tera, "post_detail.html", &ctx)
/// ```
#[must_use]
pub fn render(tera: &tera::Tera, name: &str, ctx: &tera::Context) -> Response {
    match tera.render(name, ctx) {
        Ok(body) => axum::response::Html(body).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("template `{name}` failed: {e}"),
        )
            .into_response(),
    }
}

/// Render a Tera template to a `String` (no Response wrapping).
/// Django's
/// [`render_to_string(template_name, context)`](https://docs.djangoproject.com/en/6.0/topics/templates/#django.template.loader.render_to_string).
///
/// Use when the rendered output isn't going straight back over HTTP —
/// emails, generated reports, PDF source, snapshot tests, an
/// inner-template-rendered string spliced into a parent context, etc.
///
/// Returns the underlying [`tera::Error`] on failure so the caller can
/// distinguish a missing-template situation from a runtime render
/// error (vs [`render`] which collapses both into a 500).
///
/// ```ignore
/// let mut ctx = tera::Context::new();
/// ctx.insert("user", &user);
/// let email_body = render_to_string(&tera, "welcome_email.txt", &ctx)?;
/// send_email(&user.email, "Welcome", &email_body).await?;
/// ```
///
/// # Errors
/// Returns [`tera::Error`] when the named template is missing, the
/// template fails to parse, or any filter/function inside it returns
/// an error during render.
pub fn render_to_string(
    tera: &tera::Tera,
    name: &str,
    ctx: &tera::Context,
) -> Result<String, tera::Error> {
    tera.render(name, ctx)
}

/// Return a `302 Found` redirect to `url`. Django's
/// [`redirect(to)`](https://docs.djangoproject.com/en/6.0/topics/http/shortcuts/#redirect).
///
/// Pair with [`redirect_permanent`] for `301 Moved Permanently`,
/// [`redirect_see_other`] for `303 See Other`, and
/// [`redirect_to_view`] for the Django `redirect('post-detail', pk=1)`
/// view-name shape that resolves through the URL conf.
///
/// Matches Django's status codes exactly (302 / 301) — axum's built-in
/// `Redirect::to` uses 303 See Other instead, which has subtly
/// different semantics around method preservation.
#[must_use]
pub fn redirect(url: impl Into<String>) -> Response {
    build_redirect(StatusCode::FOUND, url.into())
}

/// Django-parity [`resolve_url(to, *args, **kwargs)`](https://docs.djangoproject.com/en/6.0/topics/http/shortcuts/#resolve-url) —
/// normalize either a raw URL or a registered route name into a
/// concrete URL string.
///
/// Rules:
/// 1. If `spec` starts with `/`, `http://`, `https://`, `./`, or
///    `../`, return as-is (already a URL).
/// 2. Otherwise treat `spec` as a registered route name and call
///    [`crate::urls::reverse`] with `params` (already-stringified
///    values). Namespaced names (`"polls:detail"`) round-trip the
///    same way `urls::reverse` handles them.
///
/// # Errors
/// Forwards [`crate::urls::ReverseError`] when `spec` is a name and
/// `params` doesn't satisfy the pattern (missing / extra / unknown
/// name).
///
/// ```ignore
/// use std::collections::HashMap;
/// use rustango::shortcuts::resolve_url;
///
/// // Raw URL passes through.
/// assert_eq!(resolve_url("/posts/42", &HashMap::new()).unwrap(),
///            "/posts/42");
///
/// // Named route gets reverse-looked-up.
/// let mut params = HashMap::new();
/// params.insert("id", "42".into());
/// // assuming `register_url!("post_detail", "/posts/{id}")` somewhere
/// let url = resolve_url("post_detail", &params)?;
/// ```
pub fn resolve_url(
    spec: &str,
    params: &std::collections::HashMap<&str, String>,
) -> Result<String, crate::urls::ReverseError> {
    if spec.starts_with('/')
        || spec.starts_with("http://")
        || spec.starts_with("https://")
        || spec.starts_with("./")
        || spec.starts_with("../")
    {
        return Ok(spec.to_owned());
    }
    crate::urls::reverse(spec, params)
}

/// Django-parity `redirect('view-name', kwargs={'pk': 1})` shape —
/// resolve a registered route name + params into a URL and return
/// a `302 Found` redirect response. Pairs with [`resolve_url`].
///
/// # Errors
/// Forwards [`crate::urls::ReverseError`] when the name or params
/// don't satisfy a registered route.
///
/// ```ignore
/// use std::collections::HashMap;
/// use rustango::shortcuts::redirect_to_view;
///
/// let mut params = HashMap::new();
/// params.insert("id", "42".into());
/// // Returns a 302 Location: /posts/42 response, given the matching
/// // register_url!("post_detail", "/posts/{id}") in the app.
/// let resp = redirect_to_view("post_detail", &params)?;
/// ```
pub fn redirect_to_view(
    name: &str,
    params: &std::collections::HashMap<&str, String>,
) -> Result<Response, crate::urls::ReverseError> {
    let url = crate::urls::reverse(name, params)?;
    Ok(build_redirect(StatusCode::FOUND, url))
}

/// Return a `301 Moved Permanently` redirect to `url`. Use for
/// canonical URL migrations (search engines treat 301 differently
/// from the default 302).
#[must_use]
pub fn redirect_permanent(url: impl Into<String>) -> Response {
    build_redirect(StatusCode::MOVED_PERMANENTLY, url.into())
}

/// Return a `303 See Other` redirect to `url`. The proper status
/// code for the "after a successful POST" pattern: it forces the
/// client to follow with `GET` regardless of the original request
/// method, so the user can refresh the resulting page without
/// re-submitting the form (the classic POST→303→GET flow).
///
/// Distinct from [`redirect`] (302 Found), which historically had
/// the same "force GET" behaviour but RFC 7231 leaves it
/// implementation-defined for non-GET requests. 303 is the
/// unambiguous choice for "form submit succeeded, look here for
/// the result."
#[must_use]
pub fn redirect_see_other(url: impl Into<String>) -> Response {
    build_redirect(StatusCode::SEE_OTHER, url.into())
}

/// Return a `307 Temporary Redirect` to `url`. Unlike `302` and
/// `303`, the client is required to use the SAME request method on
/// the redirect target. Use when the resource has moved temporarily
/// but the verb still applies — e.g. a load balancer 307-redirecting
/// `POST /api/v1/...` to `POST /api/v1-new/...`.
///
/// For "form submit succeeded" use [`redirect_see_other`] instead.
/// For "this URL has moved permanently" use [`redirect_permanent`].
#[must_use]
pub fn redirect_temporary(url: impl Into<String>) -> Response {
    build_redirect(StatusCode::TEMPORARY_REDIRECT, url.into())
}

/// Return a `308 Permanent Redirect` to `url`. The method-preserving
/// counterpart of [`redirect_permanent`] (301): same long-term
/// migration semantics, but clients must reuse the request method
/// on the new URL. Useful for API endpoint canonicalization.
#[must_use]
pub fn redirect_permanent_preserve_method(url: impl Into<String>) -> Response {
    build_redirect(StatusCode::PERMANENT_REDIRECT, url.into())
}

/// Redirect to a login page, preserving the current request URL as
/// `?next=<path>` so the login handler can bounce the user back after
/// authenticating. Django's
/// [`redirect_to_login(next, login_url)`](https://docs.djangoproject.com/en/6.0/_modules/django/contrib/auth/views/#redirect_to_login).
///
/// The `next` value is URL-encoded before being appended. If
/// `login_url` already carries a query string, `next=` is added with
/// `&`; otherwise with `?`.
///
/// ```ignore
/// // Inside a view that requires auth:
/// if user.is_none() {
///     return redirect_to_login(req.uri().path_and_query()
///         .map(|p| p.as_str())
///         .unwrap_or("/"), "/login");
/// }
/// ```
///
/// Returns a `302 Found` (matching [`redirect`]'s status), so the
/// browser keeps the original method semantics consistent with the
/// rest of the shortcuts module.
#[must_use]
pub fn redirect_to_login(next: &str, login_url: &str) -> Response {
    let encoded = crate::url_codec::url_encode(next);
    let separator = if login_url.contains('?') { '&' } else { '?' };
    redirect(format!("{login_url}{separator}next={encoded}"))
}

fn build_redirect(status: StatusCode, url: String) -> Response {
    // Hand-roll the Response so the status matches Django (302/301)
    // rather than axum's modern default (303/308). Falls back to a
    // header-less response if the URL contains invalid header
    // characters — that's a programmer error so the bare status is
    // still useful for debugging.
    let mut res = Response::builder()
        .status(status)
        .body(axum::body::Body::empty())
        .expect("status + empty body is always valid");
    if let Ok(v) = HeaderValue::from_str(&url) {
        res.headers_mut().insert(header::LOCATION, v);
    }
    res
}

/// Serialize `data` to JSON and wrap it in a Response with the given
/// `status` and `Content-Type: application/json`. Django's
/// [`JsonResponse(data, status=...)`](https://docs.djangoproject.com/en/6.0/ref/request-response/#jsonresponse-objects).
///
/// Slightly more concise than the axum equivalent
/// `(StatusCode::BAD_REQUEST, Json(data)).into_response()` at API
/// error sites, and gives a single fall-through (`200 OK`,
/// `application/json`) for the common success path:
///
/// ```ignore
/// use rustango::shortcuts::{json_response, json_ok};
///
/// // Success path:
/// json_ok(&serde_json::json!({"id": 1, "name": "Alice"}))
///
/// // Error path:
/// json_response(&serde_json::json!({"error": "validation failed"}), 400)
/// ```
///
/// Failure modes:
/// - Serialization failure → `500 Internal Server Error` with an
///   empty body. Almost never happens with `serde_json::Value` /
///   ordinary structs; would require a custom Serialize that fails.
/// - Invalid `status` (outside u16 range or not a valid HTTP code)
///   → falls back to `200 OK` so the response still parses.
#[must_use]
pub fn json_response<T: serde::Serialize>(data: &T, status: u16) -> Response {
    let body = match serde_json::to_vec(data) {
        Ok(b) => b,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(axum::body::Body::empty())
                .expect("500 + empty body is always valid");
        }
    };
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let mut res = Response::builder()
        .status(status)
        .body(axum::body::Body::from(body))
        .expect("status + non-empty body is always valid");
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    res
}

/// Sugar for [`json_response`] with status `200 OK` — the most
/// common success-path shape.
#[must_use]
pub fn json_ok<T: serde::Serialize>(data: &T) -> Response {
    json_response(data, 200)
}

/// Sugar for [`json_response`] with status `400 Bad Request` — the
/// most common error-path shape for input validation failures.
#[must_use]
pub fn json_bad_request<T: serde::Serialize>(data: &T) -> Response {
    json_response(data, 400)
}

/// Sugar for [`json_response`] with status `401 Unauthorized`.
/// Pair with [`crate::auth_decorators::login_required_or_401`] for
/// hand-rolled auth checks: when the request lacks credentials,
/// return a JSON error body so the API client can render it
/// without parsing HTML.
#[must_use]
pub fn json_unauthorized<T: serde::Serialize>(data: &T) -> Response {
    json_response(data, 401)
}

/// Sugar for [`json_response`] with status `403 Forbidden`.
/// Use when the caller IS authenticated but lacks the permission
/// required for this endpoint — distinct from 401 so clients can
/// distinguish "log in" from "you can't access this." Pair with
/// [`crate::auth_decorators::user_passes_test_or_403`].
#[must_use]
pub fn json_forbidden<T: serde::Serialize>(data: &T) -> Response {
    json_response(data, 403)
}

/// Sugar for [`json_response`] with status `404 Not Found`. The
/// API counterpart to [`ShortcutError::NotFound`], which renders a
/// `text/plain` 404. Use this when the endpoint contract is JSON
/// and a plain-text body would confuse the client parser.
#[must_use]
pub fn json_not_found<T: serde::Serialize>(data: &T) -> Response {
    json_response(data, 404)
}

/// Sugar for [`json_response`] with status `500 Internal Server
/// Error`. Use when a handler catches an internal failure and
/// wants to surface a JSON-shaped error to the client (with a
/// stable error code the client can branch on) instead of a bare
/// status line.
#[must_use]
pub fn json_server_error<T: serde::Serialize>(data: &T) -> Response {
    json_response(data, 500)
}

/// Wrap a pre-rendered HTML string in a Response with the given
/// status and `Content-Type: text/html; charset=utf-8`. Django's
/// [`HttpResponse(html, status=...)`](https://docs.djangoproject.com/en/6.0/ref/request-response/#httpresponse-objects)
/// for plain-HTML responses.
///
/// Use when you have HTML in hand (string-built, snipped from another
/// source, hard-coded for a tiny error page) and don't want to spin
/// up Tera just for the response. For Tera-rendered output, use
/// [`render`] (returns a Response directly) or [`render_to_string`]
/// (returns a String you can pass to this function).
///
/// ```ignore
/// use rustango::shortcuts::html_response;
///
/// async fn maintenance() -> Response {
///     html_response("<h1>Down for maintenance</h1>", 503)
/// }
/// ```
///
/// Invalid `status` (below 100) falls back to `200 OK` so a typoed
/// status code doesn't panic the builder.
#[must_use]
pub fn html_response(content: impl Into<String>, status: u16) -> Response {
    let body = content.into();
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let mut res = Response::builder()
        .status(status)
        .body(axum::body::Body::from(body))
        .expect("status + body is always valid");
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    res
}

/// Wrap a plain-text string in a Response with the given status and
/// `Content-Type: text/plain; charset=utf-8`. Useful for tiny ops
/// endpoints (`/health`, `/version`) and CLI-style HTTP that doesn't
/// want HTML escaping.
///
/// ```ignore
/// use rustango::shortcuts::text_response;
///
/// async fn health() -> Response {
///     text_response("ok", 200)
/// }
/// ```
#[must_use]
pub fn text_response(content: impl Into<String>, status: u16) -> Response {
    let body = content.into();
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let mut res = Response::builder()
        .status(status)
        .body(axum::body::Body::from(body))
        .expect("status + body is always valid");
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    res
}

/// Django-parity
/// [`HttpResponseNotModified()`](https://docs.djangoproject.com/en/6.0/ref/request-response/#django.http.HttpResponseNotModified) —
/// `304 Not Modified` with no body. Used in conditional-GET flows
/// (`If-Modified-Since` / `If-None-Match` returning 304 when the
/// client's cached copy is still fresh).
///
/// The 304 response MUST NOT carry a body per RFC 7232 §4.1 — this
/// helper enforces that by hard-coding `Body::empty()`. Callers
/// should still copy any relevant `ETag` / `Last-Modified` /
/// `Cache-Control` headers from the cached representation manually
/// (Django's HttpResponseNotModified does NOT copy them either —
/// the caller is responsible).
///
/// ```ignore
/// use rustango::shortcuts::not_modified;
/// async fn handler() -> axum::response::Response {
///     // Conditional check returned freshness — short-circuit.
///     not_modified()
/// }
/// ```
#[must_use]
pub fn not_modified() -> Response {
    Response::builder()
        .status(StatusCode::NOT_MODIFIED)
        .body(axum::body::Body::empty())
        .expect("304 + empty body is always valid")
}

/// Django-parity
/// [`HttpResponseGone(content=b'')`](https://docs.djangoproject.com/en/6.0/ref/request-response/#django.http.HttpResponseGone) —
/// `410 Gone` with optional message body. Use when a resource that
/// previously existed has been intentionally and permanently
/// removed (search engines + clients SHOULD purge it from caches
/// + indexes; distinct from `404 Not Found` which says "I don't
/// know if this ever existed").
///
/// Body content-type is `text/plain; charset=utf-8`. Pass an empty
/// string for a bare-bones response.
///
/// ```ignore
/// use rustango::shortcuts::gone;
/// async fn deleted_user() -> axum::response::Response {
///     gone("This user account has been permanently deleted.")
/// }
/// ```
#[must_use]
pub fn gone(message: impl Into<String>) -> Response {
    let body = message.into();
    let mut res = Response::builder()
        .status(StatusCode::GONE)
        .body(axum::body::Body::from(body))
        .expect("410 + text body is always valid");
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    res
}

/// Django-shape `HttpResponse(status=204)` — `204 No Content` with
/// empty body. Used when a write succeeded but there's nothing
/// meaningful to return (DELETE handlers, PUT/PATCH that don't
/// echo the resource).
///
/// RFC 7231 §6.3.5 requires the body be empty on 204; this helper
/// hard-codes `Body::empty()` so callers can't accidentally send
/// a payload.
///
/// ```ignore
/// use rustango::shortcuts::no_content;
/// async fn delete_handler() -> axum::response::Response {
///     // ... do the delete ...
///     no_content()
/// }
/// ```
#[must_use]
pub fn no_content() -> Response {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(axum::body::Body::empty())
        .expect("204 + empty body is always valid")
}

/// Django-shape `HttpResponse(status=202)` — `202 Accepted`. Used
/// when a request has been validated and queued for async processing,
/// but the actual work hasn't finished. Body typically carries the
/// job ID or a status-poll URL.
///
/// Pair with a `Location:` header pointing at the status endpoint
/// (RFC 7231 doesn't mandate it for 202, but it's the canonical
/// shape for async-job APIs).
///
/// ```ignore
/// use rustango::shortcuts::accepted;
/// async fn submit() -> axum::response::Response {
///     // ... enqueue work ...
///     accepted("Job 42 queued; poll /jobs/42 for status.")
/// }
/// ```
#[must_use]
pub fn accepted(message: impl Into<String>) -> Response {
    let body = message.into();
    let mut res = Response::builder()
        .status(StatusCode::ACCEPTED)
        .body(axum::body::Body::from(body))
        .expect("202 + text body is always valid");
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    res
}

/// Django-shape `HttpResponse(status=415)` — `415 Unsupported
/// Media Type`. Use when the request body's `Content-Type` doesn't
/// match what the handler can parse (e.g. handler expects JSON,
/// client sent XML).
///
/// Body is a `text/plain` message describing the supported types.
/// Use an empty string for a bare-bones response.
#[must_use]
pub fn unsupported_media_type(message: impl Into<String>) -> Response {
    let body = message.into();
    let mut res = Response::builder()
        .status(StatusCode::UNSUPPORTED_MEDIA_TYPE)
        .body(axum::body::Body::from(body))
        .expect("415 + text body is always valid");
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    res
}

/// Django-shape `HttpResponse(status=409)` — `409 Conflict`. Use
/// when the request collides with current resource state — concurrent
/// edit, unique-constraint violation surfaced before INSERT,
/// stale write-token, etc.
#[must_use]
pub fn conflict(message: impl Into<String>) -> Response {
    let body = message.into();
    let mut res = Response::builder()
        .status(StatusCode::CONFLICT)
        .body(axum::body::Body::from(body))
        .expect("409 + text body is always valid");
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    res
}

/// Django-shape `HttpResponse(status=422)` — `422 Unprocessable
/// Entity`. Use when a request is syntactically valid (parses fine)
/// but semantically invalid (validation rule failed). Common in
/// JSON APIs to distinguish a 400-shaped parse error from a
/// validation-failure response.
#[must_use]
pub fn unprocessable_entity(message: impl Into<String>) -> Response {
    let body = message.into();
    let mut res = Response::builder()
        .status(StatusCode::UNPROCESSABLE_ENTITY)
        .body(axum::body::Body::from(body))
        .expect("422 + text body is always valid");
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    res
}

/// Build a download response — sets `Content-Type` plus
/// `Content-Disposition: attachment; filename="..."` so browsers
/// save the body to disk rather than rendering it inline. Django's
/// [`FileResponse(as_attachment=True, filename=...)`](https://docs.djangoproject.com/en/6.0/ref/request-response/#fileresponse-objects).
///
/// Use for CSV exports, generated PDFs, audit logs, anything the
/// user is meant to download rather than view in the browser.
///
/// ```ignore
/// use rustango::shortcuts::file_response;
///
/// async fn export_csv(...) -> Response {
///     let csv = build_report().await?;
///     file_response(csv.into_bytes(), "report.csv", "text/csv")
/// }
/// ```
///
/// `filename` characters are sanitized so a forged filename can't
/// inject extra `Content-Disposition` directives. Specifically: any
/// `"` is replaced with `_`, and CR/LF are stripped (header-splitting
/// guard). Non-ASCII filenames are *kept* in the value — modern
/// browsers handle UTF-8 in `filename=` fine; if RFC 5987 `filename*`
/// matters for you, build the header value yourself.
#[must_use]
pub fn file_response(
    content: impl Into<axum::body::Bytes>,
    filename: &str,
    content_type: &str,
) -> Response {
    let bytes = content.into();
    let safe_filename = sanitize_attachment_filename(filename);
    let mut res = Response::builder()
        .status(StatusCode::OK)
        .body(axum::body::Body::from(bytes))
        .expect("200 + body is always valid");
    let ct = HeaderValue::from_str(content_type)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    res.headers_mut().insert(header::CONTENT_TYPE, ct);
    let cd = format!(r#"attachment; filename="{safe_filename}""#);
    if let Ok(v) = HeaderValue::from_str(&cd) {
        res.headers_mut().insert(header::CONTENT_DISPOSITION, v);
    }
    res
}

/// Strip characters that would let an attacker forge extra
/// directives in a `Content-Disposition` header value via a
/// filename argument:
/// - Double quotes break out of the `filename="..."` quoting.
/// - CR/LF would split the header.
///
/// Replace these with `_` so the filename is still useful.
fn sanitize_attachment_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '"' | '\r' | '\n' => '_',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_message_round_trips() {
        let err = ShortcutError::not_found("post #42 not found");
        assert_eq!(err.to_string(), "post #42 not found");
    }

    #[tokio::test]
    async fn not_found_into_response_is_404() {
        let err = ShortcutError::not_found("missing");
        let res = err.into_response();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn database_error_into_response_is_500_not_404() {
        // Regression — early draft of this module had a
        // `From<ExecError> for Http404` impl that silently routed DB
        // failures through a 404 response. Pin the correct split:
        // NotFound → 404, Database → 500.
        let exec_err = ExecError::Sql(crate::sql::SqlError::EmptyInList);
        let err = ShortcutError::Database(exec_err);
        let res = err.into_response();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn from_exec_error_routes_to_database_variant() {
        let exec_err = ExecError::Sql(crate::sql::SqlError::EmptyInList);
        let err: ShortcutError = exec_err.into();
        assert!(matches!(err, ShortcutError::Database(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn render_template_returns_html_200() {
        let mut tera = tera::Tera::default();
        tera.add_raw_template("hello", "Hello, {{ name }}!")
            .unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("name", "alice");

        let res = render(&tera, "hello", &ctx);
        assert_eq!(res.status(), StatusCode::OK);
        // Content-type defaults to text/html via axum's Html wrapper.
        let ct = res
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .expect("content-type");
        assert!(ct.to_str().unwrap().starts_with("text/html"), "got: {ct:?}");
    }

    #[tokio::test]
    async fn render_template_missing_template_is_500() {
        let tera = tera::Tera::default();
        let ctx = tera::Context::new();
        let res = render(&tera, "nope.html", &ctx);
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn redirect_is_302_with_location_header() {
        let res = redirect("/posts/42");
        assert_eq!(res.status(), StatusCode::FOUND);
        let loc = res
            .headers()
            .get(axum::http::header::LOCATION)
            .expect("location");
        assert_eq!(loc.to_str().unwrap(), "/posts/42");
    }

    #[tokio::test]
    async fn redirect_permanent_is_301_with_location_header() {
        let res = redirect_permanent("/posts/42");
        assert_eq!(res.status(), StatusCode::MOVED_PERMANENTLY);
        let loc = res
            .headers()
            .get(axum::http::header::LOCATION)
            .expect("location");
        assert_eq!(loc.to_str().unwrap(), "/posts/42");
    }

    #[tokio::test]
    async fn redirect_see_other_is_303_with_location_header() {
        let res = redirect_see_other("/items");
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let loc = res
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(loc, "/items");
    }

    #[tokio::test]
    async fn redirect_temporary_is_307_with_location_header() {
        let res = redirect_temporary("/api/v1-new/widgets");
        assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
        let loc = res
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(loc, "/api/v1-new/widgets");
    }

    #[tokio::test]
    async fn redirect_permanent_preserve_method_is_308() {
        let res = redirect_permanent_preserve_method("/api/v2/widgets");
        assert_eq!(res.status(), StatusCode::PERMANENT_REDIRECT);
    }

    #[tokio::test]
    async fn redirect_with_crlf_drops_location_header_no_response_splitting() {
        // CRLF in a Location header would be a response-splitting
        // vector. `HeaderValue::from_str` rejects it, and our builder
        // drops the header silently rather than panicking. Status
        // stays 302 so the failure is visible (no redirect happens).
        let res = redirect("/posts/42\r\nSet-Cookie: pwned=1");
        assert_eq!(res.status(), StatusCode::FOUND);
        assert!(
            res.headers().get(axum::http::header::LOCATION).is_none(),
            "CRLF-injected URL must NOT produce a Location header"
        );
    }

    // ---------------- render_to_string ----------------

    #[test]
    fn render_to_string_returns_rendered_body() {
        let mut tera = tera::Tera::default();
        tera.add_raw_template("hi", "Hello, {{ name }}!").unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("name", "alice");
        let out = render_to_string(&tera, "hi", &ctx).unwrap();
        assert_eq!(out, "Hello, alice!");
    }

    #[test]
    fn render_to_string_propagates_template_errors() {
        let tera = tera::Tera::default();
        let ctx = tera::Context::new();
        let err = render_to_string(&tera, "absent.html", &ctx).unwrap_err();
        // The error string should mention the template name so the
        // caller can debug without a stack trace. Tera's exact
        // formatting varies; just check the name shows up somewhere.
        let s = format!("{err}");
        assert!(s.contains("absent") || s.contains("not found"), "got: {s}");
    }

    // ---------------- redirect_to_login ----------------

    #[tokio::test]
    async fn redirect_to_login_appends_next_with_question_mark() {
        let res = redirect_to_login("/profile", "/login");
        assert_eq!(res.status(), StatusCode::FOUND);
        let loc = res
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(loc, "/login?next=%2Fprofile");
    }

    #[tokio::test]
    async fn redirect_to_login_appends_next_with_ampersand_when_url_has_query() {
        let res = redirect_to_login("/profile", "/login?lang=fr");
        let loc = res
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(loc, "/login?lang=fr&next=%2Fprofile");
    }

    #[tokio::test]
    async fn redirect_to_login_encodes_special_chars_in_next() {
        // Path with spaces, ampersand, slashes — all must survive the
        // round-trip back into the login handler as a clean ?next=.
        let res = redirect_to_login("/posts/42?utm=ad&q=spaces here", "/login");
        let loc = res
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(loc.starts_with("/login?next="), "got: {loc}");
        // Spaces / ? / & all become percent-escaped.
        assert!(!loc[12..].contains(' '), "next= must percent-escape spaces");
        assert!(!loc[12..].contains('&'), "next= must percent-escape &");
        assert!(!loc[12..].contains('?'), "next= must percent-escape ?");
    }

    // ---------------- json_response / json_ok / json_bad_request ----------------

    async fn body_bytes(res: Response) -> Vec<u8> {
        axum::body::to_bytes(res.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .to_vec()
    }

    #[tokio::test]
    async fn json_response_emits_status_content_type_and_serialized_body() {
        let res = json_response(&serde_json::json!({"id": 1, "name": "Alice"}), 201);
        assert_eq!(res.status(), StatusCode::CREATED);
        assert_eq!(
            res.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "application/json"
        );
        let body = body_bytes(res).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v, serde_json::json!({"id": 1, "name": "Alice"}));
    }

    #[tokio::test]
    async fn json_ok_is_200() {
        let res = json_ok(&serde_json::json!({"hello": "world"}));
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn json_bad_request_is_400() {
        let res = json_bad_request(&serde_json::json!({"error": "validation"}));
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let body = body_bytes(res).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"], "validation");
    }

    #[tokio::test]
    async fn json_response_status_below_100_falls_back_to_200() {
        // Valid HTTP statuses are 100-999. Anything below 100 fails
        // `from_u16` and the helper falls back to 200 OK so the
        // response still parses.
        let res = json_response(&serde_json::json!({}), 42);
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn json_response_serializes_arbitrary_struct() {
        #[derive(serde::Serialize)]
        struct Out {
            ok: bool,
            count: u32,
        }
        let res = json_response(&Out { ok: true, count: 7 }, 200);
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_bytes(res).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v, serde_json::json!({"ok": true, "count": 7}));
    }

    #[tokio::test]
    async fn json_unauthorized_is_401() {
        let res = json_unauthorized(&serde_json::json!({"error": "login required"}));
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn json_forbidden_is_403() {
        let res = json_forbidden(&serde_json::json!({"error": "no access"}));
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn json_not_found_is_404() {
        let res = json_not_found(&serde_json::json!({"error": "no such item"}));
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn json_server_error_is_500() {
        let res = json_server_error(&serde_json::json!({"error": "internal"}));
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn json_error_helpers_all_emit_application_json_content_type() {
        // Pin the content-type contract across every sugar variant
        // — a single switched-off helper would silently break
        // browser dev-tools "Parsed as JSON" indicators and tests
        // that assertContent-Type.
        let cases: Vec<axum::response::Response> = vec![
            json_unauthorized(&serde_json::json!({})),
            json_forbidden(&serde_json::json!({})),
            json_not_found(&serde_json::json!({})),
            json_server_error(&serde_json::json!({})),
        ];
        for res in cases {
            let ct = res
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            assert_eq!(ct, "application/json");
        }
    }

    // ---------------- html_response / text_response ----------------

    #[tokio::test]
    async fn html_response_emits_html_content_type_and_body() {
        let res = html_response("<h1>Hello</h1>", 200);
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "text/html; charset=utf-8"
        );
        let body = body_bytes(res).await;
        assert_eq!(String::from_utf8(body).unwrap(), "<h1>Hello</h1>");
    }

    #[tokio::test]
    async fn html_response_respects_custom_status() {
        let res = html_response("<h1>Down</h1>", 503);
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn html_response_invalid_status_falls_back_to_200() {
        let res = html_response("x", 42);
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn text_response_emits_text_content_type_and_body() {
        let res = text_response("ok", 200);
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "text/plain; charset=utf-8"
        );
        let body = body_bytes(res).await;
        assert_eq!(String::from_utf8(body).unwrap(), "ok");
    }

    #[tokio::test]
    async fn text_response_respects_custom_status() {
        let res = text_response("teapot", 418);
        assert_eq!(res.status(), StatusCode::IM_A_TEAPOT);
    }

    // ---------------- file_response ----------------

    #[tokio::test]
    async fn file_response_emits_attachment_disposition_with_filename() {
        let res = file_response(b"a,b,c\n1,2,3\n".to_vec(), "report.csv", "text/csv");
        assert_eq!(res.status(), StatusCode::OK);
        let cd = res
            .headers()
            .get(axum::http::header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(cd, r#"attachment; filename="report.csv""#);
    }

    #[tokio::test]
    async fn file_response_passes_through_content_type() {
        let res = file_response(b"PDF...".to_vec(), "x.pdf", "application/pdf");
        let ct = res
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(ct, "application/pdf");
    }

    #[tokio::test]
    async fn file_response_returns_body_bytes_verbatim() {
        let body = b"raw bytes \xFF\xFE here".to_vec();
        let res = file_response(body.clone(), "x.bin", "application/octet-stream");
        let got = body_bytes(res).await;
        assert_eq!(got, body);
    }

    #[tokio::test]
    async fn file_response_sanitizes_quotes_in_filename() {
        // A quote in the filename would let the attacker append
        // extra Content-Disposition directives. Replace with _.
        let res = file_response(b"".to_vec(), r#"a"; injected=bad.txt"#, "text/plain");
        let cd = res
            .headers()
            .get(axum::http::header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        // The quote must be gone. The injected directive may still
        // appear as text but it's now inside the `filename="..."`
        // quoting and harmless.
        assert!(
            !cd.contains('"').then(|| ()).is_none() || !cd.contains("\";"),
            "got: {cd}"
        );
        assert!(cd.contains("filename=\"a_;"), "got: {cd}");
    }

    #[tokio::test]
    async fn file_response_sanitizes_crlf_in_filename_no_header_splitting() {
        let res = file_response(b"".to_vec(), "a\r\nX-Hack: y.txt", "text/plain");
        let cd = res
            .headers()
            .get(axum::http::header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(!cd.contains('\r'));
        assert!(!cd.contains('\n'));
    }

    #[tokio::test]
    async fn file_response_invalid_content_type_falls_back_to_octet_stream() {
        // A content-type value with a header-invalid byte (raw NUL)
        // gets replaced with octet-stream so the response still
        // builds.
        let res = file_response(b"".to_vec(), "x.bin", "text/x\0bad");
        let ct = res
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(ct, "application/octet-stream");
    }

    // -------- resolve_url + redirect_to_view (Django parity) --------

    use std::collections::HashMap;

    crate::register_url!("shortcuts_test_detail", "/posts/{id}");
    crate::register_url!("shortcuts_test_index", "/posts");

    #[test]
    fn resolve_url_passes_through_absolute_path() {
        let p = HashMap::new();
        assert_eq!(resolve_url("/posts/42", &p).unwrap(), "/posts/42");
    }

    #[test]
    fn resolve_url_passes_through_https_url() {
        let p = HashMap::new();
        assert_eq!(
            resolve_url("https://example.com/x", &p).unwrap(),
            "https://example.com/x"
        );
    }

    #[test]
    fn resolve_url_passes_through_http_url() {
        let p = HashMap::new();
        assert_eq!(
            resolve_url("http://example.com/x", &p).unwrap(),
            "http://example.com/x"
        );
    }

    #[test]
    fn resolve_url_passes_through_relative_url() {
        let p = HashMap::new();
        assert_eq!(resolve_url("./next", &p).unwrap(), "./next");
        assert_eq!(resolve_url("../up", &p).unwrap(), "../up");
    }

    #[test]
    fn resolve_url_reverses_named_route() {
        let mut p = HashMap::new();
        p.insert("id", "42".to_string());
        assert_eq!(
            resolve_url("shortcuts_test_detail", &p).unwrap(),
            "/posts/42"
        );
    }

    #[test]
    fn resolve_url_reverses_paramless_named_route() {
        let p = HashMap::new();
        assert_eq!(resolve_url("shortcuts_test_index", &p).unwrap(), "/posts");
    }

    #[test]
    fn resolve_url_unknown_name_surfaces_reverse_error() {
        let p = HashMap::new();
        assert!(resolve_url("no_such_route_xyz", &p).is_err());
    }

    #[tokio::test]
    async fn redirect_to_view_builds_302_with_resolved_location() {
        let mut p = HashMap::new();
        p.insert("id", "7".to_string());
        let resp = redirect_to_view("shortcuts_test_detail", &p).unwrap();
        assert_eq!(resp.status(), StatusCode::FOUND);
        let loc = resp
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(loc, "/posts/7");
    }

    #[test]
    fn redirect_to_view_unknown_name_is_err() {
        let p = HashMap::new();
        assert!(redirect_to_view("no_such_route_xyz_for_redirect", &p).is_err());
    }

    // -------- not_modified + gone (Django parity) --------

    #[tokio::test]
    async fn not_modified_is_304_with_empty_body() {
        let res = not_modified();
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
        let bytes = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert!(bytes.is_empty(), "304 must have empty body per RFC 7232");
    }

    #[tokio::test]
    async fn not_modified_omits_content_type_header() {
        // RFC 7232 §4.1: server SHOULD NOT send Content-Type on 304.
        let res = not_modified();
        assert!(res.headers().get(header::CONTENT_TYPE).is_none());
    }

    #[tokio::test]
    async fn gone_is_410_with_body() {
        let res = gone("user deleted");
        assert_eq!(res.status(), StatusCode::GONE);
        let bytes = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert_eq!(&bytes[..], b"user deleted");
    }

    #[tokio::test]
    async fn gone_sets_text_plain_content_type() {
        let res = gone("");
        let ct = res
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(ct, "text/plain; charset=utf-8");
    }

    #[tokio::test]
    async fn gone_accepts_empty_body() {
        let res = gone("");
        assert_eq!(res.status(), StatusCode::GONE);
        let bytes = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert!(bytes.is_empty());
    }

    // -------- no_content / accepted / unsupported_media_type / conflict / unprocessable_entity --------

    #[tokio::test]
    async fn no_content_is_204_with_empty_body() {
        let res = no_content();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let bytes = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert!(bytes.is_empty(), "204 must have empty body per RFC 7231");
    }

    #[tokio::test]
    async fn accepted_is_202_with_body() {
        let res = accepted("Job 42 queued");
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        let bytes = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert_eq!(&bytes[..], b"Job 42 queued");
    }

    #[tokio::test]
    async fn unsupported_media_type_is_415() {
        let res = unsupported_media_type("Only application/json accepted");
        assert_eq!(res.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let bytes = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert!(bytes.starts_with(b"Only "));
    }

    #[tokio::test]
    async fn conflict_is_409() {
        let res = conflict("Concurrent edit detected");
        assert_eq!(res.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn unprocessable_entity_is_422() {
        let res = unprocessable_entity("Validation failed: title required");
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert!(std::str::from_utf8(&bytes).unwrap().contains("required"));
    }

    #[tokio::test]
    async fn status_helpers_set_text_plain_content_type() {
        for res in [
            accepted(""),
            unsupported_media_type(""),
            conflict(""),
            unprocessable_entity(""),
        ] {
            let ct = res
                .headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            assert_eq!(ct, "text/plain; charset=utf-8");
        }
    }
}
