//! Django-shape view shortcuts — the helpers every page render needs, so
//! handlers do not reach for status codes, Tera and redirect builders by
//! hand. Mirrors the Django
//! [shortcuts module](https://docs.djangoproject.com/en/6.0/topics/http/shortcuts/).
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
//! Behind the `template_views` feature, which brings in axum and Tera.

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::core::Model;
use crate::query::QuerySet;
use crate::sql::{
    ExecError, FetcherPool, LoadRelated, MaybeMyFromRow, MaybeMyLoadRelated, MaybePgFromRow,
    MaybeSqliteFromRow, MaybeSqliteLoadRelated, Pool,
};

/// Error returned by [`get_object_or_404`] and [`get_list_or_404`]. The
/// two variants keep "no row matched" apart from a driver failure, so a
/// database outage never shows up as a 404.
///
/// It implements [`IntoResponse`], so a handler returning
/// `Result<_, ShortcutError>` gets the right status on its own:
///
/// - [`Self::NotFound`] → `404 Not Found`, with `message` as the body.
/// - [`Self::Database`] → `500 Internal Server Error`, with the
///   [`ExecError`] in the body.
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
    /// Driver errors become `Database`, which renders as `500`, so `?` in
    /// a handler never turns a database failure into a 404.
    fn from(e: ExecError) -> Self {
        Self::Database(e)
    }
}

/// Fetch the first row matching `qs`, or return
/// [`ShortcutError::NotFound`]. Django's
/// [`get_object_or_404`](https://docs.djangoproject.com/en/6.0/topics/http/shortcuts/#get-object-or-404).
///
/// Add whatever filters and ordering you need to the queryset first; this
/// only collapses the "fetch one or 404" branch.
///
/// ```ignore
/// let post = get_object_or_404(
///     Post::objects().where_(Post::slug.eq("hello-world")),
///     &state.pool,
/// ).await?;
/// ```
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

/// Fetch every row matching `qs`, or return [`ShortcutError::NotFound`]
/// when there are none. Django's
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

/// Render a Tera template into an HTML response. Django's
/// [`render(request, template, context)`](https://docs.djangoproject.com/en/6.0/topics/http/shortcuts/#render).
///
/// A render failure returns `500 Internal Server Error` with the Tera
/// error in the body. For a nicer error page, render your own error
/// template with this same helper.
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

/// Render a Tera template to a `String`. Django's
/// [`render_to_string(template_name, context)`](https://docs.djangoproject.com/en/6.0/topics/templates/#django.template.loader.render_to_string).
///
/// Use it when the output is not an HTTP body: emails, reports, PDF
/// source, snapshot tests, or a string spliced into a parent context.
///
/// Unlike [`render`], which turns every failure into a 500, this returns
/// the [`tera::Error`] so you can tell a missing template from a render
/// error.
///
/// ```ignore
/// let mut ctx = tera::Context::new();
/// ctx.insert("user", &user);
/// let email_body = render_to_string(&tera, "welcome_email.txt", &ctx)?;
/// send_email(&user.email, "Welcome", &email_body).await?;
/// ```
///
/// # Errors
/// Returns [`tera::Error`] when the template is missing, fails to parse,
/// or a filter or function inside it fails.
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
/// See [`redirect_permanent`] for 301, [`redirect_see_other`] for 303,
/// and [`redirect_to_view`] for Django's `redirect('post-detail', pk=1)`
/// shape, which resolves a route name.
///
/// This matches Django's 302. Note axum's own `Redirect::to` sends 303
/// instead, which treats the request method differently.
#[must_use]
pub fn redirect(url: impl Into<String>) -> Response {
    build_redirect(StatusCode::FOUND, url.into())
}

/// Turn either a raw URL or a registered route name into a URL string.
/// Django's
/// [`resolve_url`](https://docs.djangoproject.com/en/6.0/topics/http/shortcuts/#resolve-url).
///
/// A `spec` starting with `/`, `http://`, `https://`, `./` or `../` is
/// already a URL and comes back unchanged. Anything else is a route name
/// passed to [`crate::urls::reverse`] with `params`, including namespaced
/// names such as `"polls:detail"`.
///
/// # Errors
/// Forwards [`crate::urls::ReverseError`] when `spec` is a name and
/// `params` does not fit the pattern.
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

/// Resolve a route name and params into a URL and return a `302 Found`
/// to it. Django's `redirect('view-name', kwargs={'pk': 1})`. See also
/// [`resolve_url`].
///
/// # Errors
/// Forwards [`crate::urls::ReverseError`] when the name or params do not
/// match a registered route.
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

/// Return a `301 Moved Permanently` redirect to `url`. Use it when a URL
/// has moved for good; search engines treat 301 and 302 differently.
#[must_use]
pub fn redirect_permanent(url: impl Into<String>) -> Response {
    build_redirect(StatusCode::MOVED_PERMANENTLY, url.into())
}

/// Return a `303 See Other` redirect to `url`. This is the right code
/// after a successful POST: the client must follow with `GET`, so a page
/// refresh does not re-submit the form.
///
/// [`redirect`] (302) usually behaves the same way, but the spec leaves
/// that up to the client for non-GET requests. 303 is unambiguous.
#[must_use]
pub fn redirect_see_other(url: impl Into<String>) -> Response {
    build_redirect(StatusCode::SEE_OTHER, url.into())
}

/// Return a `307 Temporary Redirect` to `url`. Unlike 302 and 303, the
/// client must keep the same request method. Use it when a resource has
/// moved for now but the verb still applies, such as sending
/// `POST /api/v1/...` on to `POST /api/v1-new/...`.
///
/// After a form submit use [`redirect_see_other`]; for a permanent move
/// use [`redirect_permanent`].
#[must_use]
pub fn redirect_temporary(url: impl Into<String>) -> Response {
    build_redirect(StatusCode::TEMPORARY_REDIRECT, url.into())
}

/// Return a `308 Permanent Redirect` to `url`. Same meaning as
/// [`redirect_permanent`] (301), but the client must keep the request
/// method. Useful for moving API endpoints.
#[must_use]
pub fn redirect_permanent_preserve_method(url: impl Into<String>) -> Response {
    build_redirect(StatusCode::PERMANENT_REDIRECT, url.into())
}

/// Redirect to a login page with the current URL as `?next=<path>`, so
/// the login handler can send the user back afterwards. Django's
/// [`redirect_to_login(next, login_url)`](https://docs.djangoproject.com/en/6.0/_modules/django/contrib/auth/views/#redirect_to_login).
///
/// `next` is URL-encoded. It is joined with `&` when `login_url` already
/// has a query string, and with `?` otherwise.
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
/// Returns a `302 Found`, the same status as [`redirect`].
#[must_use]
pub fn redirect_to_login(next: &str, login_url: &str) -> Response {
    let encoded = crate::url_codec::url_encode(next);
    let separator = if login_url.contains('?') { '&' } else { '?' };
    redirect(format!("{login_url}{separator}next={encoded}"))
}

fn build_redirect(status: StatusCode, url: String) -> Response {
    // Built by hand so the status matches Django (302/301) instead of
    // axum's default (303/308). A URL with characters a header cannot
    // hold yields a response with no Location; that is a caller bug, and
    // the bare status still helps debugging.
    let mut res = Response::builder()
        .status(status)
        .body(axum::body::Body::empty())
        .expect("status + empty body is always valid");
    if let Ok(v) = HeaderValue::from_str(&url) {
        res.headers_mut().insert(header::LOCATION, v);
    }
    res
}

/// Serialize `data` to JSON and return it with the given `status` and
/// `Content-Type: application/json`. Django's
/// [`JsonResponse(data, status=...)`](https://docs.djangoproject.com/en/6.0/ref/request-response/#jsonresponse-objects).
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
/// If serialization fails you get `500` with an empty body. An invalid
/// `status` falls back to `200 OK`, so the response still parses.
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

/// [`json_response`] with `200 OK`.
#[must_use]
pub fn json_ok<T: serde::Serialize>(data: &T) -> Response {
    json_response(data, 200)
}

/// [`json_response`] with `400 Bad Request`, for input that failed
/// validation.
#[must_use]
pub fn json_bad_request<T: serde::Serialize>(data: &T) -> Response {
    json_response(data, 400)
}

/// [`json_response`] with `401 Unauthorized`, for a request with no
/// credentials. Pairs with
/// [`crate::auth_decorators::login_required_or_401`].
#[must_use]
pub fn json_unauthorized<T: serde::Serialize>(data: &T) -> Response {
    json_response(data, 401)
}

/// [`json_response`] with `403 Forbidden`, for a caller who is logged in
/// but lacks the permission. Pairs with
/// [`crate::auth_decorators::user_passes_test_or_403`].
#[must_use]
pub fn json_forbidden<T: serde::Serialize>(data: &T) -> Response {
    json_response(data, 403)
}

/// [`json_response`] with `404 Not Found`. Use it instead of
/// [`ShortcutError::NotFound`], which sends `text/plain`, when the
/// endpoint always answers in JSON.
#[must_use]
pub fn json_not_found<T: serde::Serialize>(data: &T) -> Response {
    json_response(data, 404)
}

/// [`json_response`] with `500 Internal Server Error`, so a caught
/// failure still returns a JSON body the client can branch on.
#[must_use]
pub fn json_server_error<T: serde::Serialize>(data: &T) -> Response {
    json_response(data, 500)
}

/// Return an HTML string with the given status and
/// `Content-Type: text/html; charset=utf-8`. Django's
/// [`HttpResponse(html, status=...)`](https://docs.djangoproject.com/en/6.0/ref/request-response/#httpresponse-objects).
///
/// Use it when the HTML is already in hand and Tera is not worth
/// starting. For Tera output, use [`render`], or [`render_to_string`]
/// and pass the result here.
///
/// ```ignore
/// use rustango::shortcuts::html_response;
///
/// async fn maintenance() -> Response {
///     html_response("<h1>Down for maintenance</h1>", 503)
/// }
/// ```
///
/// An invalid `status` falls back to `200 OK` rather than panicking.
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

/// Return a plain-text string with the given status and
/// `Content-Type: text/plain; charset=utf-8`. Handy for small ops
/// endpoints such as `/health` or `/version`.
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

/// `304 Not Modified` with no body, for conditional GET when the
/// client's cached copy is still fresh. Django's
/// [`HttpResponseNotModified()`](https://docs.djangoproject.com/en/6.0/ref/request-response/#django.http.HttpResponseNotModified).
///
/// A 304 must not carry a body, so the body is always empty. Copy any
/// `ETag`, `Last-Modified` or `Cache-Control` headers yourself; Django
/// does not copy them either.
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

/// `410 Gone` with an optional `text/plain` message. Use it when a
/// resource existed and was removed on purpose: clients and crawlers
/// should drop it from caches and indexes. `404 Not Found` says
/// something weaker — that the server does not know the URL at all.
/// Django's
/// [`HttpResponseGone`](https://docs.djangoproject.com/en/6.0/ref/request-response/#django.http.HttpResponseGone).
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

/// `204 No Content` with an empty body, for a write that succeeded and
/// has nothing to return: DELETE handlers, or a PUT/PATCH that does not
/// echo the resource. A 204 must have no body, so none can be sent.
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

/// `202 Accepted`, for a request that was queued but has not run yet.
/// Put the job ID or a status URL in the body. Adding a `Location:`
/// header pointing at the status endpoint is the usual shape for
/// async-job APIs.
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

/// `415 Unsupported Media Type`, for a request body whose
/// `Content-Type` the handler cannot parse. Put the types you do accept
/// in the `text/plain` message.
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

/// `409 Conflict`, for a request that clashes with the current state of
/// the resource: a concurrent edit, a unique-constraint hit caught
/// before INSERT, or a stale write token.
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

/// `422 Unprocessable Entity`, for a request that parsed fine but broke
/// a validation rule. JSON APIs use it to keep parse errors (400) apart
/// from validation errors.
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

/// Build a download response: sets `Content-Type` and
/// `Content-Disposition: attachment; filename="..."`, so the browser
/// saves the body instead of showing it. Use it for CSV exports,
/// generated PDFs and the like. Django's
/// [`FileResponse(as_attachment=True, filename=...)`](https://docs.djangoproject.com/en/6.0/ref/request-response/#fileresponse-objects).
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
/// `filename` is cleaned so it cannot inject extra
/// `Content-Disposition` directives: `"`, CR and LF each become `_`.
/// Non-ASCII characters are kept, which modern browsers handle. If you
/// need the RFC 5987 `filename*` form, build the header yourself.
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

/// Replace with `_` the characters that would let a filename forge extra
/// `Content-Disposition` directives: `"` escapes the `filename="..."`
/// quoting, and CR or LF would split the header.
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
