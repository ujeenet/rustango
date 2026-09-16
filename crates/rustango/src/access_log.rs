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
//! INFO http.request.method=GET url.path=/api/posts url.query=page=2
//!      http.response.status_code=200 duration_ms=12 client.address=192.0.2.1 tenant=acme
//! ```
//!
//! `url.path` is the path alone and `url.query` the redacted query
//! string, per OpenTelemetry — not one concatenated value. Grouping by a
//! `url.path` that carried the query would give a collector unbounded
//! cardinality under a name that promises the opposite.
//!
//! Field names are the OpenTelemetry HTTP semantic conventions, shared
//! with [`crate::tracing_layer`]. Before #1480 the two layers named
//! every field differently except `duration_ms` and `tenant`, so an app
//! running both emitted the same request under two schemas.
//!
//! Filter via tracing-subscriber's env-filter (e.g. `RUST_LOG=rustango::access_log=info`).
//!
//! `tenant` names the tenant the request resolved to, and is `-` when
//! none did — an apex or operator-console request, or a single-tenant
//! app. [`TenantField`] switches it to the org id, or off. See
//! [`crate::tenant_log`] for how the identity gets out of the handler.

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
    /// When `true`, prefer the first IP in `X-Forwarded-For` (or
    /// `X-Real-IP` when XFF is absent) over the TCP peer address.
    /// Default `false` — these headers are spoofable by any client
    /// reaching the server directly. Only enable when the framework
    /// is reverse-proxied behind a trusted hop (nginx, Cloudflare,
    /// AWS ALB) that strips client-supplied values and rewrites them
    /// with the real client IP. v0.30.16.
    pub trust_proxy_headers: bool,
    /// Which tenant identifier to put on the line. Defaults to
    /// [`TenantField::Slug`]; `-` whenever no tenant resolved.
    pub tenant_field: TenantField,
}

/// How the access log names the request's tenant.
///
/// The slug is readable but operator-chosen, so it often *is* the
/// customer's name — [`TenantField::Id`] keeps tenant attribution in the
/// logs without putting that in every line shipped to an aggregator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TenantField {
    /// `tenant=acme` — `Org.slug`. The default.
    #[default]
    Slug,
    /// `tenant=42` — `Org.id`.
    Id,
    /// `tenant=acme#42` — both, for correlating a renamed slug.
    Both,
    /// Never look one up; the field is always `-`.
    Off,
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
            trust_proxy_headers: false,
            tenant_field: TenantField::default(),
        }
    }

    /// Choose which tenant identifier appears on the line, or [`TenantField::Off`]
    /// to omit it. Default [`TenantField::Slug`].
    #[must_use]
    pub fn tenant_field(mut self, field: TenantField) -> Self {
        self.tenant_field = field;
        self
    }

    /// Honor `X-Forwarded-For` / `X-Real-IP` when resolving the
    /// client IP. Off by default because the headers are spoofable
    /// by direct clients. Enable ONLY when behind a trusted reverse
    /// proxy (nginx, Cloudflare, AWS ALB) that overwrites them.
    /// v0.30.16.
    #[must_use]
    pub fn trust_proxy_headers(mut self, on: bool) -> Self {
        self.trust_proxy_headers = on;
        self
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

    /// Apply values from a loaded
    /// [`crate::config::AuditSettings`] section (#87 wiring,
    /// v0.29). Currently honors `redact_query_params` — each name
    /// in the list is appended to the layer's existing redaction
    /// set (the framework's defaults aren't replaced; project
    /// overrides extend them).
    ///
    /// Use [`AccessLogLayer::redact`] directly when you want to
    /// REPLACE the default list rather than extend it.
    ///
    /// ```ignore
    /// let cfg = rustango::config::Settings::load_from_env()?;
    /// app.layer(AccessLogLayer::default().with_audit_settings(&cfg.audit).into_layer())
    /// ```
    #[cfg(feature = "config")]
    #[must_use]
    pub fn with_audit_settings(mut self, s: &crate::config::AuditSettings) -> Self {
        for name in &s.redact_query_params {
            self.redact_query_params.push(name.clone());
        }
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
    // Path and query are separate fields, because OpenTelemetry defines
    // `url.path` as the path component alone.
    //
    // They used to be concatenated into one `path` value, which was
    // harmless while the field was called `path` and became wrong the
    // moment it was renamed `url.path`: a collector grouping by that
    // field would mix `/api/posts` with `/api/posts?page=2` and every
    // other query string, giving the access log unbounded cardinality
    // under a name that promises the opposite. `tracing_layer` had it
    // right all along — path only, `url.query` separate — so this is
    // also what makes the two layers agree on the *value* and not just
    // the spelling.
    let path = req.uri().path().to_owned();
    let query = raw_query
        .map(|q| redact_query(q, &cfg.redact_query_params))
        .unwrap_or_default();
    let ip = if cfg.include_ip {
        resolve_client_ip(&req, cfg.trust_proxy_headers)
    } else {
        None
    };

    // The tenant is resolved inside the handler, below this middleware.
    // Hold a slot open across it so the identity can come back out, and
    // read it back *inside* the scope — a read after the scope future
    // resolves sees nothing. See `crate::tenant_log`.
    let want_tenant = cfg.tenant_field != TenantField::Off;
    let (response, tenant) = crate::tenant_log::scope(async {
        let response = next.run(req).await;
        let tenant = if want_tenant {
            crate::tenant_log::current()
        } else {
            None
        };
        (response, tenant)
    })
    .await;
    let status = response.status().as_u16();
    let elapsed = started.elapsed();
    // Microsecond precision as `f64`, matching `tracing_layer`'s span
    // field of the same name. Two reasons, and the first is the one
    // that bites: `duration_ms` was `u64` here and `f64` there, so a
    // collector that saw both got two types under one name —
    // Elasticsearch/OpenSearch rejects a document whose field type
    // conflicts with the established mapping. The second is that
    // `as_millis() as u64` reported `0` for every sub-millisecond
    // request, which is most of them on a local pool.
    let duration_ms = (elapsed.as_micros() as f64) / 1000.0;
    // The slow-request threshold stays integer milliseconds — it is a
    // configured whole-millisecond value, not a measurement.
    let is_slow = elapsed.as_millis() as u64 >= cfg.slow_threshold_ms;

    let is_error = status >= 400;
    if !cfg.log_success && !is_error {
        return response;
    }

    let tenant = tenant_label(cfg.tenant_field, tenant);

    // Field names follow the OpenTelemetry HTTP semantic conventions,
    // which is what `tracing_layer` already emitted. The two layers used
    // to disagree on every field but `duration_ms` and `tenant` —
    // `method`/`path`/`status` here against
    // `http.request.method`/`url.path`/`http.response.status_code`
    // there — so an app running both logged the same request twice under
    // two different schemas (#1480).
    //
    // OTel was chosen over the shorter names because these lines are
    // what gets shipped to a collector, and renaming at the edge is
    // work every deployment would repeat.
    let client_address = ip.as_deref().unwrap_or("-");
    if is_slow {
        tracing::warn!(
            "http.request.method" = %method,
            "url.path" = %path,
            "url.query" = %query,
            "http.response.status_code" = status,
            duration_ms,
            "client.address" = %client_address,
            tenant = %tenant,
            "slow request",
        );
    } else if is_error {
        tracing::warn!(
            "http.request.method" = %method,
            "url.path" = %path,
            "url.query" = %query,
            "http.response.status_code" = status,
            duration_ms,
            "client.address" = %client_address,
            tenant = %tenant,
        );
    } else {
        tracing::info!(
            "http.request.method" = %method,
            "url.path" = %path,
            "url.query" = %query,
            "http.response.status_code" = status,
            duration_ms,
            "client.address" = %client_address,
            tenant = %tenant,
        );
    }

    response
}

/// Render the request's tenant for the log line. `-` when none
/// resolved — an apex or operator-console request, or a single-tenant
/// deployment. Never blank, so "no tenant" reads differently from a
/// field that went missing.
fn tenant_label(field: TenantField, tenant: Option<crate::tenant_log::TenantLabel>) -> String {
    const NONE: &str = "-";
    let Some(t) = tenant else {
        return NONE.to_owned();
    };
    match field {
        TenantField::Off => NONE.to_owned(), // `tenant` is None when Off
        TenantField::Slug => t.slug,
        TenantField::Id => t.id.map_or_else(|| NONE.to_owned(), |id| id.to_string()),
        TenantField::Both => match t.id {
            Some(id) => format!("{}#{id}", t.slug),
            None => t.slug,
        },
    }
}

/// Resolve the client IP for an inbound request. v0.30.16.
///
/// Resolution order:
///
/// 1. When `trust_proxy_headers` is on AND the request carries
///    `X-Forwarded-For`, return the first hop (the leftmost
///    address — the original client per RFC 7239 conventions).
///    The header is comma-separated; whitespace-trimmed.
/// 2. When `trust_proxy_headers` is on AND `X-Real-IP` is set
///    (no XFF), return its value.
/// 3. Otherwise return the TCP peer from `ConnectInfo<SocketAddr>`.
///    `axum::serve` only populates this when the app is mounted via
///    `into_make_service_with_connect_info::<SocketAddr>()`; v0.30.16
///    fixed the framework's serve sites to do that.
/// 4. `None` when nothing matches — the access log renders `"-"`.
fn resolve_client_ip(req: &Request, trust_proxy: bool) -> Option<String> {
    if trust_proxy {
        if let Some(xff) = req
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
        {
            if let Some(first) = xff.split(',').next() {
                let ip = first.trim();
                if !ip.is_empty() {
                    return Some(ip.to_owned());
                }
            }
        }
        if let Some(real) = req
            .headers()
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(real.to_owned());
        }
    }
    req.extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.ip().to_string())
}

/// Default list of query-param names whose values get redacted.
pub(crate) fn default_redact_params() -> Vec<String> {
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
///
/// `pub(crate)` so [`crate::tracing_layer`] can apply the same
/// redaction to the span's `url.query`. It used to be private, and the
/// span recorded the raw string — which put the credentials back on the
/// very line this function had just cleaned, because the span context
/// renders alongside the event fields.
pub(crate) fn redact_query(raw: &str, redact_keys: &[String]) -> String {
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

    /// v0.30.16 — `trust_proxy_headers(on)` flips the flag; default
    /// off so no project accidentally trusts spoofable headers.
    #[test]
    fn trust_proxy_headers_defaults_off_and_setter_flips() {
        let l = AccessLogLayer::default();
        assert!(!l.trust_proxy_headers, "default is off (spoof-safe)");
        let l = AccessLogLayer::new().trust_proxy_headers(true);
        assert!(l.trust_proxy_headers);
    }

    /// `resolve_client_ip` honors `X-Forwarded-For` only when
    /// `trust_proxy_headers` is on. Default-off mode falls through
    /// to ConnectInfo (None here since the test doesn't inject one).
    #[test]
    fn resolve_client_ip_xff_only_when_proxy_trusted() {
        use axum::body::Body;
        let mut req = Request::builder()
            .uri("/")
            .header("x-forwarded-for", "203.0.113.7, 198.51.100.1, 10.0.0.5")
            .body(Body::empty())
            .unwrap();
        // trust off → ignored, no ConnectInfo, returns None
        assert_eq!(resolve_client_ip(&req, false), None);
        // trust on → first hop wins (the original client per RFC 7239)
        assert_eq!(
            resolve_client_ip(&req, true).as_deref(),
            Some("203.0.113.7")
        );
        // remove XFF, set X-Real-IP — same trust gate applies.
        req.headers_mut().remove("x-forwarded-for");
        req.headers_mut()
            .insert("x-real-ip", "192.0.2.99".parse().unwrap());
        assert_eq!(resolve_client_ip(&req, false), None);
        assert_eq!(resolve_client_ip(&req, true).as_deref(), Some("192.0.2.99"));
    }

    /// `X-Forwarded-For` whitespace is trimmed; empty leading
    /// commas don't crash the parse — we just fall through.
    #[test]
    fn resolve_client_ip_xff_handles_whitespace_and_empty() {
        use axum::body::Body;
        let req = Request::builder()
            .uri("/")
            .header("x-forwarded-for", "  192.0.2.1  ,  10.0.0.1")
            .body(Body::empty())
            .unwrap();
        assert_eq!(resolve_client_ip(&req, true).as_deref(), Some("192.0.2.1"));

        let req = Request::builder()
            .uri("/")
            .header("x-forwarded-for", " ,10.0.0.1")
            .body(Body::empty())
            .unwrap();
        // Empty first hop falls through (caller could decide to walk
        // the rest; v1 just renders "-" via the access_log fallback).
        assert_eq!(resolve_client_ip(&req, true), None);
    }

    /// ConnectInfo extension wins when no proxy headers + ConnectInfo
    /// is present (the common single-host case after v0.30.16's
    /// `with_connect_info` fix in `manage.rs` / `server/builder.rs`).
    #[test]
    fn resolve_client_ip_falls_back_to_connect_info() {
        use axum::body::Body;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 54321);
        let mut req = Request::builder().uri("/").body(Body::empty()).unwrap();
        req.extensions_mut().insert(ConnectInfo(peer));
        assert_eq!(resolve_client_ip(&req, false).as_deref(), Some("127.0.0.1"));
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

    /// `with_audit_settings` extends the redaction list — does NOT
    /// replace it. Defaults stay in place; per-project additions
    /// from TOML pile on top.
    #[cfg(feature = "config")]
    #[test]
    fn with_audit_settings_extends_redact_list() {
        let mut s = crate::config::AuditSettings::default();
        s.redact_query_params = vec!["session_id".into(), "csrf_token".into()];
        let l = AccessLogLayer::new().with_audit_settings(&s);
        // Project additions present
        assert!(l.redact_query_params.iter().any(|k| k == "session_id"));
        assert!(l.redact_query_params.iter().any(|k| k == "csrf_token"));
        // Framework defaults preserved
        assert!(l.redact_query_params.iter().any(|k| k == "password"));
        assert!(l.redact_query_params.iter().any(|k| k == "token"));
    }

    /// Empty TOML list is a no-op — same redaction set as default.
    #[cfg(feature = "config")]
    #[test]
    fn with_audit_settings_empty_list_is_noop() {
        let s = crate::config::AuditSettings::default();
        let before = AccessLogLayer::new();
        let after = AccessLogLayer::new().with_audit_settings(&s);
        assert_eq!(before.redact_query_params, after.redact_query_params);
    }
}
