//! Assertion helpers for axum responses.
//!
//! The quick checks a view test reaches for, written against axum's
//! `Response`:
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
//! Every helper panics on a mismatch and prints the actual value.
//! None return `Result`, so tests stay free of `?`.
//!
//! ## What is here
//!
//! * Status: [`assert_status`], [`assert_status_in`],
//!   [`assert_status_2xx`], [`assert_status_4xx`],
//!   [`assert_status_5xx`].
//! * Body: [`assert_contains`], [`assert_not_contains`],
//!   [`assert_contains_count`], [`assert_json_eq`],
//!   [`assert_json_not_eq`].
//! * Redirects: [`assert_redirects`], [`assert_redirect_chain`].
//! * Headers: [`assert_header`], [`assert_content_type`].
//! * Cookies: [`assert_cookie_set`], [`assert_cookie_not_set`].
//! * Flash messages: [`assert_messages`], gated on `template_views`.
//! * Query counts: `assert_num_queries`, from [`query_counter`].
//!
//! Two checks are still missing: "this template was used", which
//! needs a render hook inside Tera, and "this form field errored",
//! which needs the form errors from the rendered template context.
//!
//! [`assert_status`]: crate::test_assertions::assert_status
//! [`assert_status_in`]: crate::test_assertions::assert_status_in
//! [`assert_status_2xx`]: crate::test_assertions::assert_status_2xx
//! [`assert_status_4xx`]: crate::test_assertions::assert_status_4xx
//! [`assert_status_5xx`]: crate::test_assertions::assert_status_5xx
//! [`assert_contains`]: crate::test_assertions::assert_contains
//! [`assert_not_contains`]: crate::test_assertions::assert_not_contains
//! [`assert_contains_count`]: crate::test_assertions::assert_contains_count
//! [`assert_json_eq`]: crate::test_assertions::assert_json_eq
//! [`assert_json_not_eq`]: crate::test_assertions::assert_json_not_eq
//! [`assert_redirects`]: crate::test_assertions::assert_redirects
//! [`assert_redirect_chain`]: crate::test_assertions::assert_redirect_chain
//! [`assert_header`]: crate::test_assertions::assert_header
//! [`assert_content_type`]: crate::test_assertions::assert_content_type
//! [`assert_cookie_set`]: crate::test_assertions::assert_cookie_set
//! [`assert_cookie_not_set`]: crate::test_assertions::assert_cookie_not_set
//! [`assert_messages`]: crate::test_assertions::assert_messages
//! [`query_counter`]: crate::test_assertions::query_counter

// Every assertion below takes an `axum::Response`, so the HTTP half of this
// module gates on `_axum`. `query_counter` does not: `sql::executor` bumps it
// on every query, so it must work in a bare-ORM build with no axum.
#[cfg(feature = "_axum")]
use axum::body::to_bytes;
#[cfg(feature = "_axum")]
use axum::http::header;
#[cfg(feature = "_axum")]
use axum::response::Response;

pub mod query_counter;
pub use query_counter::{assert_num_queries, QueryCounter};

/// Most bytes read from a response body. 1 MiB is far above any test
/// payload, and it turns a streamed body into a clear error instead
/// of a hung test.
#[cfg(feature = "_axum")]
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Assert the response status equals `expected`.
///
/// ```ignore
/// assert_status(&res, 200);
/// assert_status(&res, 404);
/// ```
#[cfg(feature = "_axum")]
pub fn assert_status(res: &Response, expected: u16) {
    let actual = res.status().as_u16();
    assert_eq!(
        actual, expected,
        "expected HTTP status {expected}, got {actual}"
    );
}

/// Assert the response status is one of `allowed`. Use it when a
/// handler may answer with more than one valid code, such as 200 or
/// 201 from `POST /items`.
///
/// ```ignore
/// assert_status_in(&res, &[200, 201]);
/// assert_status_in(&res, &[301, 302, 307, 308]);
/// ```
#[cfg(feature = "_axum")]
pub fn assert_status_in(res: &Response, allowed: &[u16]) {
    let actual = res.status().as_u16();
    assert!(
        allowed.contains(&actual),
        "expected HTTP status to be one of {allowed:?}, got {actual}"
    );
}

/// Assert the status is 2xx (any success).
#[cfg(feature = "_axum")]
pub fn assert_status_2xx(res: &Response) {
    let actual = res.status().as_u16();
    assert!(
        (200..300).contains(&actual),
        "expected a 2xx status, got {actual}"
    );
}

/// Assert the status is 4xx (client error).
#[cfg(feature = "_axum")]
pub fn assert_status_4xx(res: &Response) {
    let actual = res.status().as_u16();
    assert!(
        (400..500).contains(&actual),
        "expected a 4xx status, got {actual}"
    );
}

/// Assert the status is 5xx (server error). Use it to check that a
/// handler reports an internal failure instead of hiding it.
#[cfg(feature = "_axum")]
pub fn assert_status_5xx(res: &Response) {
    let actual = res.status().as_u16();
    assert!(
        (500..600).contains(&actual),
        "expected a 5xx status, got {actual}"
    );
}

/// Assert the body contains `fragment`. Reads the body, so the
/// response is moved in.
///
/// ```ignore
/// assert_contains(res, "Hello, world!").await;
/// ```
///
/// The status is not checked, because checking an error page's text
/// is valid. Add [`assert_status`] when the status matters too:
///
/// ```ignore
/// assert_status(&res, 200);
/// assert_contains(res, "Hello").await;
/// ```
///
/// Panics if the body is over 1 MiB, is not UTF-8, or lacks the
/// fragment. The message carries a snippet of the body.
#[cfg(feature = "_axum")]
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

/// Opposite of [`assert_contains`]: panics when the fragment IS
/// found, e.g. a deleted post that must not show up in a list.
#[cfg(feature = "_axum")]
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

/// Assert the response is a 3xx redirect whose `Location` equals
/// `target`.
///
/// ```ignore
/// assert_redirects(&res, "/login?next=%2Fprofile");
/// ```
///
/// It does not follow the redirect. Use [`assert_redirect_chain`]
/// for that.
#[cfg(feature = "_axum")]
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

/// Read the flash-messages cookie out of `res` and assert the staged
/// messages match `expected`, a list of `(level, body)` pairs.
///
/// `secret` must be the one the handler signed with. See
/// [`crate::messages::push`] and friends.
///
/// ```ignore
/// // Handler-under-test pushes a success message + redirects.
/// let res = app.oneshot(req("POST /items")).await.unwrap();
/// assert_messages(&res, SECRET, &[("success", "Item created.")]);
/// ```
///
/// An empty `expected` asserts that no messages were set.
// `_signing` as well as `template_views`: this reads the HMAC-signed
// messages cookie, and `crate::messages` gates on the signing feature.
#[cfg(all(feature = "template_views", feature = "_signing", feature = "_axum"))]
pub fn assert_messages(res: &Response, secret: &[u8], expected: &[(&str, &str)]) {
    use crate::messages::{Level, MESSAGES_COOKIE};
    use std::str::FromStr as _;

    // Scan every Set-Cookie for the messages cookie and keep the
    // LAST one: browsers let the last write for a name win.
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

    // Put the cookie back into a Cookie request header so we can
    // reuse the `drain` parser; the body is `name=value` either way.
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

    // Check every expected level is a real Level, to catch typos in
    // the test data.
    for (lvl, _) in expected {
        Level::from_str(lvl)
            .unwrap_or_else(|_| panic!("assert_messages: `{lvl}` is not a valid Level"));
    }
}

/// Assert header `name` equals `value`. The name matches without
/// regard to case; the value must match exactly.
///
/// ```ignore
/// assert_header(&res, "content-type", "application/json");
/// assert_header(&res, "x-request-id", "abc-123");
/// ```
///
/// Panics if the header is missing or holds another value. With
/// repeated headers of that name, only the first is checked.
#[cfg(feature = "_axum")]
pub fn assert_header(res: &Response, name: &str, value: &str) {
    let actual = res
        .headers()
        .get(name)
        .map(|v| v.to_str().unwrap_or("<non-utf8>").to_owned());
    match actual {
        None => panic!("expected header `{name}: {value}`, but header was missing"),
        Some(actual) if actual == value => {}
        Some(actual) => panic!("expected header `{name}: {value}`, got `{name}: {actual}`",),
    }
}

/// [`assert_header`] for `content-type`.
///
/// ```ignore
/// assert_content_type(&res, "application/json");
/// assert_content_type(&res, "text/html; charset=utf-8");
/// ```
#[cfg(feature = "_axum")]
pub fn assert_content_type(res: &Response, expected: &str) {
    assert_header(res, "content-type", expected);
}

/// Assert the body, parsed as JSON, equals `expected`. Key order
/// does not matter.
///
/// ```ignore
/// use serde_json::json;
/// assert_json_eq(res, &json!({"id": 1, "name": "Alice"})).await;
/// ```
///
/// Panics, showing both sides, if the body is not valid JSON or does
/// not equal `expected`.
#[cfg(feature = "_axum")]
pub async fn assert_json_eq(res: Response, expected: &serde_json::Value) {
    let bytes = to_bytes(res.into_body(), MAX_BODY_BYTES)
        .await
        .expect("read response body");
    let actual: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => panic!(
            "assert_json_eq: body is not valid JSON ({e}). Raw body:\n{}",
            truncate(&String::from_utf8_lossy(&bytes), 500),
        ),
    };
    if &actual != expected {
        // Pretty-print both sides so the panic message is readable.
        let actual_pp = serde_json::to_string_pretty(&actual).unwrap_or_default();
        let expected_pp = serde_json::to_string_pretty(expected).unwrap_or_default();
        panic!("assert_json_eq mismatch.\nexpected:\n{expected_pp}\nactual:\n{actual_pp}");
    }
}

/// Opposite of [`assert_json_eq`]: the parsed body must **differ**
/// from `unexpected`.
///
/// Good for a regression test that an endpoint no longer returns an
/// old, leaky shape.
///
/// ```ignore
/// assert_json_not_eq(res, &serde_json::json!({"password": "leaked"})).await;
/// ```
#[cfg(feature = "_axum")]
pub async fn assert_json_not_eq(res: Response, unexpected: &serde_json::Value) {
    let bytes = to_bytes(res.into_body(), MAX_BODY_BYTES)
        .await
        .expect("read response body");
    let actual: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => panic!(
            "assert_json_not_eq: body is not valid JSON ({e}). Raw body:\n{}",
            truncate(&String::from_utf8_lossy(&bytes), 500),
        ),
    };
    if &actual == unexpected {
        let actual_pp = serde_json::to_string_pretty(&actual).unwrap_or_default();
        panic!("assert_json_not_eq: body equals the unexpected value:\n{actual_pp}",);
    }
}

/// Assert the chain from
/// [`crate::test_client::TestClient::get_following_redirects`] ends
/// at `final_path` with `final_status`.
///
/// Each entry is the `(status, location)` of one hop, and the last
/// one is the final response. Only that last entry is checked.
///
/// ```ignore
/// let (_res, chain) = client.get_following_redirects("/old", 5).await;
/// assert_redirect_chain(&chain, "/new-home", 200);
/// ```
///
/// A mismatch panics and prints the whole chain.
#[cfg(feature = "_axum")]
pub fn assert_redirect_chain(chain: &[(u16, String)], final_path: &str, final_status: u16) {
    let last = chain
        .last()
        .unwrap_or_else(|| panic!("assert_redirect_chain: chain is empty"));
    if last.0 != final_status || last.1 != final_path {
        let pretty = chain
            .iter()
            .enumerate()
            .map(|(i, (s, p))| format!("  {i}: {s} {p}"))
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "assert_redirect_chain: expected final hop to be `{final_status} {final_path}`, got `{} {}`.\nFull chain:\n{pretty}",
            last.0, last.1,
        );
    }
}

/// Assert the body contains `fragment` exactly `count` times.
/// `count = 0` means absent, like [`assert_not_contains`].
///
/// ```ignore
/// // Three article cards on the index.
/// assert_contains_count(res, "<article class=\"card\">", 3).await;
/// ```
///
/// A mismatch panics with the real count and a body snippet.
#[cfg(feature = "_axum")]
pub async fn assert_contains_count(res: Response, fragment: &str, count: usize) {
    let bytes = to_bytes(res.into_body(), MAX_BODY_BYTES)
        .await
        .expect("read response body");
    let body = String::from_utf8_lossy(&bytes);
    let actual = body.matches(fragment).count();
    assert_eq!(
        actual, count,
        "expected `{fragment}` to appear {count} times, found {actual}.\nBody (first 500 chars):\n{}",
        truncate(&body, 500),
    );
}

/// Assert the response set a cookie called `name`. With
/// `expected_value = Some(v)`, the value must equal `v` exactly. The
/// value is the part before the first `;`, so `Path` and `HttpOnly`
/// are not part of the comparison.
///
/// Use it to check session, CSRF, messages or custom cookies.
///
/// ```ignore
/// assert_cookie_set(&res, "rustango_messages", None);  // present, value not pinned
/// assert_cookie_set(&res, "session", Some("abc123"));  // present with exact value
/// ```
///
/// Panics with every `Set-Cookie` header when the name is missing or
/// the value differs.
#[cfg(feature = "_axum")]
pub fn assert_cookie_set(res: &Response, name: &str, expected_value: Option<&str>) {
    let mut matches: Vec<String> = Vec::new();
    for v in res.headers().get_all(axum::http::header::SET_COOKIE).iter() {
        let Ok(s) = v.to_str() else { continue };
        let first = s.split(';').next().unwrap_or("");
        if let Some(val) = first.trim().strip_prefix(&format!("{name}=")) {
            matches.push(val.to_owned());
        }
    }
    if matches.is_empty() {
        let all_cookies: Vec<String> = res
            .headers()
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok().map(str::to_owned))
            .collect();
        panic!(
            "assert_cookie_set: no `Set-Cookie` for `{name}` found. \
             Found {} Set-Cookie header(s): {all_cookies:?}",
            all_cookies.len()
        );
    }
    if let Some(expected) = expected_value {
        let any_match = matches.iter().any(|v| v == expected);
        assert!(
            any_match,
            "assert_cookie_set: `{name}` was set, but its value didn't match. \
             Expected `{expected}`, got: {matches:?}"
        );
    }
}

/// Opposite of [`assert_cookie_set`]: panics if a cookie called
/// `name` was set. Use it to check a handler left a cookie alone,
/// e.g. a logged-out request must not touch the session cookie.
#[cfg(feature = "_axum")]
pub fn assert_cookie_not_set(res: &Response, name: &str) {
    for v in res.headers().get_all(axum::http::header::SET_COOKIE).iter() {
        let Ok(s) = v.to_str() else { continue };
        let first = s.split(';').next().unwrap_or("");
        if first.trim().starts_with(&format!("{name}=")) {
            panic!(
                "assert_cookie_not_set: `Set-Cookie: {name}=...` was unexpectedly emitted: `{s}`"
            );
        }
    }
}

/// Cut a string at a UTF-8 boundary at or before `max` and append
/// `...(+N more chars)`, so a clipped body cannot be mistaken for a
/// short one. Gated with the body assertions that call it.
#[cfg(feature = "_axum")]
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

// Every case builds an `axum::Response`, so the suite carries the same gate
// as the assertions it exercises. `query_counter` has its own tests.
#[cfg(all(test, feature = "_axum"))]
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
        // The status is not checked on purpose: asking whether the
        // 404 page says "Not Found" is a valid test.
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
        // "é" is 2 bytes, so max=1 would slice mid-codepoint.
        // truncate must back up to byte 0.
        let s = "é";
        let out = truncate(s, 1);
        // At byte 0 there is no content, only the indicator.
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

    // These call `assert_messages`, which reads the HMAC-signed messages
    // cookie, so they need `_signing` as well as `template_views`.
    #[cfg(all(feature = "template_views", feature = "_signing"))]
    #[test]
    fn assert_messages_passes_on_staged_match() {
        use crate::messages;
        const SECRET: &[u8] = b"test-secret-32-bytes-aaaaaaaaaaaa";

        // Stand in for a handler that staged a message via `success`.
        let cookie = messages::success(SECRET, &axum::http::HeaderMap::new(), "Item created.");
        let res = Response::builder()
            .status(StatusCode::SEE_OTHER)
            .header(header::SET_COOKIE, cookie)
            .body(Body::empty())
            .unwrap();

        assert_messages(&res, SECRET, &[("success", "Item created.")]);
    }

    #[cfg(all(feature = "template_views", feature = "_signing"))]
    #[test]
    fn assert_messages_passes_on_empty_when_no_cookie_set() {
        let res = html_response(StatusCode::OK, "");
        assert_messages(&res, b"any-secret", &[]);
    }

    #[cfg(all(feature = "template_views", feature = "_signing"))]
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

    // -------- assert_header --------

    fn header_response(name: &'static str, value: &'static str) -> Response {
        Response::builder()
            .status(StatusCode::OK)
            .header(name, value)
            .body(Body::empty())
            .unwrap()
    }

    #[test]
    fn assert_header_passes_on_exact_match() {
        let res = header_response("X-Request-Id", "abc-123");
        assert_header(&res, "x-request-id", "abc-123");
        // Header name is case-insensitive.
        assert_header(&res, "X-REQUEST-ID", "abc-123");
    }

    #[test]
    #[should_panic(expected = "header was missing")]
    fn assert_header_panics_when_missing() {
        let res = html_response(StatusCode::OK, "");
        assert_header(&res, "x-not-set", "anything");
    }

    #[test]
    #[should_panic(expected = "expected header")]
    fn assert_header_panics_on_value_mismatch() {
        let res = header_response("x-tag", "actual");
        assert_header(&res, "x-tag", "expected");
    }

    // -------- assert_content_type --------

    #[test]
    fn assert_content_type_passes() {
        let res = header_response("content-type", "application/json");
        assert_content_type(&res, "application/json");
    }

    #[test]
    #[should_panic(expected = "expected header `content-type:")]
    fn assert_content_type_panics_on_mismatch() {
        let res = header_response("content-type", "text/html; charset=utf-8");
        assert_content_type(&res, "application/json");
    }

    // -------- assert_json_eq --------

    #[tokio::test]
    async fn assert_json_eq_passes_on_structural_match() {
        let res = Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id": 1, "name": "Alice"}"#))
            .unwrap();
        // Key order does not matter: Value equality is structural.
        assert_json_eq(res, &serde_json::json!({"name": "Alice", "id": 1})).await;
    }

    #[tokio::test]
    #[should_panic(expected = "assert_json_eq mismatch")]
    async fn assert_json_eq_panics_on_value_mismatch() {
        let res = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(r#"{"id": 1}"#))
            .unwrap();
        assert_json_eq(res, &serde_json::json!({"id": 2})).await;
    }

    #[tokio::test]
    #[should_panic(expected = "body is not valid JSON")]
    async fn assert_json_eq_panics_on_malformed_body() {
        let res = html_response(StatusCode::OK, "<html>not json</html>");
        assert_json_eq(res, &serde_json::json!({})).await;
    }

    // -------- assert_contains_count --------

    #[tokio::test]
    async fn assert_contains_count_passes_on_exact_count() {
        let body = "<li>a</li><li>b</li><li>c</li>";
        let res = html_response(StatusCode::OK, body);
        assert_contains_count(res, "<li>", 3).await;
    }

    #[tokio::test]
    async fn assert_contains_count_zero_means_absent() {
        let res = html_response(StatusCode::OK, "no nope");
        assert_contains_count(res, "yes", 0).await;
    }

    #[tokio::test]
    #[should_panic(expected = "expected `<li>` to appear 5 times, found 3")]
    async fn assert_contains_count_panics_on_wrong_count() {
        let res = html_response(StatusCode::OK, "<li>a</li><li>b</li><li>c</li>");
        assert_contains_count(res, "<li>", 5).await;
    }

    // -------- assert_json_not_eq --------

    #[tokio::test]
    async fn assert_json_not_eq_passes_when_values_differ() {
        let res = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(r#"{"id": 1}"#))
            .unwrap();
        assert_json_not_eq(res, &serde_json::json!({"id": 2})).await;
    }

    #[tokio::test]
    #[should_panic(expected = "body equals the unexpected value")]
    async fn assert_json_not_eq_panics_on_structural_match() {
        let res = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(r#"{"id": 1, "name": "Alice"}"#))
            .unwrap();
        // Key order does not matter, so these are equal.
        assert_json_not_eq(res, &serde_json::json!({"name": "Alice", "id": 1})).await;
    }

    #[tokio::test]
    #[should_panic(expected = "body is not valid JSON")]
    async fn assert_json_not_eq_panics_on_malformed_body() {
        let res = html_response(StatusCode::OK, "<html>not json</html>");
        assert_json_not_eq(res, &serde_json::json!({})).await;
    }

    // -------- assert_redirect_chain --------

    #[test]
    fn assert_redirect_chain_passes_on_matching_final_hop() {
        // Same shape as TestClient::get_following_redirects returns:
        // (status, path) per hop, last entry is where it landed.
        let chain = vec![
            (302u16, "/old".to_owned()),
            (302, "/intermediate".to_owned()),
            (200, "/canonical".to_owned()),
        ];
        assert_redirect_chain(&chain, "/canonical", 200);
    }

    #[test]
    #[should_panic(expected = "chain is empty")]
    fn assert_redirect_chain_panics_on_empty_chain() {
        assert_redirect_chain(&[], "/anywhere", 200);
    }

    #[test]
    #[should_panic(expected = "expected final hop to be `200 /canonical`")]
    fn assert_redirect_chain_panics_on_wrong_final_path() {
        let chain = vec![(302u16, "/old".to_owned()), (200, "/elsewhere".to_owned())];
        assert_redirect_chain(&chain, "/canonical", 200);
    }

    #[test]
    #[should_panic(expected = "expected final hop to be `200 /canonical`")]
    fn assert_redirect_chain_panics_on_wrong_final_status() {
        let chain = vec![(302u16, "/old".to_owned()), (404, "/canonical".to_owned())];
        assert_redirect_chain(&chain, "/canonical", 200);
    }

    // -------- assert_cookie_set / assert_cookie_not_set --------

    fn cookie_response(set_cookies: &[&str]) -> Response {
        let mut builder = Response::builder().status(StatusCode::OK);
        for c in set_cookies {
            builder = builder.header(axum::http::header::SET_COOKIE, *c);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[test]
    fn assert_cookie_set_passes_when_cookie_present() {
        let res = cookie_response(&["session=abc123; Path=/; HttpOnly"]);
        assert_cookie_set(&res, "session", None);
    }

    #[test]
    fn assert_cookie_set_passes_with_exact_value_match() {
        let res = cookie_response(&["session=abc123; Path=/; HttpOnly"]);
        assert_cookie_set(&res, "session", Some("abc123"));
    }

    #[test]
    #[should_panic(expected = "no `Set-Cookie` for `session` found")]
    fn assert_cookie_set_panics_when_cookie_absent() {
        let res = cookie_response(&["other=value"]);
        assert_cookie_set(&res, "session", None);
    }

    #[test]
    #[should_panic(expected = "value didn't match")]
    fn assert_cookie_set_panics_on_value_mismatch() {
        let res = cookie_response(&["session=abc; Path=/"]);
        assert_cookie_set(&res, "session", Some("xyz"));
    }

    #[test]
    fn assert_cookie_set_handles_multiple_set_cookie_headers() {
        // One response can carry several Set-Cookie headers, and the
        // helper must match any of them.
        let res = cookie_response(&["csrftoken=tok; Path=/", "session=abc; Path=/; HttpOnly"]);
        assert_cookie_set(&res, "csrftoken", Some("tok"));
        assert_cookie_set(&res, "session", Some("abc"));
    }

    #[test]
    fn assert_cookie_not_set_passes_when_cookie_absent() {
        let res = cookie_response(&["other=value"]);
        assert_cookie_not_set(&res, "session");
    }

    #[test]
    fn assert_cookie_not_set_passes_when_no_cookies_at_all() {
        let res = cookie_response(&[]);
        assert_cookie_not_set(&res, "session");
    }

    #[test]
    #[should_panic(expected = "unexpectedly emitted")]
    fn assert_cookie_not_set_panics_when_cookie_present() {
        let res = cookie_response(&["session=abc; Path=/"]);
        assert_cookie_not_set(&res, "session");
    }

    // -------- assert_status_in / _2xx / _4xx / _5xx --------

    #[test]
    fn assert_status_in_passes_on_allowed_match() {
        let res = html_response(StatusCode::CREATED, "");
        assert_status_in(&res, &[200, 201, 202]);
    }

    #[test]
    #[should_panic(expected = "expected HTTP status to be one of")]
    fn assert_status_in_panics_on_mismatch() {
        let res = html_response(StatusCode::OK, "");
        assert_status_in(&res, &[201, 202]);
    }

    #[test]
    fn assert_status_2xx_accepts_range() {
        for code in [200, 201, 202, 204, 299] {
            let res = html_response(StatusCode::from_u16(code).unwrap(), "");
            assert_status_2xx(&res);
        }
    }

    #[test]
    #[should_panic(expected = "expected a 2xx status, got 301")]
    fn assert_status_2xx_panics_on_redirect() {
        let res = html_response(StatusCode::MOVED_PERMANENTLY, "");
        assert_status_2xx(&res);
    }

    #[test]
    fn assert_status_4xx_accepts_range() {
        for code in [400, 401, 403, 404, 422, 499] {
            let res = html_response(StatusCode::from_u16(code).unwrap(), "");
            assert_status_4xx(&res);
        }
    }

    #[test]
    #[should_panic(expected = "expected a 4xx status, got 200")]
    fn assert_status_4xx_panics_on_success() {
        let res = html_response(StatusCode::OK, "");
        assert_status_4xx(&res);
    }

    #[test]
    fn assert_status_5xx_accepts_range() {
        for code in [500, 502, 503, 504, 599] {
            let res = html_response(StatusCode::from_u16(code).unwrap(), "");
            assert_status_5xx(&res);
        }
    }

    #[test]
    #[should_panic(expected = "expected a 5xx status, got 400")]
    fn assert_status_5xx_panics_on_4xx() {
        let res = html_response(StatusCode::BAD_REQUEST, "");
        assert_status_5xx(&res);
    }
}
