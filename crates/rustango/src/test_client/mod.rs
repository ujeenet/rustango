//! Test client — fire HTTP requests against an `axum::Router` in tests
//! without binding a real socket.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::test_client::TestClient;
//! use axum::{Router, routing::get};
//!
//! #[tokio::test]
//! async fn hello_endpoint_returns_200() {
//!     let app = Router::new().route("/hello", get(|| async { "hi" }));
//!     let client = TestClient::new(app);
//!
//!     let res = client.get("/hello").send().await;
//!     assert_eq!(res.status, 200);
//!     assert_eq!(res.text(), "hi");
//! }
//! ```
//!
//! ## JSON requests
//!
//! ```ignore
//! let res = client
//!     .post("/api/users")
//!     .json(&serde_json::json!({"name": "Alice"}))
//!     .send()
//!     .await;
//! assert_eq!(res.status, 201);
//! let body: serde_json::Value = res.json();
//! assert_eq!(body["id"], 1);
//! ```
//!
//! ## Headers + cookies
//!
//! ```ignore
//! let res = client
//!     .get("/api/me")
//!     .header("authorization", "Bearer eyJ...")
//!     .send()
//!     .await;
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::{to_bytes, Body};
use axum::http::{HeaderName, HeaderValue, Method, Request, StatusCode};
use axum::Router;
use tower::ServiceExt;

/// Test client wrapping an `axum::Router`.
///
/// Each request runs through the full router stack (middleware + handler)
/// in-process — no network, no real socket. Each call to `.send()` consumes
/// a clone of the router so the client itself is reusable across tests.
///
/// ## Cookie persistence
///
/// The client carries a cookie jar shared between requests — Django's
/// `Client` behaviour. Every `Set-Cookie` response header is parsed
/// (name+value only; `Path`/`Domain`/`Max-Age`/etc are ignored, which
/// is fine for tests against a single in-process router) and merged
/// into the jar. Every subsequent request automatically sends the
/// jar back as a single `Cookie:` header so multi-step auth flows
/// (`POST /login` then `GET /me`) just work. Issue #41.
#[derive(Clone)]
pub struct TestClient {
    router: Router,
    cookies: Arc<Mutex<HashMap<String, String>>>,
}

impl TestClient {
    /// Wrap a router for testing.
    #[must_use]
    pub fn new(router: Router) -> Self {
        Self {
            router,
            cookies: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Snapshot of the current cookie jar (name → value). Useful in
    /// tests for asserting that a session cookie was issued.
    #[must_use]
    pub fn cookies(&self) -> HashMap<String, String> {
        self.cookies.lock().expect("cookie jar poisoned").clone()
    }

    /// Read one cookie by name. Returns `None` if absent.
    #[must_use]
    pub fn cookie(&self, name: &str) -> Option<String> {
        self.cookies
            .lock()
            .expect("cookie jar poisoned")
            .get(name)
            .cloned()
    }

    /// Inject a cookie directly into the jar without going through a
    /// `Set-Cookie` round trip. Handy for tests that pre-seed an
    /// auth cookie minted by a session backend.
    pub fn set_cookie(&self, name: impl Into<String>, value: impl Into<String>) {
        self.cookies
            .lock()
            .expect("cookie jar poisoned")
            .insert(name.into(), value.into());
    }

    /// Drop every cookie. Equivalent to Django's `Client.cookies.clear()`.
    pub fn clear_cookies(&self) {
        self.cookies.lock().expect("cookie jar poisoned").clear();
    }

    /// Convenience: POST a form to `path` and return the response.
    /// The client carries a cookie jar so any session cookie set by
    /// the login handler is automatically attached to subsequent
    /// requests. Issue #41 — partial Django `Client.login` parity.
    pub async fn login(&self, path: impl Into<String>, fields: &[(&str, &str)]) -> TestResponse {
        self.post(path).form(fields).send().await
    }

    /// Convenience: clear the cookie jar (so the next request looks
    /// fully logged-out) and optionally hit `path` (a logout endpoint
    /// that may itself emit a `Set-Cookie: name=; Max-Age=0`
    /// expiration). Pass `None` to only drop the jar locally. Issue
    /// #41 — partial Django `Client.logout` parity.
    pub async fn logout(&self, path: Option<&str>) -> Option<TestResponse> {
        self.clear_cookies();
        match path {
            Some(p) => Some(self.post(p.to_owned()).send().await),
            None => None,
        }
    }

    /// Build a `GET` request to `path`.
    #[must_use]
    pub fn get(&self, path: impl Into<String>) -> RequestBuilder<'_> {
        self.request(Method::GET, path)
    }

    /// Build a `POST` request to `path`.
    #[must_use]
    pub fn post(&self, path: impl Into<String>) -> RequestBuilder<'_> {
        self.request(Method::POST, path)
    }

    /// Build a `PUT` request to `path`.
    #[must_use]
    pub fn put(&self, path: impl Into<String>) -> RequestBuilder<'_> {
        self.request(Method::PUT, path)
    }

    /// Build a `PATCH` request to `path`.
    #[must_use]
    pub fn patch(&self, path: impl Into<String>) -> RequestBuilder<'_> {
        self.request(Method::PATCH, path)
    }

    /// Build a `DELETE` request to `path`.
    #[must_use]
    pub fn delete(&self, path: impl Into<String>) -> RequestBuilder<'_> {
        self.request(Method::DELETE, path)
    }

    /// Build a `HEAD` request to `path`.
    #[must_use]
    pub fn head(&self, path: impl Into<String>) -> RequestBuilder<'_> {
        self.request(Method::HEAD, path)
    }

    /// Build a request with the given method.
    #[must_use]
    pub fn request(&self, method: Method, path: impl Into<String>) -> RequestBuilder<'_> {
        RequestBuilder {
            client: self,
            method,
            path: path.into(),
            headers: Vec::new(),
            body: Body::empty(),
            content_type: None,
        }
    }
}

/// Builder for one outgoing test request.
pub struct RequestBuilder<'a> {
    client: &'a TestClient,
    method: Method,
    path: String,
    headers: Vec<(HeaderName, HeaderValue)>,
    body: Body,
    content_type: Option<&'static str>,
}

impl<'a> RequestBuilder<'a> {
    /// Add a request header.
    #[must_use]
    pub fn header(mut self, name: &str, value: &str) -> Self {
        if let (Ok(n), Ok(v)) = (HeaderName::try_from(name), HeaderValue::try_from(value)) {
            self.headers.push((n, v));
        }
        self
    }

    /// Set the request body to a JSON-serialized value, with the
    /// `content-type: application/json` header.
    #[must_use]
    pub fn json<T: serde::Serialize>(mut self, value: &T) -> Self {
        let bytes = serde_json::to_vec(value).unwrap_or_default();
        self.body = Body::from(bytes);
        self.content_type = Some("application/json");
        self
    }

    /// Set a form-encoded body (`application/x-www-form-urlencoded`).
    #[must_use]
    pub fn form(mut self, fields: &[(&str, &str)]) -> Self {
        let body = fields
            .iter()
            .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        self.body = Body::from(body);
        self.content_type = Some("application/x-www-form-urlencoded");
        self
    }

    /// Set a raw bytes body.
    #[must_use]
    pub fn body(mut self, body: impl Into<Body>) -> Self {
        self.body = body.into();
        self
    }

    /// Send the request and await the response.
    pub async fn send(self) -> TestResponse {
        let mut req = Request::builder().method(&self.method).uri(&self.path);
        if let Some(ct) = self.content_type {
            req = req.header("content-type", ct);
        }
        // Attach the cookie jar as a single `Cookie:` header (the
        // wire format the server side will see; Django's Client does
        // the same). Only emit when non-empty so requests that
        // legitimately need no cookies don't get a stray header.
        {
            let jar = self.client.cookies.lock().expect("cookie jar poisoned");
            if !jar.is_empty() {
                let cookie_header = jar
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                req = req.header("cookie", cookie_header);
            }
        }
        for (k, v) in self.headers {
            req = req.header(k, v);
        }
        let req = req.body(self.body).unwrap();
        let response = self
            .client
            .router
            .clone()
            .oneshot(req)
            .await
            .expect("test request panicked");
        // Extract Set-Cookie response headers *before* moving the
        // response into TestResponse::from_axum — the jar is merged
        // here so the next request through this client sees the
        // freshly issued cookies.
        let set_cookies: Vec<String> = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok().map(str::to_owned))
            .collect();
        {
            let mut jar = self.client.cookies.lock().expect("cookie jar poisoned");
            for raw in &set_cookies {
                if let Some((name, value)) = parse_set_cookie(raw) {
                    // RFC 6265: empty value + Max-Age=0 / Expires in
                    // the past is a deletion. We only parse name+value
                    // here, so an empty value is treated as deletion —
                    // which is what callers want (the next request
                    // shouldn't carry a logout's invalidation cookie).
                    if value.is_empty() {
                        jar.remove(&name);
                    } else {
                        jar.insert(name, value);
                    }
                }
            }
        }
        TestResponse::from_axum(response).await
    }
}

/// Parse the `name=value` head of a `Set-Cookie` header, ignoring
/// every `; attr=val` segment after. Returns `None` for malformed
/// inputs (no `=`).
fn parse_set_cookie(raw: &str) -> Option<(String, String)> {
    let head = raw.split(';').next()?.trim();
    let (name, value) = head.split_once('=')?;
    Some((name.trim().to_owned(), value.trim().to_owned()))
}

/// Captured response from a test request.
pub struct TestResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl TestResponse {
    async fn from_axum(response: axum::http::Response<Body>) -> Self {
        let (parts, body) = response.into_parts();
        let status = parts.status.as_u16();
        let headers: HashMap<String, String> = parts
            .headers
            .iter()
            .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
            .collect();
        // Use a generous limit (16 MiB) for test responses
        let body = to_bytes(body, 16 * 1024 * 1024)
            .await
            .unwrap_or_default()
            .to_vec();
        Self {
            status,
            headers,
            body,
        }
    }

    /// True when the status is 2xx.
    #[must_use]
    pub fn is_success(&self) -> bool {
        StatusCode::from_u16(self.status).map_or(false, |s| s.is_success())
    }

    /// Body as UTF-8 text. Returns empty string if the body isn't valid UTF-8.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8(self.body.clone()).unwrap_or_default()
    }

    /// Body parsed as JSON. Panics if the body isn't valid JSON for `T`
    /// (call this in tests where you want loud failures).
    #[must_use]
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> T {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "response body is not valid JSON: {e}\nbody: {}",
                self.text()
            )
        })
    }

    /// Body parsed as a generic JSON value (no panic — returns Value::Null on parse error).
    #[must_use]
    pub fn json_value(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }

    /// Look up a response header value (case-insensitive).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let lower = name.to_ascii_lowercase();
        self.headers.iter().find_map(|(k, v)| {
            if k.eq_ignore_ascii_case(&lower) {
                Some(v.as_str())
            } else {
                None
            }
        })
    }
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::{get, post};
    use serde_json::json;

    fn app() -> Router {
        Router::new()
            .route("/hello", get(|| async { "hi" }))
            .route("/echo", post(|body: String| async move { body }))
            .route(
                "/json",
                post(|body: axum::Json<serde_json::Value>| async move {
                    axum::Json(json!({"received": body.0}))
                }),
            )
            .route(
                "/status/{code}",
                get(
                    |axum::extract::Path(code): axum::extract::Path<u16>| async move {
                        axum::http::StatusCode::from_u16(code).unwrap_or(axum::http::StatusCode::OK)
                    },
                ),
            )
            .route(
                "/header_check",
                get(|h: axum::http::HeaderMap| async move {
                    h.get("x-custom")
                        .map_or("missing".to_owned(), |v| v.to_str().unwrap().to_owned())
                }),
            )
    }

    #[tokio::test]
    async fn get_returns_text() {
        let c = TestClient::new(app());
        let r = c.get("/hello").send().await;
        assert_eq!(r.status, 200);
        assert_eq!(r.text(), "hi");
        assert!(r.is_success());
    }

    #[tokio::test]
    async fn post_with_text_body_echos() {
        let c = TestClient::new(app());
        let r = c.post("/echo").body("hello world").send().await;
        assert_eq!(r.status, 200);
        assert_eq!(r.text(), "hello world");
    }

    #[tokio::test]
    async fn post_json_body_returns_json() {
        let c = TestClient::new(app());
        let r = c.post("/json").json(&json!({"a": 1})).send().await;
        assert_eq!(r.status, 200);
        let v = r.json_value();
        assert_eq!(v["received"]["a"], 1);
    }

    #[tokio::test]
    async fn header_round_trip() {
        let c = TestClient::new(app());
        let r = c
            .get("/header_check")
            .header("x-custom", "value42")
            .send()
            .await;
        assert_eq!(r.text(), "value42");
    }

    #[tokio::test]
    async fn status_path_param() {
        let c = TestClient::new(app());
        assert_eq!(c.get("/status/200").send().await.status, 200);
        assert_eq!(c.get("/status/404").send().await.status, 404);
        assert_eq!(c.get("/status/500").send().await.status, 500);
    }

    #[tokio::test]
    async fn test_client_is_reusable() {
        let c = TestClient::new(app());
        for _ in 0..3 {
            assert_eq!(c.get("/hello").send().await.status, 200);
        }
    }

    #[tokio::test]
    async fn header_lookup_case_insensitive() {
        let c = TestClient::new(app());
        let r = c.get("/hello").send().await;
        // axum sets content-type for text responses
        assert!(r.header("Content-Type").is_some() || r.header("content-type").is_some());
    }

    #[tokio::test]
    async fn form_body_encodes_correctly() {
        let c = TestClient::new(app());
        let r = c
            .post("/echo")
            .form(&[("name", "alice & bob"), ("age", "30")])
            .send()
            .await;
        let text = r.text();
        assert!(text.contains("name=alice%20%26%20bob"));
        assert!(text.contains("age=30"));
    }

    // ---------- cookie jar (issue #41) ----------

    fn cookie_app() -> Router {
        use axum::http::{header, HeaderMap, HeaderValue};
        use axum::response::IntoResponse;

        async fn login() -> impl IntoResponse {
            // Two cookies in one response. `append` (not `insert`)
            // stacks them so both arrive as separate Set-Cookie
            // headers — exactly the wire shape Django emits.
            let mut h = HeaderMap::new();
            h.append(
                header::SET_COOKIE,
                HeaderValue::from_static("session=abc123; Path=/; HttpOnly"),
            );
            h.append(
                header::SET_COOKIE,
                HeaderValue::from_static("csrftoken=xyz; Path=/"),
            );
            (h, "ok")
        }

        async fn whoami(h: HeaderMap) -> String {
            h.get("cookie")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("(no cookies)")
                .to_owned()
        }

        async fn logout() -> impl IntoResponse {
            let mut h = HeaderMap::new();
            // Empty value = deletion in RFC 6265 spirit (Max-Age=0).
            h.insert(
                header::SET_COOKIE,
                HeaderValue::from_static("session=; Path=/; Max-Age=0"),
            );
            (h, "bye")
        }

        Router::new()
            .route("/login", post(login))
            .route("/me", get(whoami))
            .route("/logout", post(logout))
    }

    #[tokio::test]
    async fn set_cookie_persists_into_jar() {
        let c = TestClient::new(cookie_app());
        c.post("/login").send().await;
        let jar = c.cookies();
        assert_eq!(jar.get("session").map(String::as_str), Some("abc123"));
        assert_eq!(jar.get("csrftoken").map(String::as_str), Some("xyz"));
    }

    #[tokio::test]
    async fn jar_replays_on_subsequent_request() {
        let c = TestClient::new(cookie_app());
        c.post("/login").send().await;
        let echoed = c.get("/me").send().await.text();
        // Server saw both cookies as a single header. Order isn't
        // guaranteed (HashMap iteration), so check membership instead
        // of string equality.
        assert!(echoed.contains("session=abc123"), "echoed: {echoed}");
        assert!(echoed.contains("csrftoken=xyz"), "echoed: {echoed}");
    }

    #[tokio::test]
    async fn clear_cookies_drops_jar() {
        let c = TestClient::new(cookie_app());
        c.post("/login").send().await;
        assert!(!c.cookies().is_empty());
        c.clear_cookies();
        assert!(c.cookies().is_empty());
        let echoed = c.get("/me").send().await.text();
        assert!(echoed.contains("(no cookies)"), "echoed: {echoed}");
    }

    #[tokio::test]
    async fn empty_cookie_value_is_treated_as_deletion() {
        let c = TestClient::new(cookie_app());
        c.post("/login").send().await;
        assert!(c.cookie("session").is_some());
        c.post("/logout").send().await;
        assert!(
            c.cookie("session").is_none(),
            "logout's Set-Cookie: session=; Max-Age=0 should delete the cookie"
        );
        // csrftoken wasn't cleared by the logout handler, so it stays.
        assert_eq!(c.cookie("csrftoken").as_deref(), Some("xyz"));
    }

    #[tokio::test]
    async fn set_cookie_manual_injection() {
        let c = TestClient::new(cookie_app());
        c.set_cookie("session", "manual-value");
        let echoed = c.get("/me").send().await.text();
        assert!(echoed.contains("session=manual-value"), "echoed: {echoed}");
    }

    #[tokio::test]
    async fn login_helper_returns_response_and_persists_cookies() {
        let c = TestClient::new(cookie_app());
        let r = c.login("/login", &[]).await;
        assert_eq!(r.status, 200);
        assert!(c.cookie("session").is_some());
    }

    #[tokio::test]
    async fn logout_with_path_clears_jar_and_hits_endpoint() {
        let c = TestClient::new(cookie_app());
        c.login("/login", &[]).await;
        assert!(c.cookie("session").is_some());
        let r = c.logout(Some("/logout")).await;
        assert_eq!(r.expect("response").status, 200);
        // Local clear ran *before* the request, so the jar is empty
        // regardless of what the server returned.
        assert!(c.cookie("session").is_none());
    }

    #[tokio::test]
    async fn logout_without_path_only_clears_locally() {
        let c = TestClient::new(cookie_app());
        c.login("/login", &[]).await;
        let r = c.logout(None).await;
        assert!(r.is_none());
        assert!(c.cookies().is_empty());
    }

    #[tokio::test]
    async fn no_cookie_header_emitted_when_jar_empty() {
        let c = TestClient::new(cookie_app());
        // Jar starts empty; the handler should report "(no cookies)".
        let echoed = c.get("/me").send().await.text();
        assert_eq!(echoed, "(no cookies)");
    }

    #[test]
    fn parse_set_cookie_handles_attributes() {
        assert_eq!(
            parse_set_cookie("session=abc; Path=/; HttpOnly").unwrap(),
            ("session".to_owned(), "abc".to_owned())
        );
        assert_eq!(
            parse_set_cookie("foo=bar").unwrap(),
            ("foo".to_owned(), "bar".to_owned())
        );
        assert_eq!(
            parse_set_cookie("expired=; Max-Age=0").unwrap(),
            ("expired".to_owned(), String::new())
        );
        assert!(parse_set_cookie("no-equals-sign").is_none());
    }
}
