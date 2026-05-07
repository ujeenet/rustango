//! HTTP access log middleware — emit one tracing event per request.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::access_log::{AccessLogLayer, AccessLogRouterExt};
//!
//! let app = Router::new()
//!     .route("/api/posts", get(list_posts))
//!     .access_log(AccessLogLayer::default());
//! ```
//!
//! Emits one `tracing::info!` event per completed request:
//!
//! ```text
//! INFO method=GET path=/api/posts status=200 duration_ms=12 ip=192.0.2.1
//! ```
//!
//! Filter via tracing-subscriber's env-filter (e.g. `RUST_LOG=rustango::access_log=info`).

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::Response;
use axum::middleware::Next;
use axum::Router;

/// Configuration for the access log middleware.
#[derive(Clone)]
pub struct AccessLogLayer {
    /// Log all requests including 1xx/2xx/3xx (default true). When false,
    /// only 4xx/5xx are logged — useful in production to keep volume down.
    pub log_success: bool,
    /// Include the client IP address in the event (requires
    /// `into_make_service_with_connect_info::<SocketAddr>()`).
    pub include_ip: bool,
    /// Threshold (in ms) above which a request is logged at WARN instead
    /// of INFO. Set to `u64::MAX` to disable. Default 1000ms.
    pub slow_threshold_ms: u64,
    /// Query parameter names whose values get redacted in logs. Default
    /// includes the common credential-bearing params: `password`, `token`,
    /// `secret`, `api_key`, `access_token`, `refresh_token`, `signature`.
    pub redact_query_params: Vec<String>,
}

impl Default for AccessLogLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl AccessLogLayer {
    /// New layer with default config: log every request, include IP,
    /// flag requests >1000ms as slow, redact known credential query params.
    #[must_use]
    pub fn new() -> Self {
        Self {
            log_success: true,
            include_ip: true,
            slow_threshold_ms: 1000,
            redact_query_params: default_redact_params(),
        }
    }

    /// Replace the redacted-params list with `params`. Pass an empty list
    /// to disable redaction.
    #[must_use]
    pub fn redact(mut self, params: Vec<String>) -> Self {
        self.redact_query_params = params;
        self
    }

    /// Add an additional query-param name to redact (extends defaults).
    #[must_use]
    pub fn redact_additional(mut self, name: impl Into<String>) -> Self {
        self.redact_query_params.push(name.into());
        self
    }

    /// Skip 2xx/3xx responses; only log 4xx/5xx.
    #[must_use]
    pub fn errors_only(mut self) -> Self {
        self.log_success = false;
        self
    }

    /// Don't include the client IP in events.
    #[must_use]
    pub fn without_ip(mut self) -> Self {
        self.include_ip = false;
        self
    }

    /// Set the threshold (in ms) above which requests are logged at WARN.
    #[must_use]
    pub fn slow_threshold_ms(mut self, ms: u64) -> Self {
        self.slow_threshold_ms = ms;
        self
    }
}

/// Extension trait — `.access_log(layer)` on Router.
pub trait AccessLogRouterExt {
    #[must_use]
    fn access_log(self, layer: AccessLogLayer) -> Self;
}

impl<S: Clone + Send + Sync + 'static> AccessLogRouterExt for Router<S> {
    fn access_log(self, layer: AccessLogLayer) -> Self {
        let cfg = Arc::new(layer);
        self.layer(axum::middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let cfg = cfg.clone();
                async move { handle(cfg, req, next).await }
            },
        ))
    }
}

async fn handle(cfg: Arc<AccessLogLayer>, req: Request<Body>, next: Next) -> Response<Body> {
    let started = Instant::now();
    let method = req.method().clone();
    let raw_query = req.uri().query();
    let path = match raw_query {
        Some(q) => format!(
            "{}?{}",
            req.uri().path(),
            redact_query(q, &cfg.redact_query_params),
        ),
        None => req.uri().path().to_owned(),
    };
    let ip = if cfg.include_ip {
        req.extensions()
            .get::<ConnectInfo<std::net::SocketAddr>>()
            .map(|ci| ci.ip().to_string())
    } else {
        None
    };

    let response = next.run(req).await;
    let status = response.status().as_u16();
    let duration_ms = started.elapsed().as_millis() as u64;

    let is_error = status >= 400;
    if !cfg.log_success && !is_error {
        return response;
    }

    if duration_ms >= cfg.slow_threshold_ms {
        tracing::warn!(
            method = %method,
            path = %path,
            status,
            duration_ms,
            ip = ip.as_deref().unwrap_or("-"),
            "slow request",
        );
    } else if is_error {
        tracing::warn!(
            method = %method,
            path = %path,
            status,
            duration_ms,
            ip = ip.as_deref().unwrap_or("-"),
        );
    } else {
        tracing::info!(
            method = %method,
            path = %path,
            status,
            duration_ms,
            ip = ip.as_deref().unwrap_or("-"),
        );
    }

    response
}

/// Default list of query-param names whose values get redacted.
fn default_redact_params() -> Vec<String> {
    vec![
        "password".into(),
        "passwd".into(),
        "token".into(),
        "secret".into(),
        "api_key".into(),
        "apikey".into(),
        "access_token".into(),
        "refresh_token".into(),
        "signature".into(),
        "auth".into(),
    ]
}

/// Replace values of redacted params with `[redacted]` in a raw query string.
fn redact_query(raw: &str, redact_keys: &[String]) -> String {
    raw.split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, _)) if redact_keys.iter().any(|r| r.eq_ignore_ascii_case(k)) => {
                format!("{k}=[redacted]")
            }
            _ => pair.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_log_everything() {
        let l = AccessLogLayer::default();
        assert!(l.log_success);
        assert!(l.include_ip);
        assert_eq!(l.slow_threshold_ms, 1000);
    }

    #[test]
    fn errors_only_disables_success_logs() {
        let l = AccessLogLayer::new().errors_only();
        assert!(!l.log_success);
    }

    #[test]
    fn without_ip_skips_ip_capture() {
        let l = AccessLogLayer::new().without_ip();
        assert!(!l.include_ip);
    }

    #[test]
    fn slow_threshold_override() {
        let l = AccessLogLayer::new().slow_threshold_ms(500);
        assert_eq!(l.slow_threshold_ms, 500);
    }

    #[test]
    fn defaults_include_common_credential_params() {
        let l = AccessLogLayer::default();
        for required in &["password", "token", "secret", "api_key", "access_token"] {
            assert!(
                l.redact_query_params.iter().any(|k| k == required),
                "default redact list must include `{required}`"
            );
        }
    }

    #[test]
    fn redact_query_replaces_password() {
        let r = redact_query("user=alice&password=hunter2", &["password".to_owned()]);
        assert_eq!(r, "user=alice&password=[redacted]");
    }

    #[test]
    fn redact_query_handles_multiple_redacted_keys() {
        let r = redact_query(
            "u=a&token=xxx&password=yyy&q=z",
            &["password".into(), "token".into()],
        );
        assert!(r.contains("u=a"));
        assert!(r.contains("q=z"));
        assert!(r.contains("token=[redacted]"));
        assert!(r.contains("password=[redacted]"));
    }

    #[test]
    fn redact_query_is_case_insensitive_on_keys() {
        let r = redact_query("PASSWORD=x", &["password".to_owned()]);
        assert_eq!(r, "PASSWORD=[redacted]");
    }

    #[test]
    fn redact_query_passes_through_when_no_match() {
        let r = redact_query("a=1&b=2", &["password".to_owned()]);
        assert_eq!(r, "a=1&b=2");
    }

    #[test]
    fn redact_query_handles_empty_list() {
        let r = redact_query("password=x", &[]);
        assert_eq!(r, "password=x");
    }

    #[test]
    fn redact_additional_extends_defaults() {
        let l = AccessLogLayer::new().redact_additional("session_id");
        assert!(l.redact_query_params.iter().any(|k| k == "session_id"));
        // Defaults still present
        assert!(l.redact_query_params.iter().any(|k| k == "password"));
    }

    #[test]
    fn redact_replaces_default_list() {
        let l = AccessLogLayer::new().redact(vec!["only_this".into()]);
        assert_eq!(l.redact_query_params, vec!["only_this".to_owned()]);
    }
}
