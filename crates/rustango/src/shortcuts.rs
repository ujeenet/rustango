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
    let rows = qs.fetch_pool(pool).await?;
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
/// Pair with [`redirect_permanent`] for `301 Moved Permanently`.
///
/// Matches Django's status codes exactly (302 / 301) — axum's built-in
/// `Redirect::to` uses 303 See Other instead, which has subtly
/// different semantics around method preservation.
///
/// **View-name resolution** — Django's `redirect('post-detail', pk=1)`
/// shape that resolves a view name through the URL conf — depends on
/// named URL reversal which lands in #8. For now, pass the URL
/// directly, optionally formatted in the caller:
///
/// ```ignore
/// redirect(format!("/posts/{}", post.id))
/// ```
#[must_use]
pub fn redirect(url: impl Into<String>) -> Response {
    build_redirect(StatusCode::FOUND, url.into())
}

/// Return a `301 Moved Permanently` redirect to `url`. Use for
/// canonical URL migrations (search engines treat 301 differently
/// from the default 302).
#[must_use]
pub fn redirect_permanent(url: impl Into<String>) -> Response {
    build_redirect(StatusCode::MOVED_PERMANENTLY, url.into())
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
}
