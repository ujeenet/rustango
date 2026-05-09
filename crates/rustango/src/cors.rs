//! CORS middleware — Cross-Origin Resource Sharing for axum routers.
//!
//! Adds the standard CORS response headers and handles `OPTIONS` preflight
//! requests automatically.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::cors::CorsLayer;
//! use axum::Router;
//!
//! let app = Router::new()
//!     .route("/api/posts", axum::routing::get(list_posts))
//!     .layer(CorsLayer::permissive()); // allow any origin (dev only)
//! ```
//!
//! ## Production config
//!
//! ```ignore
//! use rustango::cors::CorsLayer;
//! use std::time::Duration;
//!
//! let cors = CorsLayer::new()
//!     .allow_origins(vec!["https://app.example.com", "https://admin.example.com"])
//!     .allow_methods(vec!["GET", "POST", "PUT", "PATCH", "DELETE"])
//!     .allow_headers(vec!["content-type", "authorization"])
//!     .allow_credentials(true)
//!     .max_age(Duration::from_secs(3600));
//!
//! let app = Router::new()
//!     .route("/api/posts", axum::routing::post(create_post))
//!     .layer(cors);
//! ```

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::header::{
    HeaderName, HeaderValue, ACCESS_CONTROL_ALLOW_CREDENTIALS, ACCESS_CONTROL_ALLOW_HEADERS,
    ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_EXPOSE_HEADERS,
    ACCESS_CONTROL_MAX_AGE, ACCESS_CONTROL_REQUEST_HEADERS, ORIGIN, VARY,
};
use axum::http::{Method, Request, Response, StatusCode};
use axum::middleware::Next;
use axum::Router;

/// Origin matching policy.
#[derive(Clone, Debug)]
pub enum AllowOrigin {
    /// Echo back any incoming `Origin` (sets `Access-Control-Allow-Origin: *`
    /// when no specific origin is sent — request-aware reflection).
    Any,
    /// Allow only origins in this list (case-insensitive exact match).
    List(Arc<Vec<String>>),
}

/// Builder for the CORS axum layer.
#[derive(Clone)]
pub struct CorsLayer {
    allow_origin: AllowOrigin,
    allow_methods: Vec<String>,
    allow_headers: Vec<String>,
    expose_headers: Vec<String>,
    allow_credentials: bool,
    max_age: Option<Duration>,
}

impl Default for CorsLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl CorsLayer {
    /// Empty CORS layer — no origins allowed by default. Configure with
    /// `allow_origins` / `allow_any_origin`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            allow_origin: AllowOrigin::List(Arc::new(Vec::new())),
            allow_methods: Vec::new(),
            allow_headers: Vec::new(),
            expose_headers: Vec::new(),
            allow_credentials: false,
            max_age: None,
        }
    }

    /// Wide-open development config: any origin, common methods, common
    /// headers. **Do not use in production.**
    #[must_use]
    pub fn permissive() -> Self {
        Self {
            allow_origin: AllowOrigin::Any,
            allow_methods: vec![
                "GET".into(),
                "POST".into(),
                "PUT".into(),
                "PATCH".into(),
                "DELETE".into(),
                "HEAD".into(),
                "OPTIONS".into(),
            ],
            allow_headers: vec!["*".into()],
            expose_headers: Vec::new(),
            allow_credentials: false,
            max_age: Some(Duration::from_secs(3600)),
        }
    }

    /// Allow these origins (case-insensitive exact match).
    #[must_use]
    pub fn allow_origins<I, S>(mut self, origins: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allow_origin =
            AllowOrigin::List(Arc::new(origins.into_iter().map(Into::into).collect()));
        self
    }

    /// Allow any origin (echoes the incoming Origin back).
    #[must_use]
    pub fn allow_any_origin(mut self) -> Self {
        self.allow_origin = AllowOrigin::Any;
        self
    }

    /// Build the layer from a loaded
    /// [`crate::config::SecuritySettings`] section (#87 wiring,
    /// v0.29). Three branches:
    ///
    /// - `cors_allowed_origins` empty → returns `None`. CORS is
    ///   opt-in; an empty list means "don't mount the layer at
    ///   all" (different from "allow zero origins").
    /// - List contains `"*"` → returns
    ///   `Some(Self::permissive())`. Wildcard is the canonical way
    ///   to write "any origin" in TOML; it maps to the existing
    ///   permissive preset.
    /// - Specific origins → returns `Some` with allowlist + the
    ///   common methods/headers most APIs need. Callers that want
    ///   tighter control build their own layer.
    ///
    /// ```ignore
    /// let cfg = rustango::config::Settings::load_from_env()?;
    /// if let Some(layer) = CorsLayer::from_settings(&cfg.security) {
    ///     app = app.layer(layer.into_layer());
    /// }
    /// ```
    #[cfg(feature = "config")]
    #[must_use]
    pub fn from_settings(s: &crate::config::SecuritySettings) -> Option<Self> {
        if s.cors_allowed_origins.is_empty() {
            return None;
        }
        if s.cors_allowed_origins.iter().any(|o| o == "*") {
            return Some(Self::permissive());
        }
        Some(
            Self::new()
                .allow_origins(s.cors_allowed_origins.iter().cloned())
                .allow_methods(["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"])
                .allow_headers(["Content-Type", "Authorization"])
                .max_age(Duration::from_secs(3600)),
        )
    }

    /// Allowed methods sent in preflight responses.
    #[must_use]
    pub fn allow_methods<I, S>(mut self, methods: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allow_methods = methods.into_iter().map(Into::into).collect();
        self
    }

    /// Allowed request headers sent in preflight responses.
    #[must_use]
    pub fn allow_headers<I, S>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allow_headers = headers.into_iter().map(Into::into).collect();
        self
    }

    /// Response headers exposed to the browser.
    #[must_use]
    pub fn expose_headers<I, S>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.expose_headers = headers.into_iter().map(Into::into).collect();
        self
    }

    /// Set `Access-Control-Allow-Credentials: true`. Note: when `true`
    /// you cannot use `allow_any_origin()` — browsers reject `*` with
    /// credentials.
    #[must_use]
    pub fn allow_credentials(mut self, yes: bool) -> Self {
        self.allow_credentials = yes;
        self
    }

    /// Browser preflight cache duration.
    #[must_use]
    pub fn max_age(mut self, dur: Duration) -> Self {
        self.max_age = Some(dur);
        self
    }

    /// Resolve the `Access-Control-Allow-Origin` value for a given request `Origin`.
    /// Returns `None` when the origin should be rejected.
    fn resolve_origin(&self, request_origin: Option<&str>) -> Option<String> {
        match (&self.allow_origin, request_origin) {
            (AllowOrigin::Any, Some(o)) => Some(o.to_owned()),
            (AllowOrigin::Any, None) => Some("*".to_owned()),
            (AllowOrigin::List(list), Some(o)) => {
                let lower = o.to_ascii_lowercase();
                if list
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(&lower))
                {
                    Some(o.to_owned())
                } else {
                    None
                }
            }
            (AllowOrigin::List(_), None) => None,
        }
    }
}

/// Extension trait that adds `.layer(CorsLayer)` ergonomics to `Router`.
pub trait CorsRouterExt {
    /// Apply this CORS configuration to all routes in this router.
    #[must_use]
    fn cors(self, layer: CorsLayer) -> Self;
}

impl<S: Clone + Send + Sync + 'static> CorsRouterExt for Router<S> {
    fn cors(self, layer: CorsLayer) -> Self {
        let cfg = Arc::new(layer);
        self.layer(axum::middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let cfg = cfg.clone();
                async move { handle(cfg, req, next).await }
            },
        ))
    }
}

async fn handle(cfg: Arc<CorsLayer>, req: Request<Body>, next: Next) -> Response<Body> {
    let req_origin = req
        .headers()
        .get(ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    // Preflight: short-circuit and return CORS headers
    if req.method() == Method::OPTIONS && req.headers().get(ORIGIN).is_some() {
        let mut response = Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .unwrap();
        // Echo requested headers back if a list isn't configured.
        let request_headers = req
            .headers()
            .get(ACCESS_CONTROL_REQUEST_HEADERS)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        attach_cors_headers(
            &cfg,
            req_origin.as_deref(),
            request_headers.as_deref(),
            &mut response,
        );
        return response;
    }

    // Pass through to the inner handler, then attach CORS to its response.
    let mut response = next.run(req).await;
    attach_cors_headers(&cfg, req_origin.as_deref(), None, &mut response);
    response
}

fn attach_cors_headers(
    cfg: &CorsLayer,
    request_origin: Option<&str>,
    request_headers: Option<&str>,
    response: &mut Response<Body>,
) {
    let Some(allow_origin) = cfg.resolve_origin(request_origin) else {
        return;
    };
    let headers = response.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&allow_origin) {
        headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, v);
    }
    // Vary: Origin so caches don't serve a wrong-origin response
    if matches!(cfg.allow_origin, AllowOrigin::List(_)) {
        headers.append(VARY, HeaderValue::from_static("origin"));
    }

    if !cfg.allow_methods.is_empty() {
        if let Ok(v) = HeaderValue::from_str(&cfg.allow_methods.join(", ")) {
            headers.insert(ACCESS_CONTROL_ALLOW_METHODS, v);
        }
    }

    let allow_headers = if cfg.allow_headers.is_empty() {
        request_headers.map(str::to_owned)
    } else {
        Some(cfg.allow_headers.join(", "))
    };
    if let Some(h) = allow_headers {
        if let Ok(v) = HeaderValue::from_str(&h) {
            headers.insert(ACCESS_CONTROL_ALLOW_HEADERS, v);
        }
    }

    if !cfg.expose_headers.is_empty() {
        if let Ok(v) = HeaderValue::from_str(&cfg.expose_headers.join(", ")) {
            headers.insert(ACCESS_CONTROL_EXPOSE_HEADERS, v);
        }
    }

    if cfg.allow_credentials {
        headers.insert(
            ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
    }

    if let Some(age) = cfg.max_age {
        if let Ok(v) = HeaderValue::from_str(&age.as_secs().to_string()) {
            headers.insert(ACCESS_CONTROL_MAX_AGE, v);
        }
    }
    let _ = (HeaderName::from_static("vary"),); // silence unused import in some configs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_any_with_origin() {
        let l = CorsLayer::new().allow_any_origin();
        assert_eq!(
            l.resolve_origin(Some("https://x.com")).as_deref(),
            Some("https://x.com")
        );
    }

    #[test]
    fn resolve_any_without_origin_returns_wildcard() {
        let l = CorsLayer::new().allow_any_origin();
        assert_eq!(l.resolve_origin(None).as_deref(), Some("*"));
    }

    #[test]
    fn resolve_list_match() {
        let l = CorsLayer::new().allow_origins(vec!["https://app.example.com"]);
        assert_eq!(
            l.resolve_origin(Some("https://app.example.com")).as_deref(),
            Some("https://app.example.com")
        );
    }

    #[test]
    fn resolve_list_case_insensitive() {
        let l = CorsLayer::new().allow_origins(vec!["https://APP.example.com"]);
        assert_eq!(
            l.resolve_origin(Some("https://app.example.com")).as_deref(),
            Some("https://app.example.com")
        );
    }

    #[test]
    fn resolve_list_miss_returns_none() {
        let l = CorsLayer::new().allow_origins(vec!["https://other.com"]);
        assert_eq!(l.resolve_origin(Some("https://x.com")), None);
    }

    #[test]
    fn resolve_empty_list_rejects_all() {
        let l = CorsLayer::new();
        assert_eq!(l.resolve_origin(Some("https://x.com")), None);
    }

    #[test]
    fn permissive_allows_any() {
        let l = CorsLayer::permissive();
        assert!(l.resolve_origin(Some("https://anywhere.test")).is_some());
    }

    // ---- #87 wiring: from_settings ----

    /// Empty `cors_allowed_origins` returns `None` so callers don't
    /// mount the layer at all — different from "allow zero origins"
    /// (which would 403 every preflight).
    #[cfg(feature = "config")]
    #[test]
    fn from_settings_empty_returns_none() {
        let s = crate::config::SecuritySettings::default();
        assert!(CorsLayer::from_settings(&s).is_none());
    }

    /// Wildcard `"*"` maps to the permissive preset.
    #[cfg(feature = "config")]
    #[test]
    fn from_settings_wildcard_returns_permissive() {
        let mut s = crate::config::SecuritySettings::default();
        s.cors_allowed_origins = vec!["*".into()];
        let layer = CorsLayer::from_settings(&s).expect("Some");
        assert!(layer.resolve_origin(Some("https://random.test")).is_some());
    }

    /// Specific origins build an allowlist; non-matching origins
    /// reject. Methods + headers + max-age get sensible defaults so
    /// the resulting layer is immediately usable.
    #[cfg(feature = "config")]
    #[test]
    fn from_settings_specific_origins_build_allowlist() {
        let mut s = crate::config::SecuritySettings::default();
        s.cors_allowed_origins = vec!["https://app.example.com".into()];
        let layer = CorsLayer::from_settings(&s).expect("Some");
        assert!(layer
            .resolve_origin(Some("https://app.example.com"))
            .is_some());
        assert!(layer
            .resolve_origin(Some("https://other.example.com"))
            .is_none());
    }
}
