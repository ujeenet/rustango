//! Per-request CSP nonce middleware.
//!
//! A strict Content-Security-Policy blocks inline `<script>` tags by
//! default. The standards-blessed escape hatch is per-request nonces:
//! the server generates a random token, drops it into the CSP header
//! (`script-src 'nonce-XYZ'`), and renders the same token onto every
//! inline script tag (`<script nonce="XYZ">`). The browser only runs
//! tags whose nonce matches.
//!
//! ## Wire-up
//!
//! ```ignore
//! use rustango::csp_nonce::{CspNonceLayer, CspNonceRouterExt, Nonce};
//! use rustango::security_headers::{SecurityHeadersLayer, CspBuilder};
//! use rustango::csp_nonce::CSP_NONCE_PLACEHOLDER;
//!
//! // 1. Reference the placeholder in your CSP. The middleware
//! //    substitutes the real nonce per-request.
//! let csp = CspBuilder::strict_starter()
//!     .script_src(&["'self'", CSP_NONCE_PLACEHOLDER])
//!     .style_src(&["'self'", CSP_NONCE_PLACEHOLDER])
//!     .build();
//!
//! let app = axum::Router::new()
//!     .route("/", axum::routing::get(home))
//!     .security_headers(SecurityHeadersLayer::strict().csp(csp))
//!     .csp_nonce(CspNonceLayer::default());
//!
//! // 2. Read the nonce in your handler so you can render it.
//! async fn home(axum::Extension(nonce): axum::Extension<Nonce>) -> axum::response::Html<String> {
//!     axum::response::Html(format!(
//!         "<script nonce=\"{}\">console.log('hi')</script>",
//!         nonce.value()
//!     ))
//! }
//! ```
//!
//! ## How substitution works
//!
//! After the handler runs, the middleware looks at the response's
//! `content-security-policy` (or `content-security-policy-report-only`)
//! header. If it contains the literal string [`CSP_NONCE_PLACEHOLDER`]
//! (`'nonce-__RUSTANGO_NONCE__'`), the middleware replaces every
//! occurrence with the per-request nonce. Otherwise the header is left
//! untouched — non-HTML responses get their CSP exactly as configured.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::header::{HeaderName, HeaderValue};
use axum::http::Response;
use axum::middleware::Next;
use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::{rngs::OsRng, RngCore};

/// Placeholder string that should appear in your CSP wherever you want
/// the per-request nonce to be substituted. Use it inside the source
/// list of `script-src` / `style-src` (etc.):
///
/// ```ignore
/// CspBuilder::strict_starter()
///     .script_src(&["'self'", CSP_NONCE_PLACEHOLDER])
/// ```
///
/// The full string emitted into the CSP is `'nonce-XYZ'`, so the
/// placeholder includes the surrounding quotes and the `nonce-` prefix.
pub const CSP_NONCE_PLACEHOLDER: &str = "'nonce-__RUSTANGO_NONCE__'";

const PLACEHOLDER_TOKEN: &str = "__RUSTANGO_NONCE__";

/// Per-request nonce. Stored in `request.extensions` by the middleware;
/// pull it out via `axum::Extension<Nonce>` in your handler.
#[derive(Debug, Clone)]
pub struct Nonce {
    value: Arc<String>,
}

impl Nonce {
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    #[must_use]
    pub fn into_string(self) -> String {
        Arc::try_unwrap(self.value).unwrap_or_else(|arc| (*arc).clone())
    }
}

/// CSP nonce middleware configuration.
#[derive(Clone, Debug)]
pub struct CspNonceLayer {
    /// Length (in bytes) of the random material before base64 encoding.
    /// 16 bytes (128 bits) is the OWASP recommendation. Default: 16.
    pub bytes: usize,
}

impl Default for CspNonceLayer {
    fn default() -> Self {
        Self { bytes: 16 }
    }
}

impl CspNonceLayer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the nonce byte length. Min 8, max 64.
    #[must_use]
    pub fn bytes(mut self, n: usize) -> Self {
        self.bytes = n.clamp(8, 64);
        self
    }
}

pub trait CspNonceRouterExt {
    #[must_use]
    fn csp_nonce(self, layer: CspNonceLayer) -> Self;
}

impl<S: Clone + Send + Sync + 'static> CspNonceRouterExt for Router<S> {
    fn csp_nonce(self, layer: CspNonceLayer) -> Self {
        let cfg = Arc::new(layer);
        self.layer(axum::middleware::from_fn(
            move |mut req: Request<Body>, next: Next| {
                let cfg = cfg.clone();
                async move {
                    let nonce = Nonce {
                        value: Arc::new(generate_nonce(cfg.bytes)),
                    };
                    req.extensions_mut().insert(nonce.clone());
                    let mut response = next.run(req).await;
                    substitute_nonce(&mut response, nonce.value());
                    response
                }
            },
        ))
    }
}

fn generate_nonce(byte_len: usize) -> String {
    let mut buf = vec![0u8; byte_len];
    // v0.30.12 — use OsRng directly. Predictable nonces would
    // defeat the strict-CSP `script-src 'nonce-...'` model;
    // OsRng matches the rest of the framework's crypto sites.
    OsRng.fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(&buf)
}

fn substitute_nonce(response: &mut Response<Body>, nonce: &str) {
    for name in [
        "content-security-policy",
        "content-security-policy-report-only",
    ] {
        let Ok(name) = HeaderName::try_from(name) else {
            continue;
        };
        let Some(existing) = response.headers().get(&name) else {
            continue;
        };
        let Ok(s) = existing.to_str() else { continue };
        if !s.contains(PLACEHOLDER_TOKEN) {
            continue;
        }
        let replaced = s.replace(PLACEHOLDER_TOKEN, nonce);
        if let Ok(hv) = HeaderValue::from_str(&replaced) {
            response.headers_mut().insert(name, hv);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::HeaderValue;
    use axum::http::Request;
    use axum::routing::get;
    use axum::Extension;
    use tower::ServiceExt;

    #[test]
    fn placeholder_format_includes_nonce_prefix_and_quotes() {
        // Sanity check — used directly in user CSPs.
        assert_eq!(CSP_NONCE_PLACEHOLDER, "'nonce-__RUSTANGO_NONCE__'");
    }

    #[test]
    fn nonce_bytes_is_clamped() {
        assert_eq!(CspNonceLayer::new().bytes(0).bytes, 8);
        assert_eq!(CspNonceLayer::new().bytes(1000).bytes, 64);
        assert_eq!(CspNonceLayer::new().bytes(20).bytes, 20);
    }

    #[test]
    fn generated_nonce_is_url_safe_base64() {
        let n = generate_nonce(16);
        assert!(n
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        // 16 raw bytes -> ceil(16 * 4 / 3) = 22 chars (no padding).
        assert_eq!(n.len(), 22);
    }

    #[test]
    fn each_nonce_is_unique() {
        let a = generate_nonce(16);
        let b = generate_nonce(16);
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn handler_can_read_nonce_via_extension() {
        async fn h(Extension(nonce): Extension<Nonce>) -> String {
            nonce.value().to_owned()
        }
        let app = Router::new()
            .route("/", get(h))
            .csp_nonce(CspNonceLayer::default());
        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        // Each call generates a fresh 22-char token.
        assert_eq!(body.len(), 22);
    }

    #[tokio::test]
    async fn nonce_substituted_into_csp_header() {
        async fn h() -> ([(&'static str, &'static str); 1], &'static str) {
            (
                [(
                    "content-security-policy",
                    "script-src 'self' 'nonce-__RUSTANGO_NONCE__'",
                )],
                "ok",
            )
        }
        let app = Router::new()
            .route("/", get(h))
            .csp_nonce(CspNonceLayer::default());
        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let csp = resp
            .headers()
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            !csp.contains("__RUSTANGO_NONCE__"),
            "placeholder should be replaced"
        );
        assert!(csp.contains("'nonce-"), "rendered nonce should be present");
    }

    #[tokio::test]
    async fn nonce_substituted_into_report_only_csp_too() {
        async fn h() -> ([(&'static str, &'static str); 1], &'static str) {
            (
                [(
                    "content-security-policy-report-only",
                    "script-src 'nonce-__RUSTANGO_NONCE__'",
                )],
                "ok",
            )
        }
        let app = Router::new()
            .route("/", get(h))
            .csp_nonce(CspNonceLayer::default());
        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let csp = resp
            .headers()
            .get("content-security-policy-report-only")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(!csp.contains("__RUSTANGO_NONCE__"));
    }

    #[tokio::test]
    async fn csp_without_placeholder_is_untouched() {
        async fn h() -> ([(&'static str, &'static str); 1], &'static str) {
            ([("content-security-policy", "script-src 'self'")], "ok")
        }
        let app = Router::new()
            .route("/", get(h))
            .csp_nonce(CspNonceLayer::default());
        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            resp.headers()
                .get("content-security-policy")
                .unwrap()
                .to_str()
                .unwrap(),
            "script-src 'self'"
        );
    }

    #[tokio::test]
    async fn csp_substitutes_consistently_with_handler_nonce() {
        // The same nonce should appear in BOTH the header AND whatever
        // the handler rendered into the body.
        async fn h(Extension(nonce): Extension<Nonce>) -> ([(HeaderName, HeaderValue); 1], String) {
            let csp = format!("script-src 'nonce-{}'", "__RUSTANGO_NONCE__");
            (
                [(
                    HeaderName::from_static("content-security-policy"),
                    HeaderValue::from_str(&csp).unwrap(),
                )],
                format!("nonce={}", nonce.value()),
            )
        }
        let app = Router::new()
            .route("/", get(h))
            .csp_nonce(CspNonceLayer::default());
        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let csp = resp
            .headers()
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        let body_nonce = body.strip_prefix("nonce=").unwrap();
        assert!(
            csp.contains(&format!("'nonce-{body_nonce}'")),
            "header CSP should embed the same nonce the handler saw\nCSP: {csp}\nbody: {body}"
        );
    }
}
