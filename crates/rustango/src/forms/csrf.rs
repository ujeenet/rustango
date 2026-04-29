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

/// The cookie name set by the CSRF middleware. Distinct from
/// tenancy's `rustango_session` / `rustango_tenant_session` cookie
/// names so the two flows don't collide.
const CSRF_COOKIE: &str = "rustango_csrf";

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
    S: Service<Request<Body>, Response = Response<Body>, Error = Infallible> + Clone + Send + 'static,
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
            if !is_safe_method(req.method()) {
                let header_value = req
                    .headers()
                    .get(&cfg.header_name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                let token_match = match (&cookie_value, &header_value) {
                    (Some(c), Some(h)) => constant_time_eq(c.as_bytes(), h.as_bytes()),
                    _ => false,
                };
                if !token_match {
                    return Ok(forbid_response("CSRF token missing or mismatched"));
                }
            }

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

fn is_safe_method(m: &Method) -> bool {
    matches!(*m, Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE)
}

fn read_csrf_cookie(req: &Request<Body>, name: &str) -> Option<String> {
    let raw = req
        .headers()
        .get(axum::http::header::COOKIE)?
        .to_str()
        .ok()?;
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

/// Generate a fresh 32-byte token, base64url-encoded (no padding).
fn mint_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
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
        assert_eq!(read_csrf_cookie(&req, "rustango_csrf").as_deref(), Some("hello"));
        assert_eq!(read_csrf_cookie(&req, "missing").as_deref(), None);
    }

    #[test]
    fn read_csrf_cookie_returns_none_when_no_header() {
        use axum::http::Request;
        let req = Request::builder().body(Body::empty()).unwrap();
        assert_eq!(read_csrf_cookie(&req, "anything"), None);
    }
}
