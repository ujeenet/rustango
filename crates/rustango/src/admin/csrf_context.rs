//! Request-scoped CSRF token for admin templates.
//!
//! Admin CSRF protection has two halves, and both are needed. The
//! `CsrfLayer` rejects an unsafe request without a valid token; this
//! module puts a token in every form. The layer alone turns every
//! admin mutation into a 403, and tokens alone protect nothing.
//!
//! It mints or reuses one token per request, parks it in a task-local
//! so `chrome_context` can pass it to every template, and attaches the
//! matching `Set-Cookie` itself. The layer must not add that cookie
//! later: by then the handler has rendered a token and the two would
//! disagree. `CsrfLayer` stands down when the response already sets
//! the cookie, which is what makes this safe.

use axum::body::Body;
use axum::extract::Request;
use axum::http::header::SET_COOKIE;
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;

use super::session::CURRENT_CSRF_TOKEN;

/// Install the request's CSRF token as a task-local, then attach the
/// cookie that pairs with it.
///
/// Mounted inside the auth gate so it runs for every admin page, read
/// pages included. A GET must seed the cookie, or the first POST from
/// a fresh browser has nothing to match against.
pub(crate) async fn csrf_context(request: Request<Body>, next: Next) -> Response {
    let (token, set_cookie) =
        crate::forms::csrf::ensure_token(request.headers(), crate::forms::csrf::CSRF_COOKIE);

    let mut response = CURRENT_CSRF_TOKEN.scope(token, next.run(request)).await;

    // Only when `ensure_token` minted one. It returns `None` if the
    // request already carried the cookie, and re-sending it would be
    // noise on every admin response.
    if let Some(cookie) = set_cookie {
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            response.headers_mut().append(SET_COOKIE, v);
        }
    }
    response
}
