//! Security headers middleware — HSTS, X-Frame-Options, X-Content-Type-Options,
//! Referrer-Policy, Cross-Origin-Opener-Policy, and a Content-Security-Policy builder.
//!
//! Django ships these by default via `SecurityMiddleware`. Rocket auto-attaches a
//! `Shield` fairing. This is rustango's equivalent — **must be explicitly added**
//! to your router but presets cover the common cases.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::security_headers::{SecurityHeadersLayer, SecurityHeadersRouterExt};
//!
//! let app = Router::new()
//!     .route("/api/posts", get(list_posts))
//!     .security_headers(SecurityHeadersLayer::strict());
//! ```
//!
//! ## Presets
//!
//! - [`SecurityHeadersLayer::strict`] — production: HSTS 1y + preload, XFO=DENY,
//!   nosniff, Referrer-Policy=no-referrer, COOP=same-origin, Permissions-Policy locked down
//! - [`SecurityHeadersLayer::relaxed`] — embeddable: HSTS 1y, XFO=SAMEORIGIN,
//!   nosniff, Referrer-Policy=strict-origin-when-cross-origin
//! - [`SecurityHeadersLayer::dev`] — local: nosniff only (HSTS would lock you to https forever)
//!
//! ## Custom CSP
//!
//! ```ignore
//! let csp = CspBuilder::new()
//!     .default_src(&["'self'"])
//!     .script_src(&["'self'", "https://cdn.example.com"])
//!     .style_src(&["'self'", "'unsafe-inline'"])
//!     .img_src(&["'self'", "data:", "https:"])
//!     .build();
//!
//! let layer = SecurityHeadersLayer::strict().csp(csp);
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::header::HeaderValue;
use axum::http::{HeaderName, Request, Response};
use axum::middleware::Next;
use axum::Router;

/// Configuration for the security headers middleware.
#[derive(Clone)]
pub struct SecurityHeadersLayer {
    pub hsts: Option<String>,
    pub xfo: Option<&'static str>,
    pub nosniff: bool,
    pub referrer_policy: Option<&'static str>,
    pub coop: Option<&'static str>,
    pub permissions_policy: Option<String>,
    pub csp: Option<String>,
    pub csp_report_only: bool,
    /// Custom additional headers — applied last.
    pub custom: BTreeMap<String, String>,
}

impl Default for SecurityHeadersLayer {
    fn default() -> Self {
        Self::strict()
    }
}

impl SecurityHeadersLayer {
    /// Empty config — no headers set. Build up with the chainable setters.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            hsts: None,
            xfo: None,
            nosniff: false,
            referrer_policy: None,
            coop: None,
            permissions_policy: None,
            csp: None,
            csp_report_only: false,
            custom: BTreeMap::new(),
        }
    }

    /// Production preset — strict defaults.
    ///
    /// - HSTS: `max-age=31536000; includeSubDomains; preload`
    /// - X-Frame-Options: `DENY`
    /// - X-Content-Type-Options: `nosniff`
    /// - Referrer-Policy: `no-referrer`
    /// - Cross-Origin-Opener-Policy: `same-origin`
    /// - Permissions-Policy: `camera=(), microphone=(), geolocation=()`
    #[must_use]
    pub fn strict() -> Self {
        Self {
            hsts: Some("max-age=31536000; includeSubDomains; preload".into()),
            xfo: Some("DENY"),
            nosniff: true,
            referrer_policy: Some("no-referrer"),
            coop: Some("same-origin"),
            permissions_policy: Some("camera=(), microphone=(), geolocation=()".into()),
            csp: None,
            csp_report_only: false,
            custom: BTreeMap::new(),
        }
    }

    /// Embeddable preset — allows same-origin framing.
    ///
    /// - HSTS: 1 year (no preload, no subdomains)
    /// - X-Frame-Options: `SAMEORIGIN`
    /// - X-Content-Type-Options: `nosniff`
    /// - Referrer-Policy: `strict-origin-when-cross-origin`
    #[must_use]
    pub fn relaxed() -> Self {
        Self {
            hsts: Some("max-age=31536000".into()),
            xfo: Some("SAMEORIGIN"),
            nosniff: true,
            referrer_policy: Some("strict-origin-when-cross-origin"),
            coop: None,
            permissions_policy: None,
            csp: None,
            csp_report_only: false,
            custom: BTreeMap::new(),
        }
    }

    /// Development preset — `nosniff` only. HSTS deliberately omitted so
    /// you don't lock your local dev box into HTTPS-forever.
    #[must_use]
    pub fn dev() -> Self {
        Self {
            hsts: None,
            xfo: None,
            nosniff: true,
            referrer_policy: None,
            coop: None,
            permissions_policy: None,
            csp: None,
            csp_report_only: false,
            custom: BTreeMap::new(),
        }
    }

    /// Override the HSTS header value (or set to None to remove).
    #[must_use]
    pub fn hsts(mut self, value: impl Into<String>) -> Self {
        self.hsts = Some(value.into());
        self
    }

    /// Set X-Frame-Options (`DENY`, `SAMEORIGIN`, or remove with `None`).
    #[must_use]
    pub fn xfo(mut self, value: &'static str) -> Self {
        self.xfo = Some(value);
        self
    }

    /// Attach a Content-Security-Policy. Build via [`CspBuilder`].
    #[must_use]
    pub fn csp(mut self, csp: String) -> Self {
        self.csp = Some(csp);
        self
    }

    /// Send CSP as `Content-Security-Policy-Report-Only` instead of enforcing.
    /// Use during rollout to monitor without breaking the page.
    #[must_use]
    pub fn csp_report_only(mut self, yes: bool) -> Self {
        self.csp_report_only = yes;
        self
    }

    /// Add an arbitrary custom header.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.custom.insert(name.into(), value.into());
        self
    }
}

/// Extension trait — `.security_headers(layer)` on Router.
pub trait SecurityHeadersRouterExt {
    #[must_use]
    fn security_headers(self, layer: SecurityHeadersLayer) -> Self;
}

impl<S: Clone + Send + Sync + 'static> SecurityHeadersRouterExt for Router<S> {
    fn security_headers(self, layer: SecurityHeadersLayer) -> Self {
        let cfg = Arc::new(layer);
        self.layer(axum::middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let cfg = cfg.clone();
                async move { handle(cfg, req, next).await }
            },
        ))
    }
}

async fn handle(cfg: Arc<SecurityHeadersLayer>, req: Request<Body>, next: Next) -> Response<Body> {
    let mut response = next.run(req).await;
    let headers = response.headers_mut();

    if let Some(v) = &cfg.hsts {
        if let Ok(hv) = HeaderValue::from_str(v) {
            headers.insert("strict-transport-security", hv);
        }
    }
    if let Some(v) = cfg.xfo {
        if let Ok(hv) = HeaderValue::from_str(v) {
            headers.insert("x-frame-options", hv);
        }
    }
    if cfg.nosniff {
        headers.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    }
    if let Some(v) = cfg.referrer_policy {
        if let Ok(hv) = HeaderValue::from_str(v) {
            headers.insert("referrer-policy", hv);
        }
    }
    if let Some(v) = cfg.coop {
        if let Ok(hv) = HeaderValue::from_str(v) {
            headers.insert("cross-origin-opener-policy", hv);
        }
    }
    if let Some(v) = &cfg.permissions_policy {
        if let Ok(hv) = HeaderValue::from_str(v) {
            headers.insert("permissions-policy", hv);
        }
    }
    if let Some(v) = &cfg.csp {
        let name = if cfg.csp_report_only {
            "content-security-policy-report-only"
        } else {
            "content-security-policy"
        };
        if let Ok(hv) = HeaderValue::from_str(v) {
            if let Ok(n) = HeaderName::try_from(name) {
                headers.insert(n, hv);
            }
        }
    }
    for (k, v) in &cfg.custom {
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(k.as_str()), HeaderValue::from_str(v))
        {
            headers.insert(name, value);
        }
    }

    response
}

// ------------------------------------------------------------------ CspBuilder

/// Builder for a Content-Security-Policy header value.
///
/// ```
/// use rustango::security_headers::CspBuilder;
/// let csp = CspBuilder::new()
///     .default_src(&["'self'"])
///     .script_src(&["'self'", "https://cdn.example.com"])
///     .img_src(&["'self'", "data:"])
///     .build();
/// assert!(csp.contains("default-src 'self'"));
/// assert!(csp.contains("script-src 'self' https://cdn.example.com"));
/// ```
#[derive(Debug, Clone, Default)]
pub struct CspBuilder {
    directives: BTreeMap<String, Vec<String>>,
}

impl CspBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn set(&mut self, name: &str, sources: &[&str]) {
        self.directives
            .insert(name.to_owned(), sources.iter().map(|s| (*s).to_owned()).collect());
    }

    #[must_use]
    pub fn default_src(mut self, sources: &[&str]) -> Self {
        self.set("default-src", sources);
        self
    }

    #[must_use]
    pub fn script_src(mut self, sources: &[&str]) -> Self {
        self.set("script-src", sources);
        self
    }

    #[must_use]
    pub fn style_src(mut self, sources: &[&str]) -> Self {
        self.set("style-src", sources);
        self
    }

    #[must_use]
    pub fn img_src(mut self, sources: &[&str]) -> Self {
        self.set("img-src", sources);
        self
    }

    #[must_use]
    pub fn font_src(mut self, sources: &[&str]) -> Self {
        self.set("font-src", sources);
        self
    }

    #[must_use]
    pub fn connect_src(mut self, sources: &[&str]) -> Self {
        self.set("connect-src", sources);
        self
    }

    #[must_use]
    pub fn frame_src(mut self, sources: &[&str]) -> Self {
        self.set("frame-src", sources);
        self
    }

    #[must_use]
    pub fn frame_ancestors(mut self, sources: &[&str]) -> Self {
        self.set("frame-ancestors", sources);
        self
    }

    #[must_use]
    pub fn object_src(mut self, sources: &[&str]) -> Self {
        self.set("object-src", sources);
        self
    }

    /// Add an arbitrary directive (for things not covered by named methods).
    #[must_use]
    pub fn directive(mut self, name: impl Into<String>, sources: &[&str]) -> Self {
        let name = name.into();
        self.directives
            .insert(name, sources.iter().map(|s| (*s).to_owned()).collect());
        self
    }

    /// Strict starter preset: `default-src 'self'; object-src 'none'; base-uri 'self'`.
    #[must_use]
    pub fn strict_starter() -> Self {
        Self::new()
            .default_src(&["'self'"])
            .object_src(&["'none'"])
            .directive("base-uri", &["'self'"])
    }

    /// Render the policy as a string ready to drop into the CSP header.
    #[must_use]
    pub fn build(&self) -> String {
        self.directives
            .iter()
            .map(|(k, v)| format!("{k} {}", v.join(" ")))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_preset_has_all_canonical_headers() {
        let l = SecurityHeadersLayer::strict();
        assert!(l.hsts.is_some());
        assert_eq!(l.xfo, Some("DENY"));
        assert!(l.nosniff);
        assert_eq!(l.referrer_policy, Some("no-referrer"));
        assert_eq!(l.coop, Some("same-origin"));
        assert!(l.permissions_policy.is_some());
    }

    #[test]
    fn relaxed_preset_allows_same_origin_framing() {
        let l = SecurityHeadersLayer::relaxed();
        assert_eq!(l.xfo, Some("SAMEORIGIN"));
        assert!(l.hsts.is_some());
        assert!(l.coop.is_none());
    }

    #[test]
    fn dev_preset_only_nosniff() {
        let l = SecurityHeadersLayer::dev();
        assert!(l.hsts.is_none(), "dev must NOT set HSTS — would lock localhost to https");
        assert!(l.xfo.is_none());
        assert!(l.nosniff);
    }

    #[test]
    fn empty_preset_sets_nothing() {
        let l = SecurityHeadersLayer::empty();
        assert!(l.hsts.is_none());
        assert!(!l.nosniff);
        assert!(l.csp.is_none());
    }

    #[test]
    fn custom_header_chained_in() {
        let l = SecurityHeadersLayer::strict().header("x-custom", "value");
        assert_eq!(l.custom.get("x-custom").map(String::as_str), Some("value"));
    }

    #[test]
    fn csp_builder_basic() {
        let csp = CspBuilder::new().default_src(&["'self'"]).build();
        assert_eq!(csp, "default-src 'self'");
    }

    #[test]
    fn csp_builder_multi_source() {
        let csp = CspBuilder::new()
            .script_src(&["'self'", "https://cdn.example.com"])
            .build();
        assert_eq!(csp, "script-src 'self' https://cdn.example.com");
    }

    #[test]
    fn csp_builder_multiple_directives_joined_by_semicolon() {
        let csp = CspBuilder::new()
            .default_src(&["'self'"])
            .img_src(&["'self'", "data:"])
            .build();
        // BTreeMap orders alphabetically: default-src then img-src
        assert_eq!(csp, "default-src 'self'; img-src 'self' data:");
    }

    #[test]
    fn csp_builder_strict_starter_preset() {
        let csp = CspBuilder::strict_starter().build();
        assert!(csp.contains("default-src 'self'"));
        assert!(csp.contains("object-src 'none'"));
        assert!(csp.contains("base-uri 'self'"));
    }

    #[test]
    fn csp_builder_directive_helper() {
        let csp = CspBuilder::new()
            .directive("upgrade-insecure-requests", &[])
            .build();
        assert!(csp.contains("upgrade-insecure-requests"));
    }

    #[test]
    fn csp_attached_to_layer() {
        let csp = CspBuilder::new().default_src(&["'self'"]).build();
        let l = SecurityHeadersLayer::strict().csp(csp.clone());
        assert_eq!(l.csp.as_deref(), Some(csp.as_str()));
    }

    #[test]
    fn report_only_flag_toggles() {
        let l = SecurityHeadersLayer::strict()
            .csp("default-src 'self'".into())
            .csp_report_only(true);
        assert!(l.csp_report_only);
    }
}
