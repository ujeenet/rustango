//! Opinionated HTTP client — `reqwest` wrapper with sane timeouts,
//! exponential-backoff retries on idempotent verbs / transient
//! failures, and a default `User-Agent`.
//!
//! `reqwest::Client` directly is fine for one-off calls — reach for
//! [`HttpClient`] when you want consistent retry + timeout behavior
//! across many call sites without duplicating builders.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::http_client::HttpClient;
//!
//! let client = HttpClient::builder()
//!     .timeout(std::time::Duration::from_secs(15))
//!     .max_retries(3)
//!     .user_agent("my-app/1.0")
//!     .build()?;
//!
//! // GET — retried automatically on transport errors / 5xx / 408 / 429.
//! let resp = client.get("https://api.example.com/users").send().await?;
//!
//! // POST — NOT retried by default (could be non-idempotent). Opt in:
//! let resp = client
//!     .post("https://api.example.com/idempotent")
//!     .send_with_retry(true)
//!     .await?;
//! ```
//!
//! ## What gets retried
//!
//! With `RetryPolicy::default()`:
//!
//! - GET / HEAD / OPTIONS — retried by default
//! - POST / PUT / PATCH / DELETE — only when you opt in via
//!   [`RequestBuilder::send_with_retry`] (the call must be idempotent)
//!
//! And only when one of these is true:
//!
//! - Transport error (connect refused, DNS failure, TLS handshake, timeout)
//! - HTTP 408 / 429 / 5xx response
//! - The response carries `Retry-After`, in which case we honor it as
//!   the wait before the next attempt (capped at `max_backoff`).

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, RETRY_AFTER, USER_AGENT};
use reqwest::{Method, Response, StatusCode, Url};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("reqwest: {0}")]
    Reqwest(String),
    #[error("invalid header: {0}")]
    Header(String),
    #[error("invalid URL: {0}")]
    Url(String),
}

impl From<reqwest::Error> for HttpError {
    fn from(e: reqwest::Error) -> Self {
        Self::Reqwest(e.to_string())
    }
}

#[derive(Clone, Debug)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    /// Whether to honor a `Retry-After` response header.
    pub honor_retry_after: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_backoff: Duration::from_millis(200),
            max_backoff: Duration::from_secs(10),
            honor_retry_after: true,
        }
    }
}

impl RetryPolicy {
    /// Disable retries entirely.
    #[must_use]
    pub fn off() -> Self {
        Self {
            max_retries: 0,
            ..Self::default()
        }
    }

    fn backoff_for(&self, attempt: u32) -> Duration {
        let exp = 1u64 << attempt.min(20);
        let proposed = self
            .initial_backoff
            .saturating_mul(u32::try_from(exp).unwrap_or(u32::MAX));
        proposed.min(self.max_backoff)
    }
}

/// HTTP client with built-in retry behavior.
///
/// Cheap to clone — the internal `reqwest::Client` is `Arc`-wrapped.
#[derive(Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
    policy: Arc<RetryPolicy>,
}

impl HttpClient {
    #[must_use]
    pub fn builder() -> HttpClientBuilder {
        HttpClientBuilder::default()
    }

    /// Wrap an existing `reqwest::Client` with the default retry policy.
    /// Useful when you've already built a client with custom TLS / proxy
    /// settings.
    #[must_use]
    pub fn from_reqwest(client: reqwest::Client) -> Self {
        Self {
            inner: client,
            policy: Arc::new(RetryPolicy::default()),
        }
    }

    #[must_use]
    pub fn with_policy(mut self, policy: RetryPolicy) -> Self {
        self.policy = Arc::new(policy);
        self
    }

    pub fn get(&self, url: impl IntoUrl) -> RequestBuilder {
        self.request(Method::GET, url)
    }
    pub fn post(&self, url: impl IntoUrl) -> RequestBuilder {
        self.request(Method::POST, url)
    }
    pub fn put(&self, url: impl IntoUrl) -> RequestBuilder {
        self.request(Method::PUT, url)
    }
    pub fn patch(&self, url: impl IntoUrl) -> RequestBuilder {
        self.request(Method::PATCH, url)
    }
    pub fn delete(&self, url: impl IntoUrl) -> RequestBuilder {
        self.request(Method::DELETE, url)
    }
    pub fn head(&self, url: impl IntoUrl) -> RequestBuilder {
        self.request(Method::HEAD, url)
    }
    /// Build a `QUERY` request (RFC 10008) — a safe, idempotent read with
    /// a body. Classified idempotent, so it's retried by default like GET.
    pub fn query(&self, url: impl IntoUrl) -> RequestBuilder {
        self.request(
            Method::from_bytes(b"QUERY").expect("QUERY is a valid method token"),
            url,
        )
    }

    pub fn request(&self, method: Method, url: impl IntoUrl) -> RequestBuilder {
        RequestBuilder {
            client: self.inner.clone(),
            policy: self.policy.clone(),
            method,
            url: url.into_url(),
            headers: HeaderMap::new(),
            body: None,
            retry_override: None,
        }
    }
}

/// `Url`-or-string convenience trait so callers can pass either form.
pub trait IntoUrl {
    fn into_url(self) -> Result<Url, HttpError>;
}

impl IntoUrl for String {
    fn into_url(self) -> Result<Url, HttpError> {
        Url::parse(&self).map_err(|e| HttpError::Url(e.to_string()))
    }
}
impl IntoUrl for &str {
    fn into_url(self) -> Result<Url, HttpError> {
        Url::parse(self).map_err(|e| HttpError::Url(e.to_string()))
    }
}
impl IntoUrl for Url {
    fn into_url(self) -> Result<Url, HttpError> {
        Ok(self)
    }
}

// =====================================================================
// Builder
// =====================================================================

#[derive(Default)]
pub struct HttpClientBuilder {
    timeout: Option<Duration>,
    connect_timeout: Option<Duration>,
    user_agent: Option<String>,
    default_headers: HeaderMap,
    policy: Option<RetryPolicy>,
}

impl HttpClientBuilder {
    #[must_use]
    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = Some(t);
        self
    }
    #[must_use]
    pub fn connect_timeout(mut self, t: Duration) -> Self {
        self.connect_timeout = Some(t);
        self
    }
    #[must_use]
    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = Some(ua.into());
        self
    }
    #[must_use]
    pub fn max_retries(mut self, n: u32) -> Self {
        let mut p = self.policy.take().unwrap_or_default();
        p.max_retries = n;
        self.policy = Some(p);
        self
    }
    #[must_use]
    pub fn retry_policy(mut self, p: RetryPolicy) -> Self {
        self.policy = Some(p);
        self
    }
    /// Add a header that's sent on every request from this client.
    ///
    /// # Errors
    /// Returns `HttpError::Header` when the name or value isn't valid.
    pub fn default_header(
        mut self,
        name: &'static str,
        value: impl AsRef<str>,
    ) -> Result<Self, HttpError> {
        let name = HeaderName::from_static(name);
        let value =
            HeaderValue::from_str(value.as_ref()).map_err(|e| HttpError::Header(e.to_string()))?;
        self.default_headers.insert(name, value);
        Ok(self)
    }

    /// Build the client. Defaults: 30s overall timeout, 10s connect
    /// timeout, `rustango-http/<crate version>` User-Agent.
    ///
    /// # Errors
    /// Underlying reqwest builder error (only happens with bizarre
    /// TLS / runtime config — unlikely with the workspace defaults).
    pub fn build(self) -> Result<HttpClient, HttpError> {
        let timeout = self.timeout.unwrap_or(Duration::from_secs(30));
        let connect = self.connect_timeout.unwrap_or(Duration::from_secs(10));
        let ua = self
            .user_agent
            .unwrap_or_else(|| concat!("rustango-http/", env!("CARGO_PKG_VERSION")).to_owned());

        let mut headers = self.default_headers;
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&ua).map_err(|e| HttpError::Header(e.to_string()))?,
        );

        let client = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(connect)
            .default_headers(headers)
            .build()?;
        Ok(HttpClient {
            inner: client,
            policy: Arc::new(self.policy.unwrap_or_default()),
        })
    }
}

// =====================================================================
// RequestBuilder
// =====================================================================

pub struct RequestBuilder {
    client: reqwest::Client,
    policy: Arc<RetryPolicy>,
    method: Method,
    url: Result<Url, HttpError>,
    headers: HeaderMap,
    body: Option<Vec<u8>>,
    retry_override: Option<bool>,
}

impl RequestBuilder {
    /// Override the default per-method retry policy. `true` forces
    /// retries on any verb (use only when you've confirmed the call is
    /// idempotent); `false` disables retries even on GET.
    #[must_use]
    pub fn send_with_retry(mut self, on: bool) -> Self {
        self.retry_override = Some(on);
        self
    }

    /// Add a header to this request.
    ///
    /// # Errors
    /// Returns `HttpError::Header` when the value isn't valid.
    pub fn header(mut self, name: &'static str, value: impl AsRef<str>) -> Result<Self, HttpError> {
        let name = HeaderName::from_static(name);
        let value =
            HeaderValue::from_str(value.as_ref()).map_err(|e| HttpError::Header(e.to_string()))?;
        self.headers.insert(name, value);
        Ok(self)
    }

    pub fn bearer_auth(self, token: impl AsRef<str>) -> Result<Self, HttpError> {
        self.header("authorization", format!("Bearer {}", token.as_ref()))
    }

    /// Set the body to a JSON-serialized value.
    ///
    /// # Errors
    /// Returns `HttpError::Header` when serialization fails.
    pub fn json(mut self, value: &impl Serialize) -> Result<Self, HttpError> {
        let body = serde_json::to_vec(value).map_err(|e| HttpError::Header(e.to_string()))?;
        self.headers.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );
        self.body = Some(body);
        Ok(self)
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// Send the request, applying the configured retry policy.
    ///
    /// # Errors
    /// Final `HttpError::Reqwest` when retries are exhausted.
    pub async fn send(self) -> Result<Response, HttpError> {
        let url = self.url?;
        let policy = self.policy.clone();
        let retry_on = self
            .retry_override
            .unwrap_or_else(|| is_method_idempotent(&self.method));

        let mut attempt: u32 = 0;
        loop {
            let mut req = self.client.request(self.method.clone(), url.clone());
            for (k, v) in &self.headers {
                req = req.header(k.clone(), v.clone());
            }
            if let Some(b) = &self.body {
                req = req.body(b.clone());
            }
            let result = req.send().await;
            match result {
                Ok(resp) => {
                    if retry_on
                        && attempt < policy.max_retries
                        && is_status_retryable(resp.status())
                    {
                        let backoff = retry_after(&resp, &policy)
                            .unwrap_or_else(|| policy.backoff_for(attempt));
                        attempt += 1;
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    return Ok(resp);
                }
                Err(e) => {
                    if retry_on && attempt < policy.max_retries && is_transport_retryable(&e) {
                        let backoff = policy.backoff_for(attempt);
                        attempt += 1;
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    return Err(e.into());
                }
            }
        }
    }
}

fn is_method_idempotent(m: &Method) -> bool {
    // QUERY (RFC 10008) is safe + idempotent, so it's retry-safe like GET.
    // Matched by name to stay independent of the `admin`-gated `http_query`
    // module.
    matches!(*m, Method::GET | Method::HEAD | Method::OPTIONS) || m.as_str() == "QUERY"
}

fn is_status_retryable(s: StatusCode) -> bool {
    matches!(s.as_u16(), 408 | 429) || s.is_server_error()
}

fn is_transport_retryable(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}

fn retry_after(resp: &Response, policy: &RetryPolicy) -> Option<Duration> {
    if !policy.honor_retry_after {
        return None;
    }
    let raw = resp.headers().get(RETRY_AFTER)?.to_str().ok()?;
    // RFC 7231: either delta-seconds or HTTP-date. We support seconds only;
    // HTTP-date is rare for Retry-After in modern APIs.
    let secs: u64 = raw.trim().parse().ok()?;
    Some(Duration::from_secs(secs).min(policy.max_backoff))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::{get, post};
    use axum::Router;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;
    use tokio::net::TcpListener;

    async fn start(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let h = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(20)).await;
        (url, h)
    }

    #[test]
    fn idempotent_methods_classified() {
        assert!(is_method_idempotent(&Method::GET));
        assert!(is_method_idempotent(&Method::HEAD));
        assert!(is_method_idempotent(&Method::OPTIONS));
        assert!(!is_method_idempotent(&Method::POST));
        assert!(!is_method_idempotent(&Method::PUT));
        assert!(!is_method_idempotent(&Method::PATCH));
        assert!(!is_method_idempotent(&Method::DELETE));
        // QUERY (RFC 10008) is safe + idempotent → retry-safe.
        assert!(is_method_idempotent(&Method::from_bytes(b"QUERY").unwrap()));
    }

    #[test]
    fn retryable_statuses() {
        assert!(is_status_retryable(StatusCode::REQUEST_TIMEOUT));
        assert!(is_status_retryable(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_status_retryable(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_status_retryable(StatusCode::BAD_GATEWAY));
        assert!(is_status_retryable(StatusCode::GATEWAY_TIMEOUT));
        assert!(!is_status_retryable(StatusCode::OK));
        assert!(!is_status_retryable(StatusCode::NOT_FOUND));
        assert!(!is_status_retryable(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn backoff_grows_exponentially_until_cap() {
        let p = RetryPolicy {
            max_retries: 5,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(2),
            honor_retry_after: true,
        };
        assert_eq!(p.backoff_for(0), Duration::from_millis(100));
        assert_eq!(p.backoff_for(1), Duration::from_millis(200));
        assert_eq!(p.backoff_for(2), Duration::from_millis(400));
        assert_eq!(p.backoff_for(3), Duration::from_millis(800));
        assert_eq!(p.backoff_for(4), Duration::from_millis(1600));
        // 100 * 2^5 = 3200 ms > 2 s cap → returns 2 s
        assert_eq!(p.backoff_for(5), Duration::from_secs(2));
        assert_eq!(p.backoff_for(20), Duration::from_secs(2));
    }

    #[test]
    fn retry_policy_off_disables_retries() {
        let p = RetryPolicy::off();
        assert_eq!(p.max_retries, 0);
    }

    #[tokio::test]
    async fn get_request_succeeds_on_first_try() {
        let app = Router::new().route("/", get(|| async { "hello" }));
        let (url, srv) = start(app).await;

        let client = HttpClient::builder().build().unwrap();
        let resp = client.get(url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "hello");
        srv.abort();
    }

    #[tokio::test]
    async fn query_method_and_body_go_over_the_wire() {
        // `any` accepts the extension method; the handler echoes method +
        // body so we can prove the connector sent QUERY with its body.
        let app = Router::new().route(
            "/",
            axum::routing::any(|m: Method, body: String| async move { format!("{m}:{body}") }),
        );
        let (url, srv) = start(app).await;

        let client = HttpClient::builder().build().unwrap();
        let resp = client
            .query(url)
            .body(b"q=x".to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "QUERY:q=x");
        srv.abort();
    }

    #[tokio::test]
    async fn get_retries_on_5xx_then_succeeds() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let app = Router::new().route(
            "/",
            get(move || {
                let c = c.clone();
                async move {
                    let n = c.fetch_add(1, Ordering::SeqCst);
                    if n < 2 {
                        (axum::http::StatusCode::SERVICE_UNAVAILABLE, "down")
                    } else {
                        (axum::http::StatusCode::OK, "ok")
                    }
                }
            }),
        );
        let (url, srv) = start(app).await;

        let client = HttpClient::builder()
            .retry_policy(RetryPolicy {
                max_retries: 4,
                initial_backoff: Duration::from_millis(20),
                max_backoff: Duration::from_millis(100),
                honor_retry_after: true,
            })
            .build()
            .unwrap();
        let resp = client.get(url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(counter.load(Ordering::SeqCst), 3);
        srv.abort();
    }

    #[tokio::test]
    async fn post_does_not_retry_by_default() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let app = Router::new().route(
            "/",
            post(move || {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    (axum::http::StatusCode::SERVICE_UNAVAILABLE, "down")
                }
            }),
        );
        let (url, srv) = start(app).await;

        let client = HttpClient::builder()
            .retry_policy(RetryPolicy {
                max_retries: 3,
                initial_backoff: Duration::from_millis(20),
                max_backoff: Duration::from_millis(100),
                honor_retry_after: true,
            })
            .build()
            .unwrap();
        let resp = client.post(url).body(b"x".to_vec()).send().await.unwrap();
        assert_eq!(resp.status(), 503);
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "POST is not retried by default"
        );
        srv.abort();
    }

    #[tokio::test]
    async fn post_retries_when_send_with_retry_true() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let app = Router::new().route(
            "/",
            post(move || {
                let c = c.clone();
                async move {
                    let n = c.fetch_add(1, Ordering::SeqCst);
                    if n < 1 {
                        (axum::http::StatusCode::SERVICE_UNAVAILABLE, "down")
                    } else {
                        (axum::http::StatusCode::CREATED, "ok")
                    }
                }
            }),
        );
        let (url, srv) = start(app).await;

        let client = HttpClient::builder()
            .retry_policy(RetryPolicy {
                max_retries: 3,
                initial_backoff: Duration::from_millis(20),
                max_backoff: Duration::from_millis(100),
                honor_retry_after: true,
            })
            .build()
            .unwrap();
        let resp = client
            .post(url)
            .body(b"x".to_vec())
            .send_with_retry(true)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        assert_eq!(counter.load(Ordering::SeqCst), 2);
        srv.abort();
    }

    #[tokio::test]
    async fn retry_after_header_overrides_backoff() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let app = Router::new().route(
            "/",
            get(move || {
                let c = c.clone();
                async move {
                    let n = c.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        (
                            axum::http::StatusCode::TOO_MANY_REQUESTS,
                            [("retry-after", "1")],
                            "slow down",
                        )
                            .into_response()
                    } else {
                        axum::response::Response::new("ok".into())
                    }
                }
            }),
        );
        use axum::response::IntoResponse;
        let (url, srv) = start(app).await;

        let client = HttpClient::builder()
            .retry_policy(RetryPolicy {
                max_retries: 2,
                initial_backoff: Duration::from_millis(10),
                max_backoff: Duration::from_secs(5),
                honor_retry_after: true,
            })
            .build()
            .unwrap();

        let start_t = std::time::Instant::now();
        let resp = client.get(url).send().await.unwrap();
        let elapsed = start_t.elapsed();
        assert_eq!(resp.status(), 200);
        // The retry waited ~1 second (Retry-After), not the 10 ms initial backoff.
        assert!(
            elapsed >= Duration::from_millis(900),
            "expected ~1s wait, got {elapsed:?}"
        );
        srv.abort();
    }

    #[tokio::test]
    async fn json_body_sets_content_type_and_serializes() {
        // Echo back the body so we can verify both are right.
        let app = Router::new().route(
            "/",
            post(|req: axum::http::Request<axum::body::Body>| async move {
                let ct = req
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_owned();
                let body = axum::body::to_bytes(req.into_body(), 1 << 16)
                    .await
                    .unwrap();
                axum::Json(serde_json::json!({"ct": ct, "body": String::from_utf8_lossy(&body)}))
            }),
        );
        let (url, srv) = start(app).await;

        let client = HttpClient::builder().build().unwrap();
        let resp = client
            .post(url)
            .json(&serde_json::json!({"k": "v"}))
            .unwrap()
            .send()
            .await
            .unwrap();
        let v: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(v["ct"], "application/json");
        assert!(v["body"].as_str().unwrap().contains("\"k\":\"v\""));
        srv.abort();
    }

    #[tokio::test]
    async fn user_agent_default_includes_crate_version() {
        let app = Router::new().route(
            "/",
            get(|req: axum::http::Request<axum::body::Body>| async move {
                req.headers()
                    .get("user-agent")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_owned()
            }),
        );
        let (url, srv) = start(app).await;

        let client = HttpClient::builder().build().unwrap();
        let resp = client.get(url).send().await.unwrap();
        let ua = resp.text().await.unwrap();
        assert!(ua.starts_with("rustango-http/"));
        srv.abort();
    }

    #[tokio::test]
    async fn user_agent_override_takes_precedence() {
        let app = Router::new().route(
            "/",
            get(|req: axum::http::Request<axum::body::Body>| async move {
                req.headers()
                    .get("user-agent")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_owned()
            }),
        );
        let (url, srv) = start(app).await;

        let client = HttpClient::builder()
            .user_agent("acme/1.0")
            .build()
            .unwrap();
        let resp = client.get(url).send().await.unwrap();
        assert_eq!(resp.text().await.unwrap(), "acme/1.0");
        srv.abort();
    }

    #[tokio::test]
    async fn invalid_url_returns_url_error() {
        let client = HttpClient::builder().build().unwrap();
        let err = client.get("not a url").send().await.unwrap_err();
        assert!(matches!(err, HttpError::Url(_)));
    }
}
