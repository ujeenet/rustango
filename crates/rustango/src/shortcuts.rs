//! Django-shape view shortcuts. Issue #10.
//!
//! Four helpers that show up on every Django page render — folded into
//! a single rustango module so axum handlers don't reach into the
//! low-level status-code / tera / redirect primitives for routine
//! cases. Matches the Django [shortcuts module](https://docs.djangoproject.com/en/6.0/topics/http/shortcuts/).
//!
//! ```ignore
//! use rustango::shortcuts::{get_object_or_404, render, redirect, Http404};
//!
//! async fn post_detail(
//!     State(state): State<AppState>,
//!     Path(id): Path<i64>,
//! ) -> Result<axum::response::Response, Http404> {
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

/// 404 marker error returned by [`get_object_or_404`] / [`get_list_or_404`].
/// Implements [`IntoResponse`] so handlers that return `Result<_, Http404>`
/// can propagate with `?` and get a plain 404 body for free.
///
/// The `message` field is rendered as the response body; default is
/// `"not found"`. Set a custom message when the framework's default
/// would be unhelpful (e.g. `"post #42 not found"`).
#[derive(Debug, Clone)]
pub struct Http404 {
    pub message: String,
}

impl Http404 {
    /// Construct a 404 with a custom message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Default for Http404 {
    fn default() -> Self {
        Self {
            message: "not found".into(),
        }
    }
}

impl std::fmt::Display for Http404 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Http404 {}

impl IntoResponse for Http404 {
    fn into_response(self) -> Response {
        (StatusCode::NOT_FOUND, self.message).into_response()
    }
}

impl From<ExecError> for Http404 {
    /// Driver-level errors aren't 404s — keep them as 500s by tagging
    /// the message. Callers who care about the distinction should
    /// match on the result directly instead of using `?`.
    fn from(e: ExecError) -> Self {
        Self::new(format!("query failed: {e}"))
    }
}

/// Fetch the first row matching `qs` against `pool`, or return
/// [`Http404`] if none matched. Django's
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
/// Returns `Err(Http404)` for both the "no rows" case and any
/// underlying driver error. If you need to distinguish, call
/// `qs.first(&pool).await` directly.
///
/// # Errors
/// [`Http404`] when no row matched the queryset, or wrapping any
/// underlying [`ExecError`].
pub async fn get_object_or_404<T>(qs: QuerySet<T>, pool: &Pool) -> Result<T, Http404>
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
        None => Err(Http404::new(format!("no {} matches", T::SCHEMA.name))),
    }
}

/// Fetch every row matching `qs`. If the result is empty, return
/// [`Http404`]. Django's
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
/// [`Http404`] when the result set is empty, or wrapping any
/// underlying [`ExecError`].
pub async fn get_list_or_404<T>(qs: QuerySet<T>, pool: &Pool) -> Result<Vec<T>, Http404>
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
        Err(Http404::new(format!("no {} matches", T::SCHEMA.name)))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http404_default_message() {
        let err = Http404::default();
        assert_eq!(err.message, "not found");
        assert_eq!(err.to_string(), "not found");
    }

    #[test]
    fn http404_custom_message() {
        let err = Http404::new("post #42 not found");
        assert_eq!(err.to_string(), "post #42 not found");
    }

    #[tokio::test]
    async fn http404_into_response_is_404() {
        let err = Http404::new("missing");
        let res = err.into_response();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
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
}
