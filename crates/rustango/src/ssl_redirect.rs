//! HTTP to HTTPS redirect middleware, like Django's
//! `SECURE_SSL_REDIRECT` and `SECURE_REDIRECT_EXEMPT`.
//!
//! Every plain-HTTP request gets a `301` to the same URL on HTTPS.
//! Behind a proxy that terminates TLS, set the trusted header with
//! [`SslRedirectLayer::proxy_ssl_header`]. Without it the layer
//! never sees the request as secure and redirects in a loop.
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
//! [`SslRedirectLayer::exempt`] takes path prefixes. A request whose
//! path starts with one of them is not redirected. This suits health
//! checks reached over plain HTTP. By default nothing is exempt.
//!
//! [`SslRedirectLayer::exempt`]: crate::ssl_redirect::SslRedirectLayer::exempt
//! [`SslRedirectLayer::proxy_ssl_header`]: crate::ssl_redirect::SslRedirectLayer::proxy_ssl_header

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue, Response, StatusCode};
use axum::middleware::Next;
use axum::Router;

/// Redirect settings. Apply them with
/// [`SslRedirectRouterExt::ssl_redirect`].
#[derive(Clone, Debug)]
pub struct SslRedirectLayer {
    /// Header name and value that mean "this request arrived over
    /// HTTPS". With `None` the layer reads the URI scheme, which
    /// only works when rustango terminates TLS itself.
    proxy_ssl_header: Option<(HeaderName, HeaderValue)>,
    /// Path prefixes that skip the redirect. Include the leading `/`.
    exempt: Vec<String>,
}

impl Default for SslRedirectLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl SslRedirectLayer {
    /// No proxy header, nothing exempt. Behind a TLS-terminating
    /// proxy, add [`Self::proxy_ssl_header`] or you get a redirect
    /// loop.
    #[must_use]
    pub fn new() -> Self {
        Self {
            proxy_ssl_header: None,
            exempt: Vec::new(),
        }
    }

    /// Declare the header your proxy sets when the original request
    /// used HTTPS, like Django's `SECURE_PROXY_SSL_HEADER`. A
    /// matching value skips the redirect.
    ///
    /// The layer trusts this header, so the proxy must strip any
    /// copy the client sends.
    ///
    /// ```ignore
    /// SslRedirectLayer::new().proxy_ssl_header("X-Forwarded-Proto", "https")
    /// ```
    ///
    /// An invalid header name or value is ignored, as if you never
    /// called this method.
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

    /// Add path prefixes that skip the redirect, such as health
    /// checks reached over plain HTTP.
    #[must_use]
    pub fn exempt<I, S>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.exempt.extend(paths.into_iter().map(Into::into));
        self
    }

    /// `true` when the request already arrived over HTTPS, so no
    /// redirect is needed.
    #[must_use]
    pub fn is_secure(&self, req: &Request<Body>) -> bool {
        // The scheme is set only when rustango terminates TLS.
        if req.uri().scheme_str() == Some("https") {
            return true;
        }
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

/// Adds `.ssl_redirect(layer)` to a router.
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
    // Target URL: Host header, then path and query from the request.
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
        // Nothing configured, so a plain HTTP request stays insecure.
        let req = http_req("/", "example.com");
        assert!(!layer.is_secure(&req));
        assert!(layer.proxy_ssl_header.is_none());
    }
}
