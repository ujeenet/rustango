//! CSRF middleware (slice 8.4C) — double-submit-cookie strategy.
//!
//! On safe methods (`GET`, `HEAD`, `OPTIONS`, `TRACE`) the layer is
//! a pass-through that ensures a fresh CSRF token cookie is set on
//! the response. On unsafe methods (`POST`, `PUT`, `PATCH`,
//! `DELETE`) the layer enforces that the request carries an
//! `X-CSRF-Token` header (or `_csrf` form field, when the body is
//! `application/x-www-form-urlencoded`) whose value matches the
//! `rustango_csrf` cookie. Mismatch / missing → `403 Forbidden`.
//!
//! The cookie is `HttpOnly = false` (the SPA / form code MUST be
//! able to read it), `SameSite = Lax`, `Secure` when the URL
//! scheme is `https`. Token is 32 bytes of `OsRng` rendered as
//! URL-safe base64 (no padding).
//!
//! Wire it as an axum layer:
//!
//! ```ignore
//! use rustango::forms::csrf;
//! let app = Router::new()
//!     .route("/items", post(create_item))
//!     .layer(csrf::layer());
//! ```
//!
//! The auto-admin auto-mounts this layer when the `admin` feature
//! is on; user route handlers that POST forms must apply it
//! themselves (or use a top-level `Router::layer(csrf::layer())`).
//!
//! Gated by the `csrf` feature (in `default` via `admin`). Drop the
//! `admin` feature to skip both the middleware code and the cookie
//! / rand / base64 deps.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, Response, StatusCode};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rand::RngCore;
use tower::Service;

/// Default cookie name set by the CSRF middleware. Distinct from
/// the tenancy session cookies (`rustango_session` /
/// `rustango_tenant_session`) so the two flows don't collide.
/// Public so view code that wants to read or mint a token via
/// [`ensure_token`] can use the canonical name without re-typing
/// it.
pub const CSRF_COOKIE: &str = "rustango_csrf";

/// HTTP header the middleware looks for on unsafe requests. SPAs
/// echo the cookie value here (the standard double-submit pattern).
const CSRF_HEADER: &str = "X-CSRF-Token";

/// Form-field name the middleware looks for on
/// `application/x-www-form-urlencoded` bodies. Matches Django's
/// `csrfmiddlewaretoken` semantics, renamed for rustango.
pub const CSRF_FORM_FIELD: &str = "_csrf";

/// Create the CSRF middleware as a tower [`Layer`].
///
/// Defaults are sensible: 32-byte tokens, Lax SameSite, HttpOnly
/// off (the SPA must read the cookie). Override via [`CsrfConfig`]
/// + [`with_config`].
pub fn layer() -> CsrfLayer {
    CsrfLayer::new(CsrfConfig::default())
}

/// Create the middleware with explicit config — used by integrators
/// who need a different cookie name (e.g. when stacking against a
/// different framework on the same host).
pub fn with_config(cfg: CsrfConfig) -> CsrfLayer {
    CsrfLayer::new(cfg)
}

/// Configuration for the CSRF layer. All fields have sensible
/// defaults; override only what diverges.
#[derive(Debug, Clone)]
pub struct CsrfConfig {
    /// Cookie name. Default `"rustango_csrf"`.
    pub cookie_name: String,
    /// Header name the middleware accepts on unsafe methods. Default
    /// `"X-CSRF-Token"`.
    pub header_name: String,
    /// `Secure` cookie attribute. Default `false` so dev over HTTP
    /// works; flip to `true` in production.
    pub secure: bool,
}

impl Default for CsrfConfig {
    fn default() -> Self {
        Self {
            cookie_name: CSRF_COOKIE.to_owned(),
            header_name: CSRF_HEADER.to_owned(),
            secure: false,
        }
    }
}

/// The tower [`Layer`] implementation. Wraps inner services with
/// [`CsrfService`].
#[derive(Clone)]
pub struct CsrfLayer {
    cfg: Arc<CsrfConfig>,
}

impl CsrfLayer {
    fn new(cfg: CsrfConfig) -> Self {
        Self { cfg: Arc::new(cfg) }
    }
}

impl<S> tower::Layer<S> for CsrfLayer {
    type Service = CsrfService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        CsrfService {
            inner,
            cfg: Arc::clone(&self.cfg),
        }
    }
}

/// The wrapped service. Validates unsafe-method requests, ensures
/// safe-method responses carry a CSRF cookie.
#[derive(Clone)]
pub struct CsrfService<S> {
    inner: S,
    cfg: Arc<CsrfConfig>,
}

impl<S> Service<Request<Body>> for CsrfService<S>
where
    S: Service<Request<Body>, Response = Response<Body>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn std::future::Future<Output = Result<Response<Body>, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let cfg = Arc::clone(&self.cfg);
        let mut inner = self.inner.clone();
        Box::pin(async move {
            let cookie_value = read_csrf_cookie(&req, &cfg.cookie_name);

            // Enforce on unsafe methods.
            let req = if !is_safe_method(req.method()) {
                let header_value = req
                    .headers()
                    .get(&cfg.header_name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                if let Some(h) = header_value {
                    // Header path — short-circuit, no body buffering.
                    let token_match = match &cookie_value {
                        Some(c) => constant_time_eq(c.as_bytes(), h.as_bytes()),
                        None => false,
                    };
                    if !token_match {
                        return Ok(forbid_response("CSRF token missing or mismatched"));
                    }
                    req
                } else if is_form_encoded(&req) {
                    // Form-encoded POST without the header — read
                    // `_csrf` from the body. Body is consumed once,
                    // so we buffer + replace so the inner handler
                    // can still parse it.
                    let (parts, body) = req.into_parts();
                    let bytes = match axum::body::to_bytes(body, BODY_BUFFER_LIMIT).await {
                        Ok(b) => b,
                        Err(_) => {
                            return Ok(forbid_response("CSRF: form body exceeded buffer limit"));
                        }
                    };
                    let form_token = read_form_field(&bytes, CSRF_FORM_FIELD);
                    let token_match = match (&cookie_value, &form_token) {
                        (Some(c), Some(f)) => constant_time_eq(c.as_bytes(), f.as_bytes()),
                        _ => false,
                    };
                    if !token_match {
                        return Ok(forbid_response("CSRF token missing or mismatched"));
                    }
                    Request::from_parts(parts, Body::from(bytes))
                } else {
                    // Neither header nor form body — reject.
                    return Ok(forbid_response("CSRF token missing or mismatched"));
                }
            } else {
                req
            };

            // Pass to inner. After the response comes back, ensure
            // the CSRF cookie is set so the next safe-method GET
            // doesn't have to seed it.
            let mut response = inner.call(req).await?;
            if cookie_value.is_none() {
                let token = mint_token();
                let cookie_str = format!(
                    "{}={token}; Path=/; SameSite=Lax{}",
                    cfg.cookie_name,
                    if cfg.secure { "; Secure" } else { "" }
                );
                if let Ok(hv) = HeaderValue::from_str(&cookie_str) {
                    response
                        .headers_mut()
                        .append(axum::http::header::SET_COOKIE, hv);
                }
            }
            Ok(response)
        })
    }
}

/// Cap the form-body buffer the CSRF middleware will consume
/// while extracting the `_csrf` field. 64 KiB is generous for any
/// realistic HTML form (typical forms are < 4 KiB; file uploads
/// don't use form-encoded bodies). Bodies larger than this 403
/// — the middleware can't safely buffer megabyte-scale form
/// payloads in memory just to verify a token.
const BODY_BUFFER_LIMIT: usize = 64 * 1024;

fn is_safe_method(m: &Method) -> bool {
    matches!(
        *m,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
    )
}

fn read_csrf_cookie(req: &Request<Body>, name: &str) -> Option<String> {
    read_csrf_cookie_from_headers(req.headers(), name)
}

fn read_csrf_cookie_from_headers(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some((k, v)) = part.split_once('=') {
            if k == name {
                return Some(v.to_owned());
            }
        }
    }
    None
}

/// `true` when the request body is `application/x-www-form-urlencoded`
/// — the only format the CSRF middleware knows how to scan for the
/// `_csrf` field. Multipart uploads use a different MIME and are
/// expected to send the token via the `X-CSRF-Token` header.
fn is_form_encoded(req: &Request<Body>) -> bool {
    let Some(ct) = req
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    // Strip any `; charset=...` suffix and lowercase-compare.
    let head = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    head == "application/x-www-form-urlencoded"
}

/// Tiny form-encoded parser scoped to extracting one named field.
/// Accepts `key=value&key2=value2` shape, percent-decoded, with
/// `+` treated as space (the form-encoded convention). Returns
/// the FIRST matching field's value — duplicates are rare in
/// well-formed forms.
fn read_form_field(body: &[u8], name: &str) -> Option<String> {
    let s = std::str::from_utf8(body).ok()?;
    for pair in s.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let key = percent_decode(k.replace('+', " ").as_bytes())?;
        if key == name {
            return percent_decode(v.replace('+', " ").as_bytes());
        }
    }
    None
}

/// Minimal RFC 3986 percent-decoder used by the form-field parser.
/// Returns `None` on malformed `%xx` sequences (rejects rather
/// than truncating).
fn percent_decode(bytes: &[u8]) -> Option<String> {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = hex_digit(bytes[i + 1])?;
            let lo = hex_digit(bytes[i + 2])?;
            out.push(hi * 16 + lo);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Generate a fresh 32-byte token, base64url-encoded (no padding).
fn mint_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Read the existing CSRF cookie from request headers, or mint a
/// fresh token. Returns `(token, Some(set_cookie_header_value))`
/// when the cookie was missing — caller is responsible for adding
/// the `Set-Cookie` header to the response so the template's
/// `<input value="{{ csrf_token }}">` matches what the browser
/// sends back on POST. Returns `(token, None)` when the cookie was
/// already present (no `Set-Cookie` needed).
///
/// Usage from a Tera-rendering view (template_views, custom HTML
/// handlers):
///
/// ```ignore
/// let (token, set_cookie) = rustango::forms::csrf::ensure_token(
///     req_headers,
///     rustango::forms::csrf::CSRF_COOKIE,
/// );
/// ctx.insert("csrf_token", &token);
/// let mut resp = render_template(...);
/// if let Some(c) = set_cookie {
///     resp.headers_mut().append(
///         axum::http::header::SET_COOKIE,
///         axum::http::HeaderValue::from_str(&c).unwrap(),
///     );
/// }
/// ```
///
/// Without this helper, the first-ever GET to a form view would
/// render with an empty `csrf_token`, the user's POST would fail
/// CSRF validation, and only the second attempt would succeed.
/// `ensure_token` makes the first GET idempotent.
#[must_use]
pub fn ensure_token(
    headers: &axum::http::HeaderMap,
    cookie_name: &str,
) -> (String, Option<String>) {
    if let Some(existing) = read_csrf_cookie_from_headers(headers, cookie_name) {
        return (existing, None);
    }
    let token = mint_token();
    let cookie = format!("{cookie_name}={token}; Path=/; SameSite=Lax");
    (token, Some(cookie))
}

/// Constant-time byte-slice equality. Avoids a leaky `==` even
/// though the bodies of the comparison aren't really secret in this
/// scheme — best practice.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn forbid_response(detail: &'static str) -> Response<Body> {
    let mut response = Response::new(Body::from(detail));
    *response.status_mut() = StatusCode::FORBIDDEN;
    response
        .headers_mut()
        .insert("Content-Type", HeaderValue::from_static("text/plain"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_method_predicate() {
        assert!(is_safe_method(&Method::GET));
        assert!(is_safe_method(&Method::HEAD));
        assert!(is_safe_method(&Method::OPTIONS));
        assert!(!is_safe_method(&Method::POST));
        assert!(!is_safe_method(&Method::PUT));
        assert!(!is_safe_method(&Method::DELETE));
    }

    #[test]
    fn ct_eq_matches_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn mint_token_is_base64url_no_pad() {
        let t = mint_token();
        // 32 bytes → 43 base64 chars (no padding).
        assert_eq!(t.len(), 43);
        assert!(!t.contains('='));
        assert!(URL_SAFE_NO_PAD.decode(t.as_bytes()).is_ok());
    }

    #[test]
    fn read_csrf_cookie_finds_named_pair() {
        use axum::http::Request;
        let req = Request::builder()
            .header("cookie", "session=abc; rustango_csrf=hello; theme=dark")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            read_csrf_cookie(&req, "rustango_csrf").as_deref(),
            Some("hello")
        );
        assert_eq!(read_csrf_cookie(&req, "missing").as_deref(), None);
    }

    #[test]
    fn read_csrf_cookie_returns_none_when_no_header() {
        use axum::http::Request;
        let req = Request::builder().body(Body::empty()).unwrap();
        assert_eq!(read_csrf_cookie(&req, "anything"), None);
    }

    /// `is_form_encoded` recognizes the canonical content type +
    /// the `; charset=utf-8` variant browsers sometimes append.
    /// Other types (multipart, JSON) return false.
    #[test]
    fn is_form_encoded_recognizes_canonical_and_charset_variants() {
        use axum::http::Request;
        let req = Request::builder()
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::empty())
            .unwrap();
        assert!(is_form_encoded(&req));

        let req = Request::builder()
            .header(
                "content-type",
                "application/x-www-form-urlencoded; charset=UTF-8",
            )
            .body(Body::empty())
            .unwrap();
        assert!(is_form_encoded(&req));

        let req = Request::builder()
            .header("content-type", "multipart/form-data; boundary=---")
            .body(Body::empty())
            .unwrap();
        assert!(!is_form_encoded(&req));

        let req = Request::builder()
            .header("content-type", "application/json")
            .body(Body::empty())
            .unwrap();
        assert!(!is_form_encoded(&req));

        // No content-type header: false.
        let req = Request::builder().body(Body::empty()).unwrap();
        assert!(!is_form_encoded(&req));
    }

    /// `read_form_field` extracts the named field, percent-decoded,
    /// `+` → space.
    #[test]
    fn read_form_field_extracts_named_value() {
        let body = b"foo=bar&_csrf=tok123&other=baz";
        assert_eq!(read_form_field(body, "_csrf").as_deref(), Some("tok123"));
        assert_eq!(read_form_field(body, "foo").as_deref(), Some("bar"));
        assert_eq!(read_form_field(body, "missing"), None);
    }

    #[test]
    fn read_form_field_percent_decodes_value() {
        let body = b"_csrf=abc%2Fxyz";
        assert_eq!(read_form_field(body, "_csrf").as_deref(), Some("abc/xyz"));
    }

    #[test]
    fn read_form_field_treats_plus_as_space() {
        let body = b"q=hello+world";
        assert_eq!(read_form_field(body, "q").as_deref(), Some("hello world"));
    }

    #[test]
    fn read_form_field_returns_none_for_malformed_pairs() {
        // Pair without `=` sign — skipped.
        let body = b"foo&_csrf=tok";
        assert_eq!(read_form_field(body, "_csrf").as_deref(), Some("tok"));
        assert_eq!(read_form_field(body, "foo"), None);
    }

    #[test]
    fn percent_decode_rejects_malformed() {
        assert!(percent_decode(b"%2").is_none()); // truncated
        assert!(percent_decode(b"%ZZ").is_none()); // non-hex
        assert_eq!(percent_decode(b"plain").as_deref(), Some("plain"));
        assert_eq!(percent_decode(b"a%20b").as_deref(), Some("a b"));
    }
}
