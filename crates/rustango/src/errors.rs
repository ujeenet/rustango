//! HTML error pages: one layer that dresses every 4xx/5xx of an app.
//!
//! ```ignore
//! use rustango::errors::{ErrorPagesLayer, ErrorPagesRouterExt as _, PageError};
//!
//! async fn edit() -> Result<String, PageError> {
//!     Err(PageError::forbidden("Ask an admin for the editor role."))
//! }
//!
//! let app = Router::new()
//!     .route("/edit", get(edit))
//!     .fallback(rustango::errors::not_found)
//!     .error_pages(ErrorPagesLayer::new().templates(tera.clone()));
//! ```
//!
//! Who asked decides the body:
//!
//! - a browser navigating (`Accept: text/html`) gets a full page;
//! - htmx (`HX-Request`) gets a fragment, with `HX-Retarget` / `HX-Reswap`;
//! - any other client gets the body unchanged, or the [`Message`] as plain
//!   text when the body is empty.
//!
//! JSON bodies pass through unless they carry a [`Message`] and a browser
//! asked. `HEAD`, non-text bodies and responses marked [`KeepBody`] always
//! pass through. A 5xx shows generic text unless `RUSTANGO_DISCLOSE_ERRORS`
//! is set, the same policy as every other 5xx body.
//!
//! Templates come from the Tera you pass; the first that exists wins:
//! `errors/{code}.html`, `errors/{4|5}xx.html`, `errors/default.html`, then
//! the built-in page. htmx fragments use `errors/partial.html`, then the
//! built-in one. Context: `code`, `title`, `message`, `path`, `user`
//! (`{username, is_superuser}` or null, with `tenancy`), `username`,
//! `is_superuser`, `csrf_token`, `csrf_input`, `csp_nonce` and the
//! `brand_*` keys. A template that fails to render falls back to the
//! built-in page.

use axum::response::{IntoResponse, IntoResponseParts, Response, ResponseParts};

/// A message for the user on an error response, for the error page to
/// show. On a 5xx it is shown only when error disclosure is on.
///
/// ```ignore
/// (StatusCode::FORBIDDEN, rustango::errors::Message::new("Ask Bob for access."))
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message(pub String);

impl Message {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl IntoResponseParts for Message {
    type Error = std::convert::Infallible;

    fn into_response_parts(self, mut res: ResponseParts) -> Result<ResponseParts, Self::Error> {
        res.extensions_mut().insert(self);
        Ok(res)
    }
}

/// An empty body carrying the message: `(StatusCode::GONE, Message::new("…"))`.
impl IntoResponse for Message {
    fn into_response(self) -> Response {
        let mut res = ().into_response();
        res.extensions_mut().insert(self);
        res
    }
}

/// Marks a response whose error body the handler made on purpose; the
/// error-pages layer leaves it as it is.
#[derive(Debug, Clone, Copy, Default)]
pub struct KeepBody;

impl IntoResponseParts for KeepBody {
    type Error = std::convert::Infallible;

    fn into_response_parts(self, mut res: ResponseParts) -> Result<ResponseParts, Self::Error> {
        res.extensions_mut().insert(self);
        Ok(res)
    }
}

/// Marks every HTML 4xx/5xx of a framework router as [`KeepBody`]: such a
/// page (a form re-shown with its error) is rendered on purpose.
#[cfg(feature = "admin")]
pub(crate) fn own_pages<S: Clone + Send + Sync + 'static>(
    router: axum::Router<S>,
) -> axum::Router<S> {
    router.layer(axum::middleware::map_response(
        |mut res: Response| async move {
            keep_own_page(&mut res);
            res
        },
    ))
}

/// [`KeepBody`] on `res` when it is an HTML error page.
#[cfg(any(feature = "admin", feature = "template_views"))]
pub(crate) fn keep_own_page(res: &mut Response) {
    let status = res.status();
    let html = res
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| {
            ct.trim_start()
                .to_ascii_lowercase()
                .starts_with("text/html")
        });
    if html && (status.is_client_error() || status.is_server_error()) {
        res.extensions_mut().insert(KeepBody);
    }
}

/// A handler error for the error page: a 4xx carries its message, a
/// 5xx logs its cause and keeps it out of the body (unless disclosed).
#[derive(Debug)]
pub struct PageError {
    status: axum::http::StatusCode,
    message: String,
}

impl PageError {
    /// `status` with a user-facing `message`. A 5xx goes through
    /// [`Self::server`], so its text is logged and withheld.
    #[must_use]
    pub fn new(status: axum::http::StatusCode, message: impl Into<String>) -> Self {
        let message = message.into();
        if status.is_server_error() {
            let mut e = Self::server(message);
            e.status = status;
            return e;
        }
        Self { status, message }
    }

    /// A logged `500`; the body is the opaque server-error text.
    #[must_use]
    pub fn server(cause: impl std::fmt::Display) -> Self {
        Self {
            status: axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            message: crate::error::server_error_body("errors::PageError", &cause),
        }
    }

    /// `404` with a message.
    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(axum::http::StatusCode::NOT_FOUND, message)
    }

    /// `403` with a message.
    #[must_use]
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(axum::http::StatusCode::FORBIDDEN, message)
    }

    /// The status this error renders with.
    #[must_use]
    pub fn status(&self) -> axum::http::StatusCode {
        self.status
    }
}

impl std::fmt::Display for PageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl IntoResponse for PageError {
    /// The message as a plain-text body, and as [`Message`] for the page.
    fn into_response(self) -> Response {
        let body = self.message.clone();
        (self.status, Message(self.message), body).into_response()
    }
}

/// Unknown paths, as a 404 for the layer to dress:
/// `.fallback(rustango::errors::not_found)`.
pub async fn not_found() -> axum::http::StatusCode {
    axum::http::StatusCode::NOT_FOUND
}

/// Title, and the message when there is no better one, for a status.
#[must_use]
pub fn error_words(status: axum::http::StatusCode) -> (&'static str, &'static str) {
    match status.as_u16() {
        401 => (
            "Please sign in",
            "Your session has ended. Sign in again to continue.",
        ),
        403 => (
            "Not allowed",
            "You don't have permission for this. Ask an admin for a role that includes it.",
        ),
        404 => (
            "Not found",
            "This page doesn't exist, or it was moved or deleted.",
        ),
        405 => (
            "Not supported",
            "This address doesn't accept that kind of request.",
        ),
        408 | 504 => (
            "Took too long",
            "The request timed out. Try again in a moment.",
        ),
        413 => (
            "Too large",
            "That upload is bigger than the server accepts.",
        ),
        422 => (
            "Can't do that",
            "The request couldn't be processed. Check what you entered.",
        ),
        429 => (
            "Slow down",
            "Too many requests. Wait a moment and try again.",
        ),
        s if s >= 500 => (
            "Something went wrong",
            "That failed on our side. It has been logged; try again in a moment.",
        ),
        _ => ("Request failed", "The request couldn't be completed."),
    }
}

#[cfg(feature = "template_views")]
mod layer;
#[cfg(feature = "template_views")]
pub use layer::{ErrorPagesLayer, ErrorPagesRouterExt};
