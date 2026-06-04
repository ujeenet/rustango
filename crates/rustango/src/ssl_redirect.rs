//! HTTP → HTTPS redirect middleware — Django parity for
//! `SECURE_SSL_REDIRECT = True` + `SECURE_REDIRECT_EXEMPT`.
//!
//! When enabled, every HTTP request gets a `301 Moved Permanently`
//! redirect to the same URL on the HTTPS scheme. Behind a reverse
//! proxy that terminates TLS (the common case), this works only
//! when the proxy forwards a header like `X-Forwarded-Proto: https`
//! — otherwise the layer would redirect-loop. Configure the trusted
//! header pair with [`SslRedirectLayer::proxy_ssl_header`] (Django
//! `SECURE_PROXY_SSL_HEADER`).
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::ssl_redirect::{SslRedirectLayer, SslRedirectRouterExt};
//!
//! let app = Router::new()
//!     .route("/", get(home))
//!     .ssl_redirect(
//!         SslRedirectLayer::new()
//!             .proxy_ssl_header("X-Forwarded-Proto", "https")
//!             .exempt(["/health", "/ready"]),
//!     );
//! ```
//!
//! ## Exempt paths
//!
//! [`SslRedirectLayer::exempt`] takes prefix patterns; a request
//! whose path starts with any exempt entry skips the redirect.
//! Useful for health checks behind a plain-HTTP LB. Empty exempt
//! list (default) redirects every path.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue, Response, StatusCode};
use axum::middleware::Next;
use axum::Router;

/// Tower-layer-equivalent configuration. Holds the proxy-header
/// pair + exempt-path list; applied via
/// [`SslRedirectRouterExt::ssl_redirect`].
#[derive(Clone, Debug)]
pub struct SslRedirectLayer {
    /// `(header_name, expected_value)` that signals the request
    /// arrived over HTTPS (Django `SECURE_PROXY_SSL_HEADER`). If
    /// `None`, the layer inspects the URI scheme directly — which
    /// only works when rustango is the TLS terminator (rare in
    /// production; LB usually does).
    proxy_ssl_header: Option<(HeaderName, HeaderValue)>,
    /// URL-path prefixes exempt from the redirect. Request path
    /// starts-with check; entries should include leading `/`.
    exempt: Vec<String>,
}

impl Default for SslRedirectLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl SslRedirectLayer {
    /// Build a layer with no proxy header configured and an empty
    /// exempt list. Add [`Self::proxy_ssl_header`] before deploying
    /// behind a TLS-terminating proxy to avoid redirect loops.
    #[must_use]
    pub fn new() -> Self {
        Self {
            proxy_ssl_header: None,
            exempt: Vec::new(),
        }
    }

    /// Django `SECURE_PROXY_SSL_HEADER` parity — declare the
    /// header/value pair the reverse proxy sets to indicate the
    /// upstream request was HTTPS. The layer reads the header on
    /// each request; matching value → skip redirect.
    ///
    /// ```ignore
    /// SslRedirectLayer::new().proxy_ssl_header("X-Forwarded-Proto", "https")
    /// ```
    ///
    /// Invalid header names / values are silently dropped (the
    /// builder is infallible by design — bad input behaves as if
    /// the operator never called this method).
    #[must_use]
    pub fn proxy_ssl_header(mut self, header: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        if let (Ok(h), Ok(v)) = (
            HeaderName::try_from(header.as_ref()),
            HeaderValue::try_from(value.as_ref()),
        ) {
            self.proxy_ssl_header = Some((h, v));
        }
        self
    }

    /// Append URL-path prefixes that bypass the redirect. Request
    /// paths starting with any entry skip the layer. Useful for
    /// behind-the-LB health checks that hit `http://internal-ip/`.
    #[must_use]
    pub fn exempt<I, S>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.exempt.extend(paths.into_iter().map(Into::into));
        self
    }

    /// `true` when this request should pass through (no redirect).
    /// Pure helper, exposed for testing without the full layer.
    #[must_use]
    pub fn is_secure(&self, req: &Request<Body>) -> bool {
        // URI scheme (works when rustango terminates TLS itself).
        if req.uri().scheme_str() == Some("https") {
            return true;
        }
        // Proxy header — operator-declared trusted indicator.
        if let Some((header, expected)) = &self.proxy_ssl_header {
            if let Some(got) = req.headers().get(header) {
                if got == expected {
                    return true;
                }
            }
        }
        false
    }

    /// `true` when the request path matches any exempt prefix.
    #[must_use]
    pub fn is_exempt(&self, path: &str) -> bool {
        self.exempt.iter().any(|p| path.starts_with(p))
    }
}

/// Router extension trait — `.ssl_redirect(layer)`.
pub trait SslRedirectRouterExt {
    #[must_use]
    fn ssl_redirect(self, layer: SslRedirectLayer) -> Self;
}

impl<S: Clone + Send + Sync + 'static> SslRedirectRouterExt for Router<S> {
    fn ssl_redirect(self, layer: SslRedirectLayer) -> Self {
        let cfg = Arc::new(layer);
        self.layer(axum::middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let cfg = cfg.clone();
                async move { handle(cfg, req, next).await }
            },
        ))
    }
}

async fn handle(cfg: Arc<SslRedirectLayer>, req: Request<Body>, next: Next) -> Response<Body> {
    if cfg.is_secure(&req) || cfg.is_exempt(req.uri().path()) {
        return next.run(req).await;
    }
    // Build the HTTPS URL using the Host header for the authority
    // (proxy-aware: trusts what the LB advertises) + path + query
    // from the incoming URI.
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let location = format!("https://{host}{path_and_query}");

    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::MOVED_PERMANENTLY;
    if let Ok(v) = HeaderValue::from_str(&location) {
        resp.headers_mut().insert(axum::http::header::LOCATION, v);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_req(path: &str, host: &str) -> Request<Body> {
        Request::builder()
            .uri(path)
            .method("GET")
            .header("Host", host)
            .body(Body::empty())
            .unwrap()
    }

    #[test]
    fn default_layer_treats_plain_http_as_insecure() {
        let layer = SslRedirectLayer::new();
        let req = http_req("/", "example.com");
        assert!(!layer.is_secure(&req));
    }

    #[test]
    fn proxy_header_match_marks_request_secure() {
        let layer = SslRedirectLayer::new().proxy_ssl_header("X-Forwarded-Proto", "https");
        let req = Request::builder()
            .uri("/")
            .method("GET")
            .header("Host", "example.com")
            .header("X-Forwarded-Proto", "https")
            .body(Body::empty())
            .unwrap();
        assert!(layer.is_secure(&req));
    }

    #[test]
    fn proxy_header_mismatch_stays_insecure() {
        let layer = SslRedirectLayer::new().proxy_ssl_header("X-Forwarded-Proto", "https");
        let req = Request::builder()
            .uri("/")
            .method("GET")
            .header("Host", "example.com")
            .header("X-Forwarded-Proto", "http")
            .body(Body::empty())
            .unwrap();
        assert!(!layer.is_secure(&req));
    }

    #[test]
    fn exempt_prefix_skips_redirect() {
        let layer = SslRedirectLayer::new().exempt(["/health", "/ready"]);
        assert!(layer.is_exempt("/health"));
        assert!(layer.is_exempt("/health/db"));
        assert!(layer.is_exempt("/ready"));
        assert!(!layer.is_exempt("/api/users"));
    }

    #[test]
    fn invalid_proxy_header_pair_silently_dropped() {
        let layer =
            SslRedirectLayer::new().proxy_ssl_header("Invalid Header Name\nwith CRLF", "https");
        // No header configured → request without HTTPS scheme is
        // insecure.
        let req = http_req("/", "example.com");
        assert!(!layer.is_secure(&req));
        assert!(layer.proxy_ssl_header.is_none());
    }
}
