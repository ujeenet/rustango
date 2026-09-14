//! Request-scoped CSRF token for admin templates (#1395).
//!
//! `docs/security.md` said the auto-admin "enables CSRF on every
//! mutation by default, and there is no way to opt out". Neither half
//! was true: the only protected route was `POST /login`, and `login.html`
//! was the only one of fourteen templates rendering a token. Every other
//! mutation — create, update, delete, bulk actions, audit cleanup —
//! accepted a cross-site POST carrying the admin's session cookie.
//!
//! Closing it needs two things in the same change: the layer that
//! rejects, and a token in every form. Either alone is useless — the
//! layer without the tokens turns every admin mutation into a 403, and
//! the tokens without the layer protect nothing.
//!
//! This is the second half. It mints (or reuses) the token once per
//! request, parks it in a task-local so `chrome_context` can put it in
//! every template's variables, and attaches the `Set-Cookie` the layer
//! would otherwise add later — later being too late, because the
//! handler has already rendered a token by then and the two would
//! disagree. `CsrfLayer` explicitly stands down when the response
//! already sets the cookie, which is what makes that safe.

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
/// Mounted inside the auth gate so it runs for every admin page,
/// including the ones that only read — a GET has to seed the cookie, or
/// the first POST from a fresh browser has nothing to match against.
pub(crate) async fn csrf_context(request: Request<Body>, next: Next) -> Response {
    let (token, set_cookie) =
        crate::forms::csrf::ensure_token(request.headers(), crate::forms::csrf::CSRF_COOKIE);

    let mut response = CURRENT_CSRF_TOKEN.scope(token, next.run(request)).await;

    // Only when `ensure_token` actually minted one. If the request
    // already carried the cookie it returns `None`, and re-sending it
    // would be noise on every admin response.
    if let Some(cookie) = set_cookie {
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            response.headers_mut().append(SET_COOKIE, v);
        }
    }
    response
}
