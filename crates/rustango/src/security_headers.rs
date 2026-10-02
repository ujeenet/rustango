//! Security headers middleware — HSTS, X-Frame-Options, X-Content-Type-Options,
//! Referrer-Policy, Cross-Origin-Opener-Policy, and a Content-Security-Policy builder.
//!
//! Nothing is added for you: you **must mount the layer yourself**.
//! The presets cover the common cases.
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
//! - [`SecurityHeadersLayer::strict`]: for production.
//! - [`SecurityHeadersLayer::relaxed`]: allows same-origin framing.
//! - [`SecurityHeadersLayer::dev`]: `nosniff` only. HSTS is left out
//!   so you do not pin your dev machine to HTTPS.
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
//!
//! [`SecurityHeadersLayer::strict`]: crate::security_headers::SecurityHeadersLayer::strict
//! [`SecurityHeadersLayer::relaxed`]: crate::security_headers::SecurityHeadersLayer::relaxed
//! [`SecurityHeadersLayer::dev`]: crate::security_headers::SecurityHeadersLayer::dev

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::header::HeaderValue;
use axum::http::{HeaderName, Request, Response};
use axum::middleware::Next;
use axum::Router;

/// Configuration for the security headers middleware.
///
/// A header the handler already set is kept, not overwritten.
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

    /// Production preset.
    ///
    /// - HSTS: `max-age=31536000; includeSubDomains; preload`
    /// - X-Frame-Options: `DENY`
    /// - X-Content-Type-Options: `nosniff`
    /// - Referrer-Policy: `same-origin` (not `no-referrer`: that makes browsers send `Origin: null`)
    /// - Cross-Origin-Opener-Policy: `same-origin`
    /// - Permissions-Policy: `camera=(), microphone=(), geolocation=()`
    #[must_use]
    pub fn strict() -> Self {
        Self {
            hsts: Some("max-age=31536000; includeSubDomains; preload".into()),
            xfo: Some("DENY"),
            nosniff: true,
            referrer_policy: Some("same-origin"),
            coop: Some("same-origin"),
            permissions_policy: Some("camera=(), microphone=(), geolocation=()".into()),
            csp: None,
            csp_report_only: false,
            custom: BTreeMap::new(),
        }
    }

    /// Preset that allows same-origin framing.
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

    /// Development preset: `nosniff` only. HSTS is left out on
    /// purpose, so you do not pin your dev machine to HTTPS.
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

    /// Set the CSP `report-uri`. The browser POSTs violation reports
    /// there. Use [`csp_report_router`] to receive them.
    ///
    /// `report-uri` is deprecated in favour of `report-to`, which also
    /// needs a `Report-To` header naming an endpoint group.
    #[must_use]
    pub fn csp_report_uri(mut self, uri: &str) -> Self {
        if let Some(existing) = self.csp.as_mut() {
            existing.push_str(&format!("; report-uri {uri}"));
        } else {
            self.csp = Some(format!("default-src 'self'; report-uri {uri}"));
        }
        self
    }

    /// Add an arbitrary custom header. Overrides the preset's value
    /// for the same name.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.custom.insert(name.into(), value.into());
        self
    }

    /// Build the layer from a preset name:
    ///
    /// | name              | preset            |
    /// |-------------------|-------------------|
    /// | `"strict"`        | [`Self::strict`]  |
    /// | `"relaxed"`       | [`Self::relaxed`] |
    /// | `"dev"`           | [`Self::dev`]     |
    /// | `"none"`/`"empty"`| [`Self::empty`]   |
    ///
    /// Any other name gives [`Self::strict`]. A typo must not strip
    /// the security headers.
    #[must_use]
    pub fn from_preset(name: &str) -> Self {
        match name {
            "strict" => Self::strict(),
            "relaxed" => Self::relaxed(),
            "dev" => Self::dev(),
            "none" | "empty" => Self::empty(),
            // Unknown names get strict headers, never no headers.
            // `manage check --deploy` warns about the typo separately.
            _ => Self::strict(),
        }
    }

    /// Build the layer from a [`crate::config::SecuritySettings`]
    /// section. It starts from [`Self::from_preset`], using `strict`
    /// when `headers_preset` is unset, then applies overrides:
    ///
    /// - `csp` sets the Content-Security-Policy header as given.
    /// - `hsts_max_age_secs = 0` turns HSTS off.
    /// - A larger value rebuilds HSTS with that age and keeps
    ///   `includeSubDomains` and `preload`.
    ///
    /// ```no_run
    /// # use rustango::security_headers::{SecurityHeadersLayer, SecurityHeadersRouterExt};
    /// # fn wire(app: axum::Router) -> Result<axum::Router, Box<dyn std::error::Error>> {
    /// let cfg = rustango::config::Settings::load_from_env()?;
    /// let layer = SecurityHeadersLayer::from_settings(&cfg.security);
    /// let app = app.security_headers(layer);
    /// # Ok(app) }
    /// ```
    #[cfg(feature = "config")]
    #[must_use]
    pub fn from_settings(s: &crate::config::SecuritySettings) -> Self {
        let mut layer = match s.headers_preset.as_deref() {
            Some(name) => Self::from_preset(name),
            None => Self::strict(),
        };
        if let Some(csp) = s.csp.as_ref() {
            layer.csp = Some(csp.clone());
        }
        if let Some(secs) = s.hsts_max_age_secs {
            layer.hsts = if secs == 0 {
                None
            } else {
                Some(format!("max-age={secs}; includeSubDomains; preload"))
            };
        }
        layer
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
    // Build the layer's own set first (a `.header()` overrides the
    // preset), then add only what the handler did not set (#1699).
    let mut own = axum::http::HeaderMap::new();
    let mut set = |name: &str, value: &str| {
        if let (Ok(n), Ok(v)) = (HeaderName::try_from(name), HeaderValue::from_str(value)) {
            own.insert(n, v);
        }
    };

    if let Some(v) = &cfg.hsts {
        set("strict-transport-security", v);
    }
    if let Some(v) = cfg.xfo {
        set("x-frame-options", v);
    }
    if cfg.nosniff {
        set("x-content-type-options", "nosniff");
    }
    if let Some(v) = cfg.referrer_policy {
        set("referrer-policy", v);
    }
    if let Some(v) = cfg.coop {
        set("cross-origin-opener-policy", v);
    }
    if let Some(v) = &cfg.permissions_policy {
        set("permissions-policy", v);
    }
    if let Some(v) = &cfg.csp {
        let name = if cfg.csp_report_only {
            "content-security-policy-report-only"
        } else {
            "content-security-policy"
        };
        set(name, v);
    }
    for (k, v) in &cfg.custom {
        set(k, v);
    }

    let headers = response.headers_mut();
    for (name, value) in own {
        if let Some(name) = name {
            headers.entry(name).or_insert(value);
        }
    }
    response
}

// ------------------------------------------------------------------ CSP report endpoint

/// Build a router with a CSP-violation report endpoint at `path`,
/// usually `/__csp-report`. The browser POSTs a JSON report there
/// when a directive is violated, and the handler logs it with
/// `tracing::warn!`.
///
/// ## Quick start
///
/// ```ignore
/// use rustango::security_headers::{csp_report_router, SecurityHeadersLayer, CspBuilder};
///
/// let app = Router::new()
///     .route("/", get(home))
///     .merge(csp_report_router("/__csp-report"))
///     .security_headers(
///         SecurityHeadersLayer::strict()
///             .csp(CspBuilder::strict_starter().build())
///             .csp_report_uri("/__csp-report"),
///     );
/// ```
///
/// Reports look like:
/// ```json
/// {"csp-report": {
///   "document-uri": "https://app.example.com/page",
///   "violated-directive": "script-src 'self'",
///   "blocked-uri": "inline",
///   ...
/// }}
/// ```
pub fn csp_report_router(path: &str) -> axum::Router {
    use axum::routing::post;
    let path = path.to_owned();
    axum::Router::new().route(&path, post(handle_csp_report))
}

async fn handle_csp_report(body: axum::extract::Json<serde_json::Value>) -> axum::http::StatusCode {
    // Standard CSP report format wraps the body in {"csp-report": {...}}
    let report = body.0.get("csp-report").unwrap_or(&body.0);
    let document_uri = report
        .get("document-uri")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let violated = report
        .get("violated-directive")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let blocked = report
        .get("blocked-uri")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    tracing::warn!(
        document_uri = %document_uri,
        violated_directive = %violated,
        blocked_uri = %blocked,
        "CSP violation report",
    );
    axum::http::StatusCode::NO_CONTENT
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
        self.directives.insert(
            name.to_owned(),
            sources.iter().map(|s| (*s).to_owned()).collect(),
        );
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

    /// Add any other directive that has no named method here.
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
        assert_eq!(l.referrer_policy, Some("same-origin"));
        assert_eq!(l.coop, Some("same-origin"));
        assert!(l.permissions_policy.is_some());
    }

    /// A header the handler set is kept (#1699): the impersonation
    /// handoff's `no-referrer` must survive the outer layer.
    #[tokio::test]
    async fn a_header_the_handler_set_wins() {
        use axum::routing::get;
        use tower::ServiceExt as _;
        let app = Router::new()
            .route(
                "/",
                get(|| async { ([("referrer-policy", "no-referrer")], "ok") }),
            )
            .security_headers(SecurityHeadersLayer::strict());
        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.headers()["referrer-policy"], "no-referrer");
        assert_eq!(resp.headers()["x-frame-options"], "DENY");
    }

    /// `.header()` still overrides the preset for the same name.
    #[tokio::test]
    async fn a_custom_header_overrides_the_preset() {
        use axum::routing::get;
        use tower::ServiceExt as _;
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .security_headers(
                SecurityHeadersLayer::strict().header("x-frame-options", "SAMEORIGIN"),
            );
        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.headers()["x-frame-options"], "SAMEORIGIN");
    }

    /// Under `no-referrer` browsers send `Origin: null` on every POST,
    /// and the CSRF Origin check refuses it, so no form works (#1695).
    #[test]
    fn no_preset_makes_browsers_send_a_null_origin() {
        for l in [
            SecurityHeadersLayer::strict(),
            SecurityHeadersLayer::relaxed(),
            SecurityHeadersLayer::dev(),
        ] {
            assert_ne!(l.referrer_policy, Some("no-referrer"));
        }
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
        assert!(
            l.hsts.is_none(),
            "dev must NOT set HSTS — would lock localhost to https"
        );
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

    #[test]
    fn csp_report_uri_appends_to_existing_csp() {
        let l = SecurityHeadersLayer::strict()
            .csp("default-src 'self'".into())
            .csp_report_uri("/__csp-report");
        let csp = l.csp.unwrap();
        assert!(csp.contains("default-src 'self'"));
        assert!(csp.contains("report-uri /__csp-report"));
    }

    #[test]
    fn csp_report_uri_creates_default_csp_if_missing() {
        let l = SecurityHeadersLayer::strict().csp_report_uri("/__csp-report");
        let csp = l.csp.unwrap();
        assert!(csp.contains("default-src 'self'"));
        assert!(csp.contains("report-uri"));
    }

    // ---- from_preset + from_settings ----

    #[test]
    fn from_preset_maps_known_names() {
        assert_eq!(
            SecurityHeadersLayer::from_preset("strict").hsts.as_deref(),
            SecurityHeadersLayer::strict().hsts.as_deref(),
        );
        assert!(SecurityHeadersLayer::from_preset("dev").hsts.is_none());
        assert!(SecurityHeadersLayer::from_preset("none").hsts.is_none());
        assert!(!SecurityHeadersLayer::from_preset("none").nosniff);
    }

    /// An unknown preset name falls back to `strict()`. A typo in the
    /// TOML must not strip security headers in production.
    #[test]
    fn from_preset_unknown_name_fails_safe_to_strict() {
        let l = SecurityHeadersLayer::from_preset("strixt"); // typo
        assert_eq!(
            l.hsts.as_deref(),
            SecurityHeadersLayer::strict().hsts.as_deref(),
            "unknown preset should yield strict, not empty"
        );
    }

    #[cfg(feature = "config")]
    #[test]
    fn from_settings_default_picks_strict() {
        let s = crate::config::SecuritySettings::default();
        let l = SecurityHeadersLayer::from_settings(&s);
        assert_eq!(
            l.hsts.as_deref(),
            SecurityHeadersLayer::strict().hsts.as_deref(),
        );
    }

    #[cfg(feature = "config")]
    #[test]
    fn from_settings_honors_preset_name() {
        let mut s = crate::config::SecuritySettings::default();
        s.headers_preset = Some("dev".into());
        let l = SecurityHeadersLayer::from_settings(&s);
        assert!(l.hsts.is_none());
    }

    /// `hsts_max_age_secs = 0` turns HSTS off even if the preset set
    /// it. Useful on a staging tier with a self-signed certificate.
    #[cfg(feature = "config")]
    #[test]
    fn from_settings_zero_hsts_disables_header() {
        let mut s = crate::config::SecuritySettings::default();
        s.headers_preset = Some("strict".into());
        s.hsts_max_age_secs = Some(0);
        let l = SecurityHeadersLayer::from_settings(&s);
        assert!(l.hsts.is_none(), "hsts_max_age_secs = 0 should drop HSTS");
    }

    #[cfg(feature = "config")]
    #[test]
    fn from_settings_custom_hsts_max_age() {
        let mut s = crate::config::SecuritySettings::default();
        s.hsts_max_age_secs = Some(60);
        let l = SecurityHeadersLayer::from_settings(&s);
        let hsts = l.hsts.expect("hsts present");
        assert!(hsts.contains("max-age=60"), "got: {hsts}");
        assert!(
            hsts.contains("includeSubDomains"),
            "preserved subdomain inclusion"
        );
    }

    #[cfg(feature = "config")]
    #[test]
    fn from_settings_custom_csp_overrides_preset() {
        let mut s = crate::config::SecuritySettings::default();
        s.csp = Some("default-src 'self'; img-src *".into());
        let l = SecurityHeadersLayer::from_settings(&s);
        assert_eq!(l.csp.as_deref(), Some("default-src 'self'; img-src *"));
    }
}
