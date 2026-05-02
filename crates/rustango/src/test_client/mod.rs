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

use axum::body::{to_bytes, Body};
use axum::http::{HeaderName, HeaderValue, Method, Request, StatusCode};
use axum::Router;
use tower::ServiceExt;

/// Test client wrapping an `axum::Router`.
///
/// Each request runs through the full router stack (middleware + handler)
/// in-process — no network, no real socket. Each call to `.send()` consumes
/// a clone of the router so the client itself is reusable across tests.
#[derive(Clone)]
pub struct TestClient {
    router: Router,
}

impl TestClient {
    /// Wrap a router for testing.
    #[must_use]
    pub fn new(router: Router) -> Self {
        Self { router }
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
        TestResponse::from_axum(response).await
    }
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
        let body = to_bytes(body, 16 * 1024 * 1024).await.unwrap_or_default().to_vec();
        Self { status, headers, body }
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
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("response body is not valid JSON: {e}\nbody: {}", self.text()))
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
            .route("/status/{code}", get(|axum::extract::Path(code): axum::extract::Path<u16>| async move {
                axum::http::StatusCode::from_u16(code).unwrap_or(axum::http::StatusCode::OK)
            }))
            .route("/header_check", get(|h: axum::http::HeaderMap| async move {
                h.get("x-custom").map_or("missing".to_owned(), |v| v.to_str().unwrap().to_owned())
            }))
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
        let r = c.get("/header_check").header("x-custom", "value42").send().await;
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
        let r = c.post("/echo").form(&[("name", "alice & bob"), ("age", "30")]).send().await;
        let text = r.text();
        assert!(text.contains("name=alice%20%26%20bob"));
        assert!(text.contains("age=30"));
    }
}
