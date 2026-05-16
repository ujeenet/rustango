//! Django-shape assertion helpers for axum response objects. Issue #40.
//!
//! Quick assertions every Django test suite uses, ported to axum's
//! `Response` shape so test code reads tighter:
//!
//! ```ignore
//! use rustango::test_assertions::{assert_contains, assert_redirects, assert_status};
//! use tower::ServiceExt;
//!
//! #[tokio::test]
//! async fn home_renders_greeting() {
//!     let app = make_app();
//!     let res = app.oneshot(Request::builder().uri("/").body(Body::empty()).unwrap()).await.unwrap();
//!     assert_status(&res, 200);
//!     assert_contains(res, "Hello, world!").await;
//! }
//!
//! #[tokio::test]
//! async fn login_redirects_anonymous() {
//!     let res = app.oneshot(req("/profile")).await.unwrap();
//!     assert_redirects(&res, "/login?next=%2Fprofile");
//! }
//! ```
//!
//! All helpers `panic!` on mismatch — `cargo test` reports them as
//! failures with descriptive messages. They don't return `Result` so
//! tests stay readable (no `?` clutter for assertions).
//!
//! ## Implemented (this slice)
//!
//! - [`assert_status`] — exact-status match against a u16.
//! - [`assert_contains`] — response body contains a UTF-8 substring.
//! - [`assert_not_contains`] — body does NOT contain a substring.
//! - [`assert_redirects`] — 3xx status + `Location` header equality.
//! - [`assert_messages`] — read + assert on the flash-messages cookie
//!   from [`crate::messages`]. Gated on `template_views` so consumers
//!   that use messages get the helper for free.
//!
//! ## Out of scope (queued as follow-ups)
//!
//! Django's full assertion surface includes:
//! - `assertTemplateUsed` — needs a Tera render-tracking hook; would
//!   require wrapping the Tera instance with an instrumented variant.
//! - `assertNumQueries` — needs a query-counting probe on `Pool`.
//!   Doable with a wrapping executor but adds complexity.
//! - `assertFormError(response, field, message)` — needs Form errors
//!   in the rendered template context, which is template-shape
//!   specific. Right now `FormView` stamps `errors: HashMap` into
//!   context — a helper could inspect that map but only for views
//!   that use the canonical key.
//! - `assertRedirectChain` — follow multi-step redirects. Needs a
//!   test-client wrapper that re-issues requests on 3xx.
//!
//! These pair naturally with a future test-client wrapper. The
//! helpers here are the high-leverage subset that needs nothing
//! beyond `axum::Response`.

use axum::body::to_bytes;
use axum::http::header;
use axum::response::Response;

/// Maximum bytes consumed from a response body for inspection.
/// 1 MiB is far above any reasonable test payload; gives a clean
/// error if a streamed body would otherwise hang the test.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Assert the response status equals `expected`. Panics on mismatch
/// with the actual status in the message.
///
/// ```ignore
/// assert_status(&res, 200);
/// assert_status(&res, 404);
/// ```
pub fn assert_status(res: &Response, expected: u16) {
    let actual = res.status().as_u16();
    assert_eq!(
        actual, expected,
        "expected HTTP status {expected}, got {actual}"
    );
}

/// Assert the response body contains `fragment` as a UTF-8
/// substring. Consumes the body, so the response is moved in.
///
/// ```ignore
/// assert_contains(res, "Hello, world!").await;
/// ```
///
/// Status is NOT checked — error-page content assertions like
/// `assert_contains(res_404, "Not Found")` are legitimate. Pair with
/// [`assert_status`] when the status itself is part of the
/// expectation:
///
/// ```ignore
/// assert_status(&res, 200);
/// assert_contains(res, "Hello").await;
/// ```
///
/// Panics if the body exceeds 1 MiB (defensive — streamed bodies
/// would otherwise hang the test), the body isn't UTF-8, or the
/// fragment isn't found. Snippet of the actual body (truncated +
/// "more chars" indicator) is included in the panic message for fast
/// debugging.
pub async fn assert_contains(res: Response, fragment: &str) {
    let body = to_bytes(res.into_body(), MAX_BODY_BYTES)
        .await
        .unwrap_or_else(|e| panic!("assert_contains: failed to read body: {e}"));
    let body_str = std::str::from_utf8(&body)
        .unwrap_or_else(|e| panic!("assert_contains: body is not UTF-8: {e}"));
    assert!(
        body_str.contains(fragment),
        "expected body to contain `{fragment}`, got:\n{}",
        truncate(body_str, 500)
    );
}

/// Inverse of [`assert_contains`] — panics when the fragment IS
/// found. Useful for "the deleted post shouldn't appear in the list"
/// style assertions.
pub async fn assert_not_contains(res: Response, fragment: &str) {
    let body = to_bytes(res.into_body(), MAX_BODY_BYTES)
        .await
        .unwrap_or_else(|e| panic!("assert_not_contains: failed to read body: {e}"));
    let body_str = std::str::from_utf8(&body)
        .unwrap_or_else(|e| panic!("assert_not_contains: body is not UTF-8: {e}"));
    assert!(
        !body_str.contains(fragment),
        "expected body to NOT contain `{fragment}`, got:\n{}",
        truncate(body_str, 500)
    );
}

/// Assert the response is a 3xx redirect with `Location` exactly
/// equal to `target`. Catches both the status check and the URL
/// check in one assertion — Django's `assertRedirects` shape.
///
/// ```ignore
/// assert_redirects(&res, "/login?next=%2Fprofile");
/// ```
///
/// Does NOT follow the redirect or check what the target serves —
/// that's `assert_redirect_chain` territory (queued).
pub fn assert_redirects(res: &Response, target: &str) {
    let status = res.status();
    assert!(
        status.is_redirection(),
        "assert_redirects: status was {status}, expected 3xx"
    );
    let loc = res
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_else(|| panic!("assert_redirects: no Location header on {status} response"));
    assert_eq!(
        loc, target,
        "assert_redirects: expected Location `{target}`, got `{loc}`"
    );
}

/// Drain the `Set-Cookie` value(s) for the messages framework cookie
/// from `res` and assert the staged messages match `expected` —
/// list of `(level_str, body)` pairs.
///
/// `secret` is the same secret the production handler used. Pair with
/// [`crate::messages::push`] / `success` / `info` etc.
///
/// ```ignore
/// // Handler-under-test pushes a success message + redirects.
/// let res = app.oneshot(req("POST /items")).await.unwrap();
/// assert_messages(&res, SECRET, &[("success", "Item created.")]);
/// ```
///
/// Empty `expected` asserts NO messages cookie was set (or it's a
/// clear-cookie with `Max-Age=0`).
#[cfg(feature = "template_views")]
pub fn assert_messages(res: &Response, secret: &[u8], expected: &[(&str, &str)]) {
    use crate::messages::{Level, MESSAGES_COOKIE};
    use std::str::FromStr as _;

    // Walk every Set-Cookie header looking for the messages cookie.
    // Take the LAST value (multiple Set-Cookie for the same name =
    // last write wins, browser behavior).
    let mut cookie_value: Option<String> = None;
    for v in res.headers().get_all(header::SET_COOKIE).iter() {
        let Ok(s) = v.to_str() else {
            continue;
        };
        let first = s.split(';').next().unwrap_or("");
        if let Some(val) = first.trim().strip_prefix(&format!("{MESSAGES_COOKIE}=")) {
            cookie_value = Some(val.to_owned());
        }
    }

    let Some(raw) = cookie_value else {
        if expected.is_empty() {
            return;
        }
        panic!("assert_messages: no `{MESSAGES_COOKIE}` Set-Cookie header found");
    };

    // Fold the cookie body back into a Cookie request header so we
    // can reuse the messages `drain` parser. Same cookie body either
    // way (`name=value`).
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        header::COOKIE,
        axum::http::HeaderValue::from_str(&format!("{MESSAGES_COOKIE}={raw}"))
            .expect("cookie value is header-safe (just produced it)"),
    );
    let (msgs, _) = crate::messages::drain(secret, &headers);

    if expected.is_empty() {
        assert!(
            msgs.is_empty(),
            "assert_messages: expected no messages, got {msgs:?}"
        );
        return;
    }

    let actual: Vec<(String, String)> = msgs
        .iter()
        .map(|m| (m.level.as_str().to_owned(), m.body.clone()))
        .collect();
    let expected_owned: Vec<(String, String)> = expected
        .iter()
        .map(|(lvl, body)| ((*lvl).to_owned(), (*body).to_owned()))
        .collect();
    assert_eq!(
        actual, expected_owned,
        "assert_messages: messages don't match — left=actual right=expected"
    );

    // Sanity-check that every "expected" level is a real Level — catch
    // typos in test fixture data early.
    for (lvl, _) in expected {
        Level::from_str(lvl)
            .unwrap_or_else(|_| panic!("assert_messages: `{lvl}` is not a valid Level"));
    }
}

/// Truncate a string at a UTF-8 char boundary at or before `max`,
/// appending a `...(N more chars)` indicator so the panic message
/// doesn't confuse a clipped 1000-char body for a 500-char one.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut idx = max;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    let remaining = s.len() - idx;
    format!("{}...(+{remaining} more chars)", &s[..idx])
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::StatusCode;

    fn html_response(status: StatusCode, body: &str) -> Response {
        Response::builder()
            .status(status)
            .body(Body::from(body.to_owned()))
            .unwrap()
    }

    fn redirect_response(status: StatusCode, location: &str) -> Response {
        Response::builder()
            .status(status)
            .header(header::LOCATION, location)
            .body(Body::empty())
            .unwrap()
    }

    // -------- assert_status --------

    #[test]
    fn assert_status_passes_on_match() {
        let res = html_response(StatusCode::OK, "");
        assert_status(&res, 200);
    }

    #[test]
    #[should_panic(expected = "expected HTTP status 404, got 200")]
    fn assert_status_panics_on_mismatch() {
        let res = html_response(StatusCode::OK, "");
        assert_status(&res, 404);
    }

    // -------- assert_contains --------

    #[tokio::test]
    async fn assert_contains_passes_when_body_includes_fragment() {
        let res = html_response(StatusCode::OK, "Hello, world!");
        assert_contains(res, "world").await;
    }

    #[tokio::test]
    #[should_panic(expected = "expected body to contain `nope`")]
    async fn assert_contains_panics_when_fragment_missing() {
        let res = html_response(StatusCode::OK, "Hello, world!");
        assert_contains(res, "nope").await;
    }

    #[tokio::test]
    async fn assert_contains_passes_on_error_status_when_fragment_present() {
        // Status is intentionally NOT checked — `assertContains` is
        // legitimate against an error page ("does the 404 say
        // 'Not Found'?"). Pair with `assert_status` if the status
        // itself matters.
        let res = html_response(StatusCode::NOT_FOUND, "Not Found");
        assert_contains(res, "Not Found").await;
    }

    #[test]
    fn truncate_short_input_passes_through() {
        assert_eq!(truncate("hello", 500), "hello");
    }

    #[test]
    fn truncate_long_input_appends_more_chars_indicator() {
        let long = "x".repeat(1000);
        let out = truncate(&long, 500);
        assert!(out.starts_with(&"x".repeat(500)));
        assert!(out.contains("...(+500 more chars)"), "got: {out}");
    }

    #[test]
    fn truncate_clips_at_utf8_boundary_no_mid_codepoint_slice() {
        // "é" is 2 bytes in UTF-8. With max=1 we'd be trying to slice
        // mid-codepoint; truncate should back up to byte 0.
        let s = "é";
        let out = truncate(s, 1);
        // Back-up landed at 0 → 0 bytes of content + the indicator.
        assert!(out.starts_with("..."), "got: {out}");
    }

    // -------- assert_not_contains --------

    #[tokio::test]
    async fn assert_not_contains_passes_when_fragment_missing() {
        let res = html_response(StatusCode::OK, "Goodbye, sky.");
        assert_not_contains(res, "world").await;
    }

    #[tokio::test]
    #[should_panic(expected = "expected body to NOT contain `world`")]
    async fn assert_not_contains_panics_when_fragment_present() {
        let res = html_response(StatusCode::OK, "world peace");
        assert_not_contains(res, "world").await;
    }

    // -------- assert_redirects --------

    #[test]
    fn assert_redirects_passes_on_302_with_location() {
        let res = redirect_response(StatusCode::FOUND, "/login?next=%2Fprofile");
        assert_redirects(&res, "/login?next=%2Fprofile");
    }

    #[test]
    fn assert_redirects_passes_on_301() {
        let res = redirect_response(StatusCode::MOVED_PERMANENTLY, "/new-home");
        assert_redirects(&res, "/new-home");
    }

    #[test]
    #[should_panic(expected = "expected 3xx")]
    fn assert_redirects_panics_on_non_redirect_status() {
        let res = html_response(StatusCode::OK, "");
        assert_redirects(&res, "/login");
    }

    #[test]
    #[should_panic(expected = "expected Location `/wrong`")]
    fn assert_redirects_panics_on_location_mismatch() {
        let res = redirect_response(StatusCode::FOUND, "/login");
        assert_redirects(&res, "/wrong");
    }

    // -------- assert_messages --------

    #[cfg(feature = "template_views")]
    #[test]
    fn assert_messages_passes_on_staged_match() {
        use crate::messages;
        const SECRET: &[u8] = b"test-secret-32-bytes-aaaaaaaaaaaa";

        // Simulate a handler that staged a message via `success`.
        let cookie = messages::success(SECRET, &axum::http::HeaderMap::new(), "Item created.");
        let res = Response::builder()
            .status(StatusCode::SEE_OTHER)
            .header(header::SET_COOKIE, cookie)
            .body(Body::empty())
            .unwrap();

        assert_messages(&res, SECRET, &[("success", "Item created.")]);
    }

    #[cfg(feature = "template_views")]
    #[test]
    fn assert_messages_passes_on_empty_when_no_cookie_set() {
        let res = html_response(StatusCode::OK, "");
        assert_messages(&res, b"any-secret", &[]);
    }

    #[cfg(feature = "template_views")]
    #[test]
    #[should_panic(expected = "messages don't match")]
    fn assert_messages_panics_on_mismatch() {
        use crate::messages;
        const SECRET: &[u8] = b"test-secret-32-bytes-aaaaaaaaaaaa";
        let cookie = messages::success(SECRET, &axum::http::HeaderMap::new(), "Item created.");
        let res = Response::builder()
            .status(StatusCode::SEE_OTHER)
            .header(header::SET_COOKIE, cookie)
            .body(Body::empty())
            .unwrap();
        assert_messages(&res, SECRET, &[("error", "Something broke.")]);
    }
}
