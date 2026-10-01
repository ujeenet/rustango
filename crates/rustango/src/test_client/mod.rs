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
use std::net::SocketAddr;
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
/// ## Browser behaviour
///
/// Requests look like a browser's on `http://testserver`, so a test that
/// passes here also passes in one:
///
/// - **Cookie jar.** `Set-Cookie` is stored with its `Path`; `Max-Age=0`
///   or a past `Expires` deletes, and a cookie is only sent to matching
///   paths. `Domain` and `Secure` are ignored (one host).
/// - **`Host: testserver`**, plus a same-origin `Origin` on unsafe
///   methods, unless the request sets its own.
/// - **Peer address** `127.0.0.1` as `ConnectInfo`, like a real
///   connection; override per request with [`RequestBuilder::remote_addr`].
/// - **CSRF** is enforced, not bypassed: send the token with
///   [`RequestBuilder::with_csrf`] or a `_csrf` form field.
#[derive(Clone)]
pub struct TestClient {
    router: Router,
    cookies: Arc<Mutex<CookieJar>>,
}

/// Host every request is addressed to unless it sets its own.
pub const TEST_HOST: &str = "testserver";

impl TestClient {
    /// Wrap a router for testing.
    #[must_use]
    pub fn new(router: Router) -> Self {
        Self {
            router,
            cookies: Arc::new(Mutex::new(CookieJar::default())),
        }
    }

    fn jar(&self) -> std::sync::MutexGuard<'_, CookieJar> {
        self.cookies.lock().expect("cookie jar poisoned")
    }

    /// Snapshot of the live cookie jar (name → value). Useful in
    /// tests for asserting that a session cookie was issued.
    #[must_use]
    pub fn cookies(&self) -> HashMap<String, String> {
        self.jar().snapshot()
    }

    /// Read one cookie by name. Returns `None` if absent or expired.
    #[must_use]
    pub fn cookie(&self, name: &str) -> Option<String> {
        self.jar().snapshot().remove(name)
    }

    /// The CSRF token cookie the [`crate::forms::csrf`] layer set, if any.
    #[must_use]
    pub fn csrf_token(&self) -> Option<String> {
        self.cookie(crate::forms::csrf::CSRF_COOKIE)
    }

    /// Inject a cookie directly into the jar without going through a
    /// `Set-Cookie` round trip. Handy for tests that pre-seed an
    /// auth cookie minted by a session backend.
    pub fn set_cookie(&self, name: impl Into<String>, value: impl Into<String>) {
        self.jar().store(StoredCookie {
            name: name.into(),
            value: value.into(),
            path: "/".to_owned(),
            expires_at: None,
        });
    }

    /// Drop every cookie from the jar.
    pub fn clear_cookies(&self) {
        self.jar().cookies.clear();
    }

    /// Convenience: POST a form to `path` and return the response.
    /// The client carries a cookie jar so any session cookie set by
    /// the login handler is automatically attached to subsequent
    /// requests.
    pub async fn login(&self, path: impl Into<String>, fields: &[(&str, &str)]) -> TestResponse {
        self.post(path).form(fields).send().await
    }

    /// Mint a tenant session cookie directly into the jar. Bypasses
    /// the login form entirely: subsequent requests look authenticated to
    /// the [`crate::extractors::SessionUser`] extractor as long as
    /// `user`'s row is `active = true` and its password hash is unchanged.
    ///
    /// Useful when the login flow is incidental to what's being tested
    /// (typing through the form per test is slow, and tests of
    /// non-auth handlers shouldn't depend on the auth surface working).
    ///
    /// `slug` is the tenant slug the cookie should be bound to (the
    /// decoder rejects cookies minted for a different slug, so this
    /// must match the resolved tenant at request time). `ttl_secs` is
    /// the session expiry; one hour (3600) is a fine default for a
    /// test run.
    ///
    /// Skips the login form: mints the cookie directly.
    #[cfg(feature = "tenancy")]
    pub fn force_login_tenant_user(
        &self,
        secret: &crate::tenancy::session::SessionSecret,
        slug: impl Into<String>,
        user: &crate::tenancy::User,
        ttl_secs: i64,
    ) -> &Self {
        use crate::tenancy::tenant_console;
        let mut payload = tenant_console::TenantSessionPayload::new(
            user.id
                .get()
                .copied()
                .expect("force_login_tenant_user needs a saved user row"),
            slug,
            ttl_secs,
            tenant_console::PasswordFingerprint::of(secret, &user.password_hash),
        );
        payload.iat = crate::session::issued_at(user.sessions_revoked_at);
        let cookie = tenant_console::encode(secret, &payload);
        self.set_cookie(tenant_console::COOKIE_NAME, cookie);
        self
    }

    /// Mint an operator-console session cookie directly into the jar
    /// — `force_login` for the operator stack. Same shape as
    /// [`Self::force_login_tenant_user`] but for the operator
    /// (control-plane) cookie that [`crate::extractors::SessionOperator`]
    /// reads.
    ///
    /// Operator sessions aren't slug-bound — operators span all
    /// tenants in the registry — so no slug parameter.
    ///
    /// The operator counterpart of [`TestClient::force_login_tenant_user`].
    #[cfg(feature = "tenancy")]
    pub fn force_login_operator(
        &self,
        secret: &crate::tenancy::session::SessionSecret,
        operator: &crate::tenancy::Operator,
        ttl_secs: i64,
    ) -> &Self {
        use crate::tenancy::session;
        let mut payload = session::SessionPayload::new(
            operator
                .id
                .get()
                .copied()
                .expect("force_login_operator needs a saved operator row"),
            ttl_secs,
            session::PasswordFingerprint::of(secret, &operator.password_hash),
        );
        payload.iat = crate::session::issued_at(operator.sessions_revoked_at);
        let cookie = session::encode(secret, &payload);
        self.set_cookie(session::COOKIE_NAME, cookie);
        self
    }

    /// Log out like a browser: `Some(path)` POSTs there with the jar and
    /// the CSRF token, and the jar changes only by the response's
    /// `Set-Cookie`. `None` just drops the jar locally.
    pub async fn logout(&self, path: Option<&str>) -> Option<TestResponse> {
        match path {
            Some(p) => Some(self.post(p.to_owned()).with_csrf().send().await),
            None => {
                self.clear_cookies();
                None
            }
        }
    }

    /// Build a `GET` request to `path`.
    #[must_use]
    pub fn get(&self, path: impl Into<String>) -> RequestBuilder<'_> {
        self.request(Method::GET, path)
    }

    /// Issue a GET request and follow up to `max_hops` 3xx redirects.
    /// Returns the final response **plus** the chain of visited
    /// (status, location) pairs. See
    /// [`RequestBuilder::send_following_redirects`].
    ///
    /// ```ignore
    /// let (final_res, chain) = client.get_following_redirects("/old", 5).await;
    /// // chain = [(302, "/new"), (302, "/canonical"), (200, "/canonical")]
    /// assert_eq!(final_res.status, 200);
    /// assert_eq!(chain.last().unwrap().1, "/canonical");
    /// ```
    pub async fn get_following_redirects(
        &self,
        path: impl Into<String>,
        max_hops: usize,
    ) -> (TestResponse, Vec<(u16, String)>) {
        self.get(path).send_following_redirects(max_hops).await
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

    /// Build a `QUERY` request to `path` (RFC 10008). Pair with
    /// `.form(...)` or `.json(...)` to send the query criteria in the
    /// body; see [`crate::http_query`] and [`crate::params::Params`].
    #[must_use]
    pub fn query(&self, path: impl Into<String>) -> RequestBuilder<'_> {
        self.request(crate::http_query::QUERY.clone(), path)
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
            remote_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
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
    remote_addr: SocketAddr,
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

    /// Send the jar's CSRF token as the `X-CSRF-Token` header, as a
    /// page's script would. No-op when the jar holds no token.
    #[must_use]
    pub fn with_csrf(self) -> Self {
        match self.client.csrf_token() {
            Some(token) => self.header("x-csrf-token", &token),
            None => self,
        }
    }

    /// The peer address handlers see as `ConnectInfo<SocketAddr>`.
    /// Defaults to `127.0.0.1`.
    #[must_use]
    pub fn remote_addr(mut self, addr: SocketAddr) -> Self {
        self.remote_addr = addr;
        self
    }

    /// Send the request and await the response.
    pub async fn send(self) -> TestResponse {
        let client = self.client;
        client
            .dispatch(Outgoing {
                method: self.method,
                path: self.path,
                headers: self.headers,
                body: self.body,
                content_type: self.content_type,
                remote_addr: self.remote_addr,
            })
            .await
    }

    /// Send, then follow up to `max_hops` redirects like a browser:
    /// 301/302/303 re-request with `GET` (`HEAD` stays `HEAD`) and no
    /// body; 307/308 repeat the method and body. Returns the final
    /// response and the visited `(status, location)` chain, ending with
    /// the final status and path.
    pub async fn send_following_redirects(
        self,
        max_hops: usize,
    ) -> (TestResponse, Vec<(u16, String)>) {
        let client = self.client;
        let mut body = Some(
            to_bytes(self.body, 16 * 1024 * 1024)
                .await
                .unwrap_or_default(),
        );
        let mut req = Outgoing {
            method: self.method,
            path: self.path,
            headers: self.headers,
            body: Body::empty(),
            content_type: self.content_type,
            remote_addr: self.remote_addr,
        };
        let mut chain: Vec<(u16, String)> = Vec::new();
        let mut last = client.dispatch(req.with_body(body.clone())).await;
        for _ in 0..max_hops {
            let status = last.status;
            if !(300..400).contains(&status) {
                break;
            }
            let Some(location) = last.header("location").map(str::to_owned) else {
                break;
            };
            chain.push((status, location.clone()));
            req.path = resolve_location(&req.path, &location);
            if !matches!(status, 307 | 308) && req.method != Method::HEAD {
                req.method = Method::GET;
                req.content_type = None;
                body = None;
            }
            last = client.dispatch(req.with_body(body.clone())).await;
        }
        chain.push((last.status, req.path));
        (last, chain)
    }
}

/// One request as [`TestClient::dispatch`] sends it.
struct Outgoing {
    method: Method,
    path: String,
    headers: Vec<(HeaderName, HeaderValue)>,
    body: Body,
    content_type: Option<&'static str>,
    remote_addr: SocketAddr,
}

impl Outgoing {
    fn with_body(&self, body: Option<axum::body::Bytes>) -> Self {
        Self {
            method: self.method.clone(),
            path: self.path.clone(),
            headers: self.headers.clone(),
            body: body.map_or_else(Body::empty, Body::from),
            content_type: self.content_type,
            remote_addr: self.remote_addr,
        }
    }
}

impl TestClient {
    async fn dispatch(&self, out: Outgoing) -> TestResponse {
        let has = |name: &HeaderName| out.headers.iter().any(|(k, _)| k == name);
        let host = out
            .headers
            .iter()
            .find(|(k, _)| k == axum::http::header::HOST)
            .and_then(|(_, v)| v.to_str().ok())
            .unwrap_or(TEST_HOST)
            .to_owned();
        let mut req = Request::builder().method(&out.method).uri(&out.path);
        if !has(&axum::http::header::HOST) {
            req = req.header(axum::http::header::HOST, TEST_HOST);
        }
        // Browsers send Origin on every request that is not GET/HEAD.
        let safe = matches!(
            out.method,
            Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
        );
        if !safe && !has(&axum::http::header::ORIGIN) {
            req = req.header(axum::http::header::ORIGIN, format!("http://{host}"));
        }
        if let Some(ct) = out.content_type {
            req = req.header("content-type", ct);
        }
        let request_path = out.path.split(['?', '#']).next().unwrap_or("/").to_owned();
        if let Some(cookie_header) = self.jar().header_for(&request_path) {
            req = req.header("cookie", cookie_header);
        }
        for (k, v) in out.headers {
            req = req.header(k, v);
        }
        let mut req = req.body(out.body).expect("invalid test request");
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(out.remote_addr));
        let response = self
            .router
            .clone()
            .oneshot(req)
            .await
            .expect("test request panicked");
        {
            let mut jar = self.jar();
            for raw in response.headers().get_all("set-cookie") {
                if let Ok(raw) = raw.to_str() {
                    jar.apply_set_cookie(raw, &request_path);
                }
            }
        }
        TestResponse::from_axum(response).await
    }
}

/// `location` resolved against the current request path, as a path.
/// Absolute URLs keep only their path and query (one host).
fn resolve_location(current: &str, location: &str) -> String {
    if let Some(rest) = location
        .strip_prefix("http://")
        .or_else(|| location.strip_prefix("https://"))
    {
        return match rest.find('/') {
            Some(i) => rest[i..].to_owned(),
            None => "/".to_owned(),
        };
    }
    if location.starts_with('/') {
        return location.to_owned();
    }
    let base = current.split(['?', '#']).next().unwrap_or("/");
    let dir = &base[..=base.rfind('/').unwrap_or(0)];
    format!("{dir}{location}")
}

/// One cookie as a browser keeps it: keyed by name and path.
#[derive(Clone, Debug)]
struct StoredCookie {
    name: String,
    value: String,
    path: String,
    expires_at: Option<i64>,
}

/// The client's cookie store (RFC 6265 subset for one host).
#[derive(Default)]
struct CookieJar {
    cookies: Vec<StoredCookie>,
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

impl CookieJar {
    fn live(&self) -> impl Iterator<Item = &StoredCookie> {
        let now = unix_now();
        self.cookies
            .iter()
            .filter(move |c| c.expires_at.is_none_or(|t| t > now))
    }

    fn store(&mut self, cookie: StoredCookie) {
        self.cookies
            .retain(|c| !(c.name == cookie.name && c.path == cookie.path));
        self.cookies.push(cookie);
    }

    /// Name → value of the live cookies; the longest path wins a clash.
    fn snapshot(&self) -> HashMap<String, String> {
        let mut live: Vec<&StoredCookie> = self.live().collect();
        live.sort_by_key(|c| c.path.len());
        live.into_iter()
            .map(|c| (c.name.clone(), c.value.clone()))
            .collect()
    }

    /// The `Cookie:` header for `path`, longest paths first (RFC 6265 5.4).
    fn header_for(&self, path: &str) -> Option<String> {
        let mut matching: Vec<&StoredCookie> = self
            .live()
            .filter(|c| path_matches(path, &c.path))
            .collect();
        if matching.is_empty() {
            return None;
        }
        matching.sort_by_key(|c| std::cmp::Reverse(c.path.len()));
        Some(
            matching
                .iter()
                .map(|c| format!("{}={}", c.name, c.value))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    /// Store, replace or delete per one `Set-Cookie` received for `request_path`.
    fn apply_set_cookie(&mut self, raw: &str, request_path: &str) {
        let Ok(parsed) = cookie::Cookie::parse(raw) else {
            return;
        };
        let path = match parsed.path() {
            Some(p) if p.starts_with('/') => p.to_owned(),
            _ => default_path(request_path),
        };
        // Max-Age wins over Expires (RFC 6265 5.3 step 3).
        let expires_at = match (parsed.max_age(), parsed.expires_datetime()) {
            (Some(age), _) => Some(unix_now() + age.whole_seconds()),
            (None, Some(at)) => Some(at.unix_timestamp()),
            (None, None) => None,
        };
        let cookie = StoredCookie {
            name: parsed.name().to_owned(),
            value: parsed.value().to_owned(),
            path,
            expires_at,
        };
        if cookie.expires_at.is_some_and(|t| t <= unix_now()) {
            self.cookies
                .retain(|c| !(c.name == cookie.name && c.path == cookie.path));
        } else {
            self.store(cookie);
        }
    }
}

/// RFC 6265 5.1.4 default-path: the request path up to its last `/`.
fn default_path(request_path: &str) -> String {
    match request_path.rfind('/') {
        Some(0) | None => "/".to_owned(),
        Some(i) => request_path[..i].to_owned(),
    }
}

/// RFC 6265 5.1.4 path-match.
fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    request_path == cookie_path
        || (request_path.starts_with(cookie_path)
            && (cookie_path.ends_with('/')
                || request_path.as_bytes().get(cookie_path.len()) == Some(&b'/')))
}

// ============================================================================
// RequestFactory
// ============================================================================

/// `RequestFactory` — builds `axum::http::Request<Body>`
/// instances for direct handler / extractor tests, without dispatching
/// through a router and without the cookie persistence machinery
/// [`TestClient`] adds.
///
/// Use this when you want to call a handler function directly, or
/// build a request to pass into [`tower::ServiceExt::oneshot`] yourself
/// against a single component (a `tower::Layer`, an extractor).
/// For full router round-trips with cookie persistence, prefer
/// [`TestClient`].
///
/// ## Quick start
///
/// ```ignore
/// use rustango::test_client::RequestFactory;
///
/// let factory = RequestFactory::new();
///
/// // Plain GET
/// let req = factory.get("/api/posts").build();
///
/// // GET with query string and header
/// let req = factory
///     .get("/api/posts?author=42")
///     .header("authorization", "Bearer abc")
///     .build();
///
/// // POST with JSON body
/// let req = factory
///     .post("/api/posts")
///     .json(&serde_json::json!({"title": "Hello"}))
///     .build();
/// ```
///
/// ## Attaching extensions
///
/// For tests that bypass the auth layer and need to pre-populate the
/// request extensions (e.g. an extracted user), use
/// [`FactoryRequestBuilder::extension`]:
///
/// ```ignore
/// use rustango::test_client::RequestFactory;
///
/// struct MockUser { id: i64 }
///
/// let req = RequestFactory::new()
///     .get("/admin/dashboard")
///     .extension(MockUser { id: 42 })
///     .build();
/// // pass `req` to a handler / middleware directly
/// ```
#[derive(Default, Clone, Copy)]
pub struct RequestFactory;

impl RequestFactory {
    /// Construct a new factory. Stateless — clones are free.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Build a `GET` request to `path`.
    #[must_use]
    pub fn get(self, path: &str) -> FactoryRequestBuilder {
        FactoryRequestBuilder::new(Method::GET, path)
    }

    /// Build a `POST` request to `path`.
    #[must_use]
    pub fn post(self, path: &str) -> FactoryRequestBuilder {
        FactoryRequestBuilder::new(Method::POST, path)
    }

    /// Build a `PUT` request to `path`.
    #[must_use]
    pub fn put(self, path: &str) -> FactoryRequestBuilder {
        FactoryRequestBuilder::new(Method::PUT, path)
    }

    /// Build a `PATCH` request to `path`.
    #[must_use]
    pub fn patch(self, path: &str) -> FactoryRequestBuilder {
        FactoryRequestBuilder::new(Method::PATCH, path)
    }

    /// Build a `DELETE` request to `path`.
    #[must_use]
    pub fn delete(self, path: &str) -> FactoryRequestBuilder {
        FactoryRequestBuilder::new(Method::DELETE, path)
    }

    /// Build a `HEAD` request to `path`.
    #[must_use]
    pub fn head(self, path: &str) -> FactoryRequestBuilder {
        FactoryRequestBuilder::new(Method::HEAD, path)
    }

    /// Build an `OPTIONS` request to `path`.
    #[must_use]
    pub fn options(self, path: &str) -> FactoryRequestBuilder {
        FactoryRequestBuilder::new(Method::OPTIONS, path)
    }

    /// Build a `QUERY` request to `path` (RFC 10008).
    #[must_use]
    pub fn query(self, path: &str) -> FactoryRequestBuilder {
        FactoryRequestBuilder::new(crate::http_query::QUERY.clone(), path)
    }
}

/// Chained builder returned by [`RequestFactory`] verb methods. Finalize
/// with [`Self::build`] to get an `axum::http::Request<Body>`.
pub struct FactoryRequestBuilder {
    method: Method,
    path: String,
    headers: Vec<(HeaderName, HeaderValue)>,
    body: Body,
    content_type: Option<&'static str>,
    extensions: axum::http::Extensions,
}

impl FactoryRequestBuilder {
    fn new(method: Method, path: &str) -> Self {
        Self {
            method,
            path: path.to_owned(),
            headers: Vec::new(),
            body: Body::empty(),
            content_type: None,
            extensions: axum::http::Extensions::new(),
        }
    }

    /// Add a request header. Invalid header names / values are silently
    /// dropped — the typical test path provides valid ASCII.
    #[must_use]
    pub fn header(mut self, name: &str, value: &str) -> Self {
        if let (Ok(n), Ok(v)) = (HeaderName::try_from(name), HeaderValue::try_from(value)) {
            self.headers.push((n, v));
        }
        self
    }

    /// Set the body to a JSON-serialized value, plus `content-type:
    /// application/json`.
    #[must_use]
    pub fn json<T: serde::Serialize>(mut self, value: &T) -> Self {
        let bytes = serde_json::to_vec(value).unwrap_or_default();
        self.body = Body::from(bytes);
        self.content_type = Some("application/json");
        self
    }

    /// Set a form-encoded body, plus
    /// `content-type: application/x-www-form-urlencoded`. Field names
    /// and values are URL-encoded the same way [`TestClient`] does.
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

    /// Set a raw bytes / arbitrary body.
    #[must_use]
    pub fn body(mut self, body: impl Into<Body>) -> Self {
        self.body = body.into();
        self
    }

    /// Attach a typed extension to the request, mirroring axum's
    /// `Extension` extractor. Tests use this to pre-populate auth
    /// context, mock services, or per-request configuration.
    #[must_use]
    pub fn extension<T: Clone + Send + Sync + 'static>(mut self, value: T) -> Self {
        self.extensions.insert(value);
        self
    }

    /// Finalize and return the built request.
    ///
    /// # Panics
    /// Only if the configured path is not a valid URI — every other
    /// step is infallible. Keeping panic-on-build matches the test-only
    /// nature of this builder; production code should use
    /// `Request::builder()` directly.
    pub fn build(self) -> Request<Body> {
        let mut req = Request::builder().method(&self.method).uri(&self.path);
        if let Some(ct) = self.content_type {
            req = req.header("content-type", ct);
        }
        for (k, v) in self.headers {
            req = req.header(k, v);
        }
        let mut req = req.body(self.body).expect("invalid path / URI");
        *req.extensions_mut() = self.extensions;
        req
    }
}

/// Captured response from a test request.
pub struct TestResponse {
    pub status: u16,
    /// One value per name; a repeated header keeps its last value. Use
    /// [`Self::header_all`] or [`Self::header_map`] for every value.
    pub headers: HashMap<String, String>,
    /// Every response header, repeats included (e.g. `Set-Cookie`).
    pub header_map: axum::http::HeaderMap,
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
        let header_map = parts.headers;
        // Use a generous limit (16 MiB) for test responses
        let body = to_bytes(body, 16 * 1024 * 1024)
            .await
            .unwrap_or_default()
            .to_vec();
        Self {
            status,
            headers,
            header_map,
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

    /// Every value of a response header, in order (case-insensitive).
    #[must_use]
    pub fn header_all(&self, name: &str) -> Vec<&str> {
        self.header_map
            .get_all(name.to_ascii_lowercase().as_str())
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect()
    }
}

use crate::url_codec::url_encode;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_query::QueryRouterExt;
    use axum::routing::{get, post};
    use serde_json::json;

    fn app() -> Router {
        Router::new()
            .route("/hello", get(|| async { "hi" }))
            .route("/echo", post(|body: String| async move { body }))
            .route(
                "/search",
                get(|| async { "list" }).query(|body: String| async move { format!("q:{body}") }),
            )
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
    async fn query_builder_sends_query_method_with_body() {
        let c = TestClient::new(app());
        // GET and QUERY share the path; the QUERY builder hits the QUERY
        // handler and its body round-trips.
        let g = c.get("/search").send().await;
        assert_eq!(g.text(), "list");
        let r = c.query("/search").form(&[("q", "x")]).send().await;
        assert_eq!(r.status, 200);
        assert_eq!(r.text(), "q:q=x");
    }

    #[tokio::test]
    async fn request_factory_builds_query_request() {
        let req = RequestFactory::new().query("/search").build();
        assert_eq!(req.method().as_str(), "QUERY");
        assert_eq!(req.uri().path(), "/search");
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
            // headers — exactly the wire shape a browser sends.
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
    async fn max_age_zero_deletes_the_cookie() {
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
        // Gone because the server expired it, as in a browser.
        assert!(c.cookie("session").is_none());
        assert_eq!(c.cookie("csrftoken").as_deref(), Some("xyz"));
    }

    // ---------- browser parity (#1958) ----------

    fn csrf_app() -> Router {
        use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
        use axum::response::IntoResponse;

        async fn login() -> impl IntoResponse {
            let mut h = HeaderMap::new();
            h.insert(
                header::SET_COOKIE,
                HeaderValue::from_static("session=abc; Path=/; HttpOnly"),
            );
            (h, "form")
        }
        // Revokes only when the session reaches it, like a real logout.
        async fn logout(h: HeaderMap) -> impl IntoResponse {
            let has_session = h
                .get("cookie")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|c| c.contains("session=abc"));
            if !has_session {
                return (StatusCode::UNAUTHORIZED, HeaderMap::new(), "no session");
            }
            let mut out = HeaderMap::new();
            out.insert(
                header::SET_COOKIE,
                HeaderValue::from_static("session=; Path=/; Max-Age=0"),
            );
            (StatusCode::OK, out, "bye")
        }
        Router::new()
            .route("/login", get(login))
            .route("/submit", post(|| async { "saved" }))
            .route("/logout", post(logout))
            .layer(crate::forms::csrf::layer())
    }

    #[tokio::test]
    async fn csrf_protected_form_post_needs_the_token_like_a_browser() {
        let c = TestClient::new(csrf_app());
        assert_eq!(c.get("/login").send().await.status, 200);
        let token = c.csrf_token().expect("GET sets the CSRF cookie");

        let bare = c.post("/submit").form(&[("x", "1")]).send().await;
        assert_eq!(bare.status, 403, "no token is refused");

        let ok = c
            .post("/submit")
            .form(&[("x", "1"), ("_csrf", &token)])
            .send()
            .await;
        assert_eq!(ok.status, 200, "{}", ok.text());

        let forged = c
            .post("/submit")
            .header("origin", "http://evil.example")
            .form(&[("_csrf", &token)])
            .send()
            .await;
        assert_eq!(forged.status, 403, "a foreign Origin is refused");
    }

    #[tokio::test]
    async fn logout_reaches_the_server_with_session_and_token() {
        let c = TestClient::new(csrf_app());
        c.get("/login").send().await;
        assert!(c.cookie("session").is_some());
        let r = c.logout(Some("/logout")).await.expect("response");
        assert_eq!(r.status, 200, "{}", r.text());
        assert!(c.cookie("session").is_none(), "the server's expiry applied");
    }

    #[tokio::test]
    async fn unsafe_requests_carry_host_and_same_origin() {
        let app = Router::new().route(
            "/who",
            post(|h: axum::http::HeaderMap| async move {
                let get = |n: &str| {
                    h.get(n)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("-")
                        .to_owned()
                };
                format!("{} {}", get("host"), get("origin"))
            }),
        );
        let c = TestClient::new(app);
        assert_eq!(
            c.post("/who").send().await.text(),
            "testserver http://testserver"
        );
        let custom = c.post("/who").header("host", "acme.test").send().await;
        assert_eq!(custom.text(), "acme.test http://acme.test");
    }

    #[tokio::test]
    async fn handlers_see_a_peer_address() {
        use axum::extract::ConnectInfo;
        let app = Router::new().route(
            "/ip",
            get(|ConnectInfo(addr): ConnectInfo<SocketAddr>| async move { addr.ip().to_string() }),
        );
        let c = TestClient::new(app);
        let r = c.get("/ip").send().await;
        assert_eq!((r.status, r.text().as_str()), (200, "127.0.0.1"));
        let other = c
            .get("/ip")
            .remote_addr(SocketAddr::from(([10, 0, 0, 7], 1)))
            .send()
            .await;
        assert_eq!(other.text(), "10.0.0.7");
    }

    #[tokio::test]
    async fn repeated_set_cookie_headers_are_all_kept() {
        let c = TestClient::new(cookie_app());
        let r = c.post("/login").send().await;
        assert_eq!(r.header_all("Set-Cookie").len(), 2);
    }

    #[tokio::test]
    async fn redirects_keep_or_drop_the_method_like_a_browser() {
        use axum::http::{header, StatusCode};
        let app = Router::new()
            .route(
                "/temp",
                post(|| async {
                    (
                        StatusCode::TEMPORARY_REDIRECT,
                        [(header::LOCATION, "/dest")],
                    )
                }),
            )
            .route(
                "/seeother",
                post(|| async { (StatusCode::SEE_OTHER, [(header::LOCATION, "/dest")]) }),
            )
            .route(
                "/dest",
                get(|| async { "GET".to_owned() })
                    .post(|body: String| async move { format!("POST {body}") }),
            );
        let c = TestClient::new(app);
        let (kept, chain) = c
            .post("/temp")
            .body("payload")
            .send_following_redirects(3)
            .await;
        assert_eq!(kept.text(), "POST payload", "307 repeats method and body");
        assert_eq!(chain, vec![(307, "/dest".into()), (200, "/dest".into())]);
        let (dropped, _) = c
            .post("/seeother")
            .body("payload")
            .send_following_redirects(3)
            .await;
        assert_eq!(dropped.text(), "GET", "303 becomes GET");
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
    fn jar_honours_path_and_expiry() {
        let mut jar = CookieJar::default();
        jar.apply_set_cookie("a=1; Path=/admin", "/login");
        jar.apply_set_cookie("b=2", "/account/login");
        jar.apply_set_cookie("old=x; Expires=Wed, 21 Oct 2015 07:28:00 GMT", "/");
        assert_eq!(jar.header_for("/admin/users").as_deref(), Some("a=1"));
        assert_eq!(jar.header_for("/administrator"), None);
        assert_eq!(jar.header_for("/account/x").as_deref(), Some("b=2"));
        assert_eq!(jar.header_for("/"), None, "past Expires is never stored");
        jar.apply_set_cookie("a=; Path=/admin; Max-Age=0", "/");
        assert_eq!(jar.header_for("/admin"), None, "Max-Age=0 deletes");
    }

    #[test]
    fn redirect_locations_resolve_to_paths() {
        assert_eq!(
            resolve_location("/a/b", "http://testserver/x?y=1"),
            "/x?y=1"
        );
        assert_eq!(resolve_location("/a/b", "https://h"), "/");
        assert_eq!(resolve_location("/a/b?q", "c"), "/a/c");
        assert_eq!(resolve_location("/a/b", "/d"), "/d");
    }

    // ---------- get_following_redirects (issue #41 follow-up) ----------

    fn redirect_app() -> Router {
        use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
        use axum::response::IntoResponse;

        async fn old() -> impl IntoResponse {
            let mut h = HeaderMap::new();
            h.insert(header::LOCATION, HeaderValue::from_static("/middle"));
            (StatusCode::FOUND, h, "")
        }
        async fn middle() -> impl IntoResponse {
            let mut h = HeaderMap::new();
            h.insert(header::LOCATION, HeaderValue::from_static("/new"));
            (StatusCode::MOVED_PERMANENTLY, h, "")
        }
        async fn new_handler() -> impl IntoResponse {
            (StatusCode::OK, "final")
        }
        async fn loops() -> impl IntoResponse {
            let mut h = HeaderMap::new();
            h.insert(header::LOCATION, HeaderValue::from_static("/loop"));
            (StatusCode::FOUND, h, "")
        }
        async fn redirect_no_location() -> impl IntoResponse {
            // 3xx with no Location header — the follower should stop.
            (StatusCode::FOUND, "")
        }

        Router::new()
            .route("/old", get(old))
            .route("/middle", get(middle))
            .route("/new", get(new_handler))
            .route("/loop", get(loops))
            .route("/dangling", get(redirect_no_location))
            .route("/direct", get(|| async { "hi" }))
    }

    #[tokio::test]
    async fn follows_two_hop_chain_to_final_200() {
        let c = TestClient::new(redirect_app());
        let (final_res, chain) = c.get_following_redirects("/old", 5).await;
        assert_eq!(final_res.status, 200);
        assert_eq!(final_res.text(), "final");
        // chain = [(302, "/middle"), (301, "/new"), (200, "/new")]
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0], (302, "/middle".to_owned()));
        assert_eq!(chain[1], (301, "/new".to_owned()));
        assert_eq!(chain[2].0, 200);
        assert_eq!(chain[2].1, "/new");
    }

    #[tokio::test]
    async fn follow_no_op_when_first_response_is_200() {
        let c = TestClient::new(redirect_app());
        let (res, chain) = c.get_following_redirects("/direct", 5).await;
        assert_eq!(res.status, 200);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].0, 200);
    }

    #[tokio::test]
    async fn follow_stops_at_max_hops() {
        // /loop always redirects to itself. With max_hops=3, we
        // should follow exactly 3 hops then bail with the last 3xx.
        let c = TestClient::new(redirect_app());
        let (res, chain) = c.get_following_redirects("/loop", 3).await;
        // After 3 follows we ran out — the final response is still 3xx.
        assert_eq!(res.status, 302);
        // The chain holds the 3 hops plus the final unresolved-as-3xx.
        assert_eq!(chain.len(), 4);
        for hop in &chain[..3] {
            assert_eq!(hop.0, 302);
            assert_eq!(hop.1, "/loop");
        }
    }

    #[tokio::test]
    async fn follow_stops_when_3xx_has_no_location() {
        // /dangling: 302 without Location header. The follower
        // shouldn't try to "go" anywhere; it just returns that 3xx.
        let c = TestClient::new(redirect_app());
        let (res, chain) = c.get_following_redirects("/dangling", 5).await;
        assert_eq!(res.status, 302);
        // Only the initial dangling 302 is in the chain.
        assert_eq!(chain.len(), 1);
    }

    #[tokio::test]
    async fn follow_max_hops_zero_returns_first_response() {
        let c = TestClient::new(redirect_app());
        let (res, chain) = c.get_following_redirects("/old", 0).await;
        // No follows performed — first response surfaced as-is.
        assert_eq!(res.status, 302);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].1, "/old");
    }

    // ---------------- force_login (tenancy) ----------------

    #[cfg(feature = "tenancy")]
    fn user_with_id(id: i64) -> crate::tenancy::User {
        crate::tenancy::User {
            id: crate::sql::Auto::Set(id),
            ..crate::testkit::user()
        }
    }

    #[cfg(feature = "tenancy")]
    #[tokio::test]
    async fn force_login_tenant_user_writes_decodable_cookie() {
        use crate::tenancy::session::SessionSecret;
        use crate::tenancy::tenant_console::{decode, COOKIE_NAME};

        let secret = SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec());
        let c = TestClient::new(Router::new());
        c.force_login_tenant_user(&secret, "acme", &user_with_id(42), 3600);

        let cookie = c.cookie(COOKIE_NAME).expect("session cookie present");
        let payload = decode(&secret, "acme", &cookie).expect("cookie decodes");
        assert_eq!(payload.uid, 42);
        assert_eq!(payload.slug, "acme");
        assert!(!payload.is_impersonation());
    }

    #[cfg(feature = "tenancy")]
    #[tokio::test]
    async fn force_login_tenant_user_rejects_wrong_slug() {
        use crate::tenancy::session::SessionSecret;
        use crate::tenancy::tenant_console::decode;

        let secret = SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec());
        let c = TestClient::new(Router::new());
        c.force_login_tenant_user(&secret, "acme", &user_with_id(42), 3600);
        let cookie = c
            .cookie(crate::tenancy::tenant_console::COOKIE_NAME)
            .unwrap();

        // Cross-tenant replay fails — the slug binding holds.
        assert!(decode(&secret, "globex", &cookie).is_err());
    }

    #[cfg(feature = "tenancy")]
    #[tokio::test]
    async fn force_login_operator_writes_decodable_cookie() {
        use crate::tenancy::session::{decode, SessionSecret, COOKIE_NAME};

        let secret = SessionSecret::from_bytes(b"a-test-secret-thirty-two-bytes-x".to_vec());
        let c = TestClient::new(Router::new());
        let op = crate::tenancy::Operator {
            id: crate::sql::Auto::Set(7),
            username: "op".into(),
            password_hash: "$argon2id$test".into(),
            active: true,
            created_at: chrono::Utc::now(),
            password_changed_at: None,
            sessions_revoked_at: None,
        };
        c.force_login_operator(&secret, &op, 3600);

        let cookie = c.cookie(COOKIE_NAME).expect("operator cookie present");
        let payload = decode(&secret, &cookie).expect("cookie decodes");
        assert_eq!(payload.oid, 7);
    }

    /// An unsaved row panics rather than minting a cookie for id 0.
    #[cfg(feature = "tenancy")]
    #[test]
    #[should_panic(expected = "saved user row")]
    fn force_login_tenant_user_refuses_an_unsaved_user() {
        let secret = crate::tenancy::session::SessionSecret::from_bytes(vec![3u8; 32]);
        let c = TestClient::new(Router::new());
        c.force_login_tenant_user(&secret, "acme", &crate::testkit::user(), 3600);
    }

    #[cfg(feature = "tenancy")]
    #[tokio::test]
    async fn force_login_bad_secret_does_not_validate() {
        use crate::tenancy::session::SessionSecret;
        use crate::tenancy::tenant_console::decode;

        let mint_secret = SessionSecret::from_bytes(b"mint-secret-thirty-two-bytes-xxx".to_vec());
        let wrong_secret = SessionSecret::from_bytes(b"wrong-secret-thirty-two-bytes-xx".to_vec());

        let c = TestClient::new(Router::new());
        c.force_login_tenant_user(&mint_secret, "acme", &user_with_id(1), 3600);
        let cookie = c
            .cookie(crate::tenancy::tenant_console::COOKIE_NAME)
            .unwrap();

        assert!(decode(&wrong_secret, "acme", &cookie).is_err());
    }

    // -- RequestFactory --

    async fn read_body_bytes(req: Request<Body>) -> Vec<u8> {
        let (_, body) = req.into_parts();
        to_bytes(body, 64 * 1024).await.unwrap().to_vec()
    }

    #[tokio::test]
    async fn request_factory_get_emits_method_path_empty_body() {
        let req = RequestFactory::new().get("/api/posts").build();
        assert_eq!(req.method(), Method::GET);
        assert_eq!(req.uri().path(), "/api/posts");
        assert!(req.headers().is_empty(), "no headers on plain GET");
        let body = read_body_bytes(req).await;
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn request_factory_get_preserves_query_string() {
        let req = RequestFactory::new()
            .get("/api/posts?author=42&sort=desc")
            .build();
        assert_eq!(req.uri().path(), "/api/posts");
        assert_eq!(req.uri().query(), Some("author=42&sort=desc"));
    }

    #[tokio::test]
    async fn request_factory_post_with_json_sets_content_type_and_body() {
        let req = RequestFactory::new()
            .post("/api/posts")
            .json(&json!({"title": "Hi", "draft": true}))
            .build();
        assert_eq!(req.method(), Method::POST);
        assert_eq!(
            req.headers().get("content-type").unwrap().to_str().unwrap(),
            "application/json"
        );
        let body = read_body_bytes(req).await;
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["title"], "Hi");
        assert_eq!(parsed["draft"], true);
    }

    #[tokio::test]
    async fn request_factory_post_with_form_sets_content_type_and_url_encodes() {
        let req = RequestFactory::new()
            .post("/login")
            .form(&[("user", "alice & bob"), ("pw", "p@ss=word")])
            .build();
        assert_eq!(
            req.headers().get("content-type").unwrap().to_str().unwrap(),
            "application/x-www-form-urlencoded"
        );
        let body = String::from_utf8(read_body_bytes(req).await).unwrap();
        // Special chars are URL-encoded (matches the TestClient form
        // encoder so the two stay symmetric).
        assert!(
            body.contains("user=alice%20%26%20bob"),
            "missing encoded user, got: {body}"
        );
        assert!(
            body.contains("pw=p%40ss%3Dword"),
            "missing encoded pw, got: {body}"
        );
    }

    #[tokio::test]
    async fn request_factory_header_overrides_user_provided() {
        let req = RequestFactory::new()
            .get("/me")
            .header("authorization", "Bearer eyJabc")
            .header("x-custom", "v1")
            .build();
        assert_eq!(
            req.headers()
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer eyJabc"
        );
        assert_eq!(
            req.headers().get("x-custom").unwrap().to_str().unwrap(),
            "v1"
        );
    }

    #[derive(Debug, Clone, PartialEq)]
    struct MockUser {
        id: i64,
    }

    #[tokio::test]
    async fn request_factory_extension_attaches_typed_value() {
        let req = RequestFactory::new()
            .get("/admin/dashboard")
            .extension(MockUser { id: 42 })
            .build();
        let user = req.extensions().get::<MockUser>().expect("extension set");
        assert_eq!(user.id, 42);
    }

    #[tokio::test]
    async fn request_factory_all_verbs_round_trip() {
        let f = RequestFactory::new();
        assert_eq!(f.get("/x").build().method(), Method::GET);
        assert_eq!(f.post("/x").build().method(), Method::POST);
        assert_eq!(f.put("/x").build().method(), Method::PUT);
        assert_eq!(f.patch("/x").build().method(), Method::PATCH);
        assert_eq!(f.delete("/x").build().method(), Method::DELETE);
        assert_eq!(f.head("/x").build().method(), Method::HEAD);
        assert_eq!(f.options("/x").build().method(), Method::OPTIONS);
    }

    #[tokio::test]
    async fn request_factory_request_is_usable_with_oneshot() {
        // End-to-end shape: build a request via the factory, then
        // dispatch it directly through a Router via tower::oneshot.
        // That is what this helper exists for.
        let app = Router::new().route("/hello", axum::routing::get(|| async { "hi" }));
        let req = RequestFactory::new().get("/hello").build();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
    }
}
