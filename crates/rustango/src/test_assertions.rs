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
//! ## Implemented
//!
//! - [`assert_status`] — exact-status match against a u16.
//! - [`assert_status_in`] — status is one of an allowed set.
//! - [`assert_status_2xx`] — status is in the 200–299 range.
//! - [`assert_status_4xx`] — status is in the 400–499 range.
//! - [`assert_status_5xx`] — status is in the 500–599 range.
//! - [`assert_contains`] — response body contains a UTF-8 substring.
//! - [`assert_not_contains`] — body does NOT contain a substring.
//! - [`assert_contains_count`] — body contains fragment N times
//!   (Django's `assertContains(..., count=N)`).
//! - [`assert_redirects`] — 3xx status + `Location` header equality.
//! - [`assert_header`] — exact header value match.
//! - [`assert_content_type`] — sugar for `assert_header("content-type", ...)`.
//! - [`assert_json_eq`] — body parses as JSON and equals expected
//!   (Django's `assertJSONEqual`).
//! - [`assert_json_not_eq`] — body parses as JSON and DIFFERS from
//!   expected (Django's `assertJSONNotEqual`).
//! - [`assert_redirect_chain`] — inspect the chain produced by
//!   [`crate::test_client::TestClient::get_following_redirects`] and
//!   assert it ends at a given (path, status). Django's
//!   `assertRedirects(..., fetch_redirect_response=True)`.
//! - [`assert_messages`] — read + assert on the flash-messages cookie
//!   from [`crate::messages`]. Gated on `template_views` so consumers
//!   that use messages get the helper for free.
//! - [`assert_cookie_set`] — assert a `Set-Cookie` header for the
//!   given cookie name was emitted, with optional exact-value match.
//! - [`assert_cookie_not_set`] — inverse: assert no `Set-Cookie` for
//!   the given name was emitted.
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
//!
//! The helpers here are the high-leverage subset that needs nothing
//! beyond `axum::Response` plus the existing test_client redirect
//! follower.

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

/// Assert the response status is one of `allowed`. Useful when a
/// handler can legitimately return either of several success
/// codes — e.g. `POST /items` might return 200 (idempotent
/// touch) OR 201 (new resource).
///
/// ```ignore
/// assert_status_in(&res, &[200, 201]);
/// assert_status_in(&res, &[301, 302, 307, 308]);
/// ```
pub fn assert_status_in(res: &Response, allowed: &[u16]) {
    let actual = res.status().as_u16();
    assert!(
        allowed.contains(&actual),
        "expected HTTP status to be one of {allowed:?}, got {actual}"
    );
}

/// Assert the response status is in the 2xx range (success).
/// Sugar for the very common "any success" check.
pub fn assert_status_2xx(res: &Response) {
    let actual = res.status().as_u16();
    assert!(
        (200..300).contains(&actual),
        "expected a 2xx status, got {actual}"
    );
}

/// Assert the response status is in the 4xx range (client error).
pub fn assert_status_4xx(res: &Response) {
    let actual = res.status().as_u16();
    assert!(
        (400..500).contains(&actual),
        "expected a 4xx status, got {actual}"
    );
}

/// Assert the response status is in the 5xx range (server error).
/// Useful for negative tests of error pages / handlers that must
/// surface internal failures rather than degrade silently.
pub fn assert_status_5xx(res: &Response) {
    let actual = res.status().as_u16();
    assert!(
        (500..600).contains(&actual),
        "expected a 5xx status, got {actual}"
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
/// pair with [`assert_redirect_chain`] for that.
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

/// Assert that response header `name` equals `value` exactly.
/// Matches the header name case-insensitively (per RFC 7230), and
/// the value byte-for-byte after UTF-8 decode.
///
/// ```ignore
/// assert_header(&res, "content-type", "application/json");
/// assert_header(&res, "x-request-id", "abc-123");
/// ```
///
/// Panics if the header is missing or carries a different value.
/// Multiple headers with the same name match the FIRST occurrence
/// (axum's default header API surfaces them in order).
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

/// Assert the response `Content-Type` header equals `expected`.
/// Sugar for [`assert_header`] with `name = "content-type"`.
///
/// ```ignore
/// assert_content_type(&res, "application/json");
/// assert_content_type(&res, "text/html; charset=utf-8");
/// ```
pub fn assert_content_type(res: &Response, expected: &str) {
    assert_header(res, "content-type", expected);
}

/// Assert the response body, parsed as JSON, equals `expected`.
/// Django's `assertJSONEqual`. Order-insensitive for object keys
/// (JSON Value equality is structural).
///
/// ```ignore
/// use serde_json::json;
/// assert_json_eq(res, &json!({"id": 1, "name": "Alice"})).await;
/// ```
///
/// Panics with both bodies in the message when:
/// - the response body isn't valid JSON;
/// - the parsed body doesn't structurally equal `expected`.
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
        // Pretty-print both sides so the diff is readable in the
        // panic message — JSON ordering noise dominates otherwise.
        let actual_pp = serde_json::to_string_pretty(&actual).unwrap_or_default();
        let expected_pp = serde_json::to_string_pretty(expected).unwrap_or_default();
        panic!("assert_json_eq mismatch.\nexpected:\n{expected_pp}\nactual:\n{actual_pp}");
    }
}

/// Inverse of [`assert_json_eq`] — Django's `assertJSONNotEqual`.
/// Asserts the parsed JSON body **differs** from `unexpected`.
///
/// Useful for negative regression tests: "this endpoint no longer
/// returns the leaky old shape." Same body-decode safety as the
/// positive form (panics on non-JSON with a 500-char snippet).
///
/// ```ignore
/// assert_json_not_eq(res, &serde_json::json!({"password": "leaked"})).await;
/// ```
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

/// Assert the redirect chain produced by
/// [`crate::test_client::TestClient::get_following_redirects`] ends at
/// `final_path` with `final_status`. Django's `assertRedirects` with
/// `fetch_redirect_response=True`.
///
/// The chain is a `Vec<(u16, String)>` where each entry is the
/// (status, location) of one hop, and the final entry is the
/// (status, resolved_path) of the last response. This helper inspects
/// only the final entry.
///
/// ```ignore
/// let (_res, chain) = client.get_following_redirects("/old", 5).await;
/// assert_redirect_chain(&chain, "/new-home", 200);
/// ```
///
/// Panics with the full chain in the message when the final hop's
/// status or path doesn't match — so a misconfigured chain is easy
/// to diagnose.
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

/// Assert the response body contains `fragment` exactly `count`
/// times. Django's `assertContains(..., count=N)`. `count = 0`
/// asserts absence — same as [`assert_not_contains`].
///
/// ```ignore
/// // Three article cards on the index.
/// assert_contains_count(res, "<article class=\"card\">", 3).await;
/// ```
///
/// Panics with the actual count and a 500-char body snippet when
/// the counts don't match.
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

/// Assert that the response emitted a `Set-Cookie` header for the
/// given cookie `name`. If `expected_value` is `Some`, also assert
/// the cookie's value (the portion BEFORE the first `;` — i.e. the
/// `name=value` segment without `Path` / `HttpOnly` / etc.) equals
/// that value byte-for-byte. Returns the matched `Set-Cookie` header
/// value(s).
///
/// Use to verify that handlers set session / CSRF / messages /
/// custom cookies as expected.
///
/// ```ignore
/// assert_cookie_set(&res, "rustango_messages", None);  // present, value not pinned
/// assert_cookie_set(&res, "session", Some("abc123"));  // present with exact value
/// ```
///
/// Panics with the full Set-Cookie list when no header for that
/// name is found, or when the value doesn't match.
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

/// Inverse of [`assert_cookie_set`] — panic if a `Set-Cookie` for
/// `name` IS present. Use to verify that a handler did NOT set a
/// cookie under specific conditions (logged-out request shouldn't
/// touch the session cookie, etc.).
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
        // Key order doesn't matter — serde_json::Value equality
        // is structural.
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
        // Key order doesn't matter — structural equality strikes.
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
        // Chain shape mirrors what TestClient::get_following_redirects
        // produces: (status, path) per hop, last entry is the final
        // landing response.
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
        // Multiple Set-Cookie headers can appear in one response;
        // the helper should match against any of them.
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
