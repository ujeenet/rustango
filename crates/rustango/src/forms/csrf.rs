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
    /// `Secure` cookie attribute.
    ///
    /// Defaults to `true` since v0.43 — the safe production default.
    /// Browsers will not send a `Secure` cookie over plain HTTP, so
    /// local development against `http://localhost:8080` needs an
    /// explicit opt-out:
    ///
    /// ```ignore
    /// rustango::forms::csrf::with_config(
    ///     CsrfConfig::default().allow_insecure_for_dev(),
    /// )
    /// ```
    ///
    /// `manage check --deploy` continues to warn when this is `false`
    /// on a prod tier.
    pub secure: bool,
}

impl Default for CsrfConfig {
    fn default() -> Self {
        Self {
            cookie_name: CSRF_COOKIE.to_owned(),
            header_name: CSRF_HEADER.to_owned(),
            secure: true,
        }
    }
}

impl CsrfConfig {
    /// Opt out of the `Secure` cookie attribute for local HTTP
    /// development. v0.43 — replaces the pre-v0.43 default of
    /// `secure: false`, which made dev easy but silently shipped to
    /// production unless the operator remembered to flip it.
    #[must_use]
    pub fn allow_insecure_for_dev(mut self) -> Self {
        self.secure = false;
        self
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
            //
            // BUT: skip the mint if the response already carries a
            // `Set-Cookie` for our cookie name. Form-rendering
            // handlers (`ensure_token` / `stamp_into_context`) also
            // mint when no cookie was present in the request — if
            // we appended ours on top, the browser would see two
            // Set-Cookies with the SAME name and DIFFERENT random
            // values, keep the last (ours), and the form would
            // POST a stale token from the handler's mint that no
            // longer matches the cookie. Detection by name-prefix
            // is cheap; we only inspect once per response.
            let mut response = inner.call(req).await?;
            if cookie_value.is_none() {
                let cookie_prefix = format!("{}=", cfg.cookie_name);
                let already_set = response
                    .headers()
                    .get_all(axum::http::header::SET_COOKIE)
                    .iter()
                    .any(|v| {
                        v.to_str()
                            .map(|s| s.starts_with(&cookie_prefix))
                            .unwrap_or(false)
                    });
                if !already_set {
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

// ============================================================== template helpers (issue #15)

/// Render the hidden `<input>` HTML that posts the token back to the
/// middleware. Use this when you already have the token string and
/// just need the form field. Pair with [`stamp_into_context`] for the
/// full handler-side flow.
///
/// ```ignore
/// let html = csrf_input_html("abc123");
/// assert_eq!(html, r#"<input type="hidden" name="_csrf" value="abc123">"#);
/// ```
#[must_use]
pub fn csrf_input_html(token: &str) -> String {
    let escaped = html_escape_attr(token);
    format!(r#"<input type="hidden" name="{CSRF_FORM_FIELD}" value="{escaped}">"#)
}

/// Tiny HTML-attribute escaper — sufficient for the token alphabet
/// (`A-Z`, `a-z`, `0-9`, `-`, `_` from base64url) but defensive in
/// case a caller passes a token from an unusual source. Avoids
/// pulling a full HTML-escape crate for the one-string case.
fn html_escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Read or mint a CSRF token and stamp it into the Tera context with
/// two keys callers can pick from: `csrf_token` (raw string, for SPA
/// meta tags or custom HTML) and `csrf_input` (pre-rendered hidden
/// input, ready for `{{ csrf_input | safe }}`). Returns the
/// optional `Set-Cookie` header value the caller should attach to
/// the response when a fresh token was minted (so the first GET
/// doesn't render an empty token).
///
/// Public counterpart of the private helper used by [`crate::template_views`]
/// — promoted so users with hand-rolled handlers don't re-implement
/// the cookie-mint dance.
///
/// ```ignore
/// async fn contact_form(headers: HeaderMap) -> Response {
///     let mut ctx = tera::Context::new();
///     let set_cookie = rustango::forms::csrf::stamp_into_context(&headers, &mut ctx);
///     let mut res = render(&tera, "contact.html", &ctx);
///     if let Some(c) = set_cookie {
///         if let Ok(h) = axum::http::HeaderValue::from_str(&c) {
///             res.headers_mut().append(axum::http::header::SET_COOKIE, h);
///         }
///     }
///     res
/// }
/// ```
///
/// Template usage:
///
/// ```jinja
/// <form method="POST">
///   {{ csrf_input | safe }}
///   …
/// </form>
///
/// <!-- SPA meta-tag variant: -->
/// <meta name="csrf-token" content="{{ csrf_token }}">
/// ```
#[cfg(feature = "template_views")]
#[must_use]
pub fn stamp_into_context(
    headers: &axum::http::HeaderMap,
    ctx: &mut tera::Context,
) -> Option<String> {
    let (token, set_cookie) = ensure_token(headers, CSRF_COOKIE);
    let html = csrf_input_html(&token);
    ctx.insert("csrf_token", &token);
    ctx.insert("csrf_input", &html);
    set_cookie
}

/// Register a `csrf_input` Tera filter that converts a token string
/// to the hidden-input HTML. Useful when a template has the raw
/// `csrf_token` in context (the existing `template_views` shape) but
/// wants the `<input>` shape without hand-writing it:
///
/// ```jinja
/// {{ csrf_token | csrf_input | safe }}
/// ```
///
/// Call at app setup, alongside any other Tera registration. Pair
/// with [`stamp_into_context`] for handlers that don't go through
/// the built-in `template_views` CBVs.
///
/// Non-string filter input passes through unchanged so the chain
/// doesn't blow up on accidental wiring.
#[cfg(feature = "template_views")]
pub fn register_csrf_filter(tera: &mut tera::Tera) {
    tera.register_filter("csrf_input", csrf_input_filter);
}

#[cfg(feature = "template_views")]
fn csrf_input_filter(
    value: &tera::Value,
    _: &std::collections::HashMap<String, tera::Value>,
) -> tera::Result<tera::Value> {
    if let Some(s) = value.as_str() {
        Ok(tera::Value::String(csrf_input_html(s)))
    } else {
        Ok(value.clone())
    }
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

    // ---- template helpers (issue #15) ----

    #[test]
    fn csrf_input_html_uses_form_field_constant() {
        let html = csrf_input_html("abc123");
        assert_eq!(html, r#"<input type="hidden" name="_csrf" value="abc123">"#);
        // The `name` attribute matches the middleware-expected constant.
        assert!(html.contains(&format!(r#"name="{CSRF_FORM_FIELD}""#)));
    }

    #[test]
    fn csrf_input_html_escapes_attribute_specials() {
        // Defensive escape — base64url tokens never contain these,
        // but a user could pass in something else by accident.
        let html = csrf_input_html(r#"x"<&>'"#);
        assert!(
            !html.contains(r#"x"<&>'"#),
            "raw specials must NOT survive: {html}"
        );
        assert!(html.contains("&quot;"), "{html}");
        assert!(html.contains("&lt;"), "{html}");
        assert!(html.contains("&amp;"), "{html}");
        assert!(html.contains("&gt;"), "{html}");
        assert!(html.contains("&#39;"), "{html}");
    }

    #[cfg(feature = "template_views")]
    #[test]
    fn stamp_into_context_stamps_both_keys_and_returns_set_cookie_on_fresh() {
        let headers = axum::http::HeaderMap::new(); // no cookie → fresh mint
        let mut ctx = tera::Context::new();
        let set_cookie = stamp_into_context(&headers, &mut ctx);

        assert!(set_cookie.is_some(), "fresh GET should return Set-Cookie");
        let json = ctx.into_json();
        let token = json
            .get("csrf_token")
            .and_then(|v| v.as_str())
            .expect("csrf_token stamped");
        let input_html = json
            .get("csrf_input")
            .and_then(|v| v.as_str())
            .expect("csrf_input stamped");
        assert!(!token.is_empty(), "minted token shouldn't be empty");
        assert!(
            input_html.contains(token),
            "csrf_input should embed the token: {input_html}"
        );
    }

    #[cfg(feature = "template_views")]
    #[test]
    fn stamp_into_context_reuses_existing_cookie_no_set_cookie() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_static("rustango_csrf=fixed_value"),
        );
        let mut ctx = tera::Context::new();
        let set_cookie = stamp_into_context(&headers, &mut ctx);

        assert!(set_cookie.is_none(), "existing cookie should not re-mint");
        let json = ctx.into_json();
        assert_eq!(
            json.get("csrf_token").and_then(|v| v.as_str()),
            Some("fixed_value")
        );
    }

    #[cfg(feature = "template_views")]
    #[test]
    fn csrf_input_filter_converts_token_to_html() {
        let mut tera = tera::Tera::default();
        register_csrf_filter(&mut tera);
        tera.add_raw_template("_", "{{ csrf_token | csrf_input | safe }}")
            .unwrap();

        let mut ctx = tera::Context::new();
        ctx.insert("csrf_token", "tok-abc");
        let out = tera.render("_", &ctx).unwrap();
        assert_eq!(out, r#"<input type="hidden" name="_csrf" value="tok-abc">"#);
    }

    #[cfg(feature = "template_views")]
    #[test]
    fn csrf_input_filter_passes_through_non_string_values() {
        let mut tera = tera::Tera::default();
        register_csrf_filter(&mut tera);
        // Number → filter passes through, Tera renders as the number.
        tera.add_raw_template("_", "{{ n | csrf_input }}").unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("n", &42_i64);
        assert_eq!(tera.render("_", &ctx).unwrap(), "42");
    }

    /// CMS issue #278 — the form-rendering helper (`stamp_into_context`)
    /// and this middleware both mint a token when the request arrives
    /// without a CSRF cookie. If both append `Set-Cookie`, the browser
    /// keeps the later one (the middleware's) while the form was
    /// rendered with the handler's token — every subsequent POST then
    /// 403s as "CSRF token missing or mismatched". The middleware now
    /// skips its own mint when the inner handler already set a
    /// `Set-Cookie` for the configured cookie name. Regression guard.
    #[tokio::test]
    async fn middleware_skips_mint_when_response_already_has_cookie() {
        use tower::{Service, ServiceExt};
        let layer = with_config(CsrfConfig::default());
        let inner = tower::service_fn(|_req: Request<Body>| async {
            // Stand-in for `stamp_into_context`: mint + set our own.
            let mut resp = Response::new(Body::empty());
            resp.headers_mut().append(
                axum::http::header::SET_COOKIE,
                HeaderValue::from_static("rustango_csrf=handler-token; Path=/; SameSite=Lax"),
            );
            Ok::<_, Infallible>(resp)
        });
        let mut svc = tower::Layer::layer(&layer, inner);
        let req = Request::builder()
            .method(Method::GET)
            .body(Body::empty())
            .unwrap();
        let resp = svc.ready().await.unwrap().call(req).await.unwrap();
        let csrf_cookies: Vec<String> = resp
            .headers()
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter(|s| s.starts_with("rustango_csrf="))
            .map(str::to_owned)
            .collect();
        assert_eq!(
            csrf_cookies.len(),
            1,
            "expected exactly one rustango_csrf Set-Cookie (handler's), got: {csrf_cookies:?}",
        );
        assert!(
            csrf_cookies[0].starts_with("rustango_csrf=handler-token"),
            "middleware overwrote handler's mint: {}",
            csrf_cookies[0],
        );
    }

    /// Symmetric to the previous test: when the inner handler does NOT
    /// set the cookie (e.g. a non-form view), the middleware still has
    /// to seed it so the next safe-method GET doesn't have to.
    #[tokio::test]
    async fn middleware_mints_cookie_when_handler_didnt() {
        use tower::{Service, ServiceExt};
        let layer = with_config(CsrfConfig::default());
        let inner = tower::service_fn(|_req: Request<Body>| async {
            // No Set-Cookie — the canonical case for an API/JSON view.
            Ok::<_, Infallible>(Response::new(Body::empty()))
        });
        let mut svc = tower::Layer::layer(&layer, inner);
        let req = Request::builder()
            .method(Method::GET)
            .body(Body::empty())
            .unwrap();
        let resp = svc.ready().await.unwrap().call(req).await.unwrap();
        let csrf_cookies: Vec<String> = resp
            .headers()
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter(|s| s.starts_with("rustango_csrf="))
            .map(str::to_owned)
            .collect();
        assert_eq!(
            csrf_cookies.len(),
            1,
            "middleware should seed exactly one cookie when none was set"
        );
    }
}
