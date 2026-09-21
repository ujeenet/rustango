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
//! Field names follow the OpenTelemetry HTTP semantic conventions, the
//! same ones [`crate::tracing_layer`] uses, so an app running both
//! layers logs one request under one schema. `url.path` holds the path
//! alone and `url.query` the query, kept apart so a collector grouping
//! by `url.path` does not see one bucket per query string.
//!
//! Query values are redacted before they are logged. Never add a
//! credential-bearing parameter to a log line by hand.
//!
//! Filter via tracing-subscriber's env-filter (e.g. `RUST_LOG=rustango::access_log=info`).
//!
//! `tenant` is the tenant the request resolved to, or `-` when none
//! did: an apex or operator-console request, or a single-tenant app.
//! [`TenantField`] switches it to the org id, or off. See
//! [`crate::tenant_log`] for how the identity leaves the handler.
//!
//! [`TenantField`]: crate::access_log::TenantField

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
    /// Log every request (default). When `false`, only 4xx and 5xx are
    /// logged, which keeps production volume down.
    pub log_success: bool,
    /// Include the client IP. Needs
    /// `into_make_service_with_connect_info::<SocketAddr>()`.
    pub include_ip: bool,
    /// Requests at or above this many ms are logged at WARN instead of
    /// INFO. Default 1000. Use `u64::MAX` to turn it off.
    pub slow_threshold_ms: u64,
    /// Query parameters whose values are replaced with `[redacted]`.
    /// The default list covers the usual credential names such as
    /// `password`, `token`, `secret`, `api_key` and `access_token`.
    pub redact_query_params: Vec<String>,
    /// Trust `X-Forwarded-For` (or `X-Real-IP`) over the TCP peer
    /// address. Default `false`, because any client talking to the
    /// server directly can forge those headers. Turn it on only behind
    /// a trusted proxy (nginx, Cloudflare, AWS ALB) that overwrites
    /// them with the real client IP.
    pub trust_proxy_headers: bool,
    /// Which tenant identifier goes on the line. Default
    /// [`TenantField::Slug`], and `-` when no tenant resolved.
    pub tenant_field: TenantField,
}

/// How the access log names the request's tenant.
///
/// The slug is readable, but an operator picks it, so it is often the
/// customer's own name. [`TenantField::Id`] keeps tenant attribution
/// without shipping that name to an aggregator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TenantField {
    /// `tenant=acme`, the `Org.slug`. The default.
    #[default]
    Slug,
    /// `tenant=42`, the `Org.id`.
    Id,
    /// `tenant=acme#42`, both, to follow a renamed slug.
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
    /// Default layer: log every request, include the IP, flag requests
    /// over 1000ms as slow, redact the usual credential parameters.
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

    /// Pick the tenant identifier for the line, or [`TenantField::Off`]
    /// to leave it out. Default [`TenantField::Slug`].
    #[must_use]
    pub fn tenant_field(mut self, field: TenantField) -> Self {
        self.tenant_field = field;
        self
    }

    /// Use `X-Forwarded-For` / `X-Real-IP` for the client IP. Off by
    /// default, because a direct client can forge them. Turn it on
    /// ONLY behind a trusted reverse proxy (nginx, Cloudflare, AWS
    /// ALB) that overwrites them.
    #[must_use]
    pub fn trust_proxy_headers(mut self, on: bool) -> Self {
        self.trust_proxy_headers = on;
        self
    }

    /// Replace the redaction list with `params`. An empty list turns
    /// redaction off, which will log credentials in plain text.
    #[must_use]
    pub fn redact(mut self, params: Vec<String>) -> Self {
        self.redact_query_params = params;
        self
    }

    /// Add one more parameter name to redact, keeping the defaults.
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

    /// Leave the client IP out of events.
    #[must_use]
    pub fn without_ip(mut self) -> Self {
        self.include_ip = false;
        self
    }

    /// Set the slow-request threshold in ms; slower requests log at
    /// WARN.
    #[must_use]
    pub fn slow_threshold_ms(mut self, ms: u64) -> Self {
        self.slow_threshold_ms = ms;
        self
    }

    /// Apply a loaded [`crate::config::AuditSettings`] section. It
    /// reads `redact_query_params` and appends each name to the
    /// layer's list, so project settings extend the defaults instead
    /// of replacing them.
    ///
    /// Use [`AccessLogLayer::redact`] to replace the list instead.
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

/// Mount request observability on `router`: the span, the request id,
/// and the access log when one is configured.
///
/// One definition on purpose. Two serving paths mount these three
/// layers (`Cli::assemble_app` and `server::Builder`), and when each
/// had its own copy the two drifted into opposite layer order. Both
/// sites call this instead.
///
/// Order is the whole design. `.layer()` wraps, so the LAST call is
/// outermost and runs FIRST on the way in:
///
/// ```text
///   TracingLayer   outermost — opens the span
///     access_log   inside it, so its line inherits the span
///       request_id innermost — runs with the span current, so
///                  `record` lands on it
///       handler
/// ```
///
/// `access_log: None` means `[logging] access_log = false`: the log
/// line goes away, but **the span and request id stay**. That setting
/// names the log only, and a service logging at the edge still wants
/// trace context and `X-Request-Id`.
///
/// The span redacts with the access log's *configured* key list, not
/// the defaults. Otherwise a project's own secret key would be hidden
/// in the event and printed in clear text by the span on the same
/// line.
///
/// The `cfg` gate matches the callers: `Cli::mount_observability`
/// needs `manage`, `server::Builder` needs `tenancy`. A build with
/// neither has no caller, and `-D warnings` turns the dead code into
/// a build failure.
#[cfg(any(feature = "manage", feature = "tenancy"))]
#[must_use]
pub(crate) fn mount_observability(router: Router, access_log: Option<AccessLogLayer>) -> Router {
    #[cfg(feature = "admin")]
    {
        use crate::request_id::RequestIdRouterExt as _;
        let span = match access_log.as_ref() {
            Some(l) => {
                crate::tracing_layer::TracingLayer::new().redact(l.redact_query_params.clone())
            }
            None => crate::tracing_layer::TracingLayer::new(),
        };
        let router = router.request_id(crate::request_id::RequestIdLayer::default());
        let router = match access_log {
            Some(l) => router.access_log(l),
            None => router,
        };
        return router.layer(span);
    }

    // Without `admin` there is no span and no request id. The access
    // log still mounts, because it carries `tenant` itself.
    #[cfg(not(feature = "admin"))]
    match access_log {
        Some(l) => router.access_log(l),
        None => router,
    }
}

/// Adds `.access_log(layer)` to a Router.
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
    // Path and query stay separate fields: OpenTelemetry defines
    // `url.path` as the path alone. Joining them would make a
    // collector treat every query string as its own path.
    let path = req.uri().path().to_owned();
    let query = raw_query
        .map(|q| redact_query(q, &cfg.redact_query_params))
        .unwrap_or_default();
    let ip = if cfg.include_ip {
        resolve_client_ip(&req, cfg.trust_proxy_headers)
    } else {
        None
    };

    // The handler below resolves the tenant. Hold a slot open across
    // it so the identity can come back out, and read it *inside* the
    // scope: a read after the scope future resolves sees nothing.
    // See `crate::tenant_log`.
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
    // `f64` with microsecond precision, matching `tracing_layer`'s
    // field of the same name. One type per field name, or a collector
    // like Elasticsearch rejects the document. It also keeps
    // sub-millisecond requests from all reporting `0`.
    let duration_ms = (elapsed.as_micros() as f64) / 1000.0;
    // The threshold stays whole milliseconds: it is a setting, not a
    // measurement.
    let is_slow = elapsed.as_millis() as u64 >= cfg.slow_threshold_ms;

    let is_error = status >= 400;
    if !cfg.log_success && !is_error {
        return response;
    }

    let tenant = tenant_label(cfg.tenant_field, tenant);

    // Field names are the OpenTelemetry HTTP conventions, same as
    // `tracing_layer`. These lines go straight to a collector, so
    // renaming at the edge would be work every deployment repeats.
    let client_address = ip.as_deref().unwrap_or("-");

    // `url.query` goes out only when the request had one. OTel says
    // to omit it otherwise, `tracing_layer` does the same, and a
    // collector may reject an empty keyword field.
    //
    // One event callsite cannot drop a field, so the branch has to
    // wrap the whole macro call. The macro keeps that from turning
    // into six copies that drift apart.
    macro_rules! emit {
        ($level:ident $(, $msg:literal)?) => {
            if query.is_empty() {
                tracing::$level!(
                    "http.request.method" = %method,
                    "url.path" = %path,
                    "http.response.status_code" = status,
                    duration_ms,
                    "client.address" = %client_address,
                    tenant = %tenant,
                    $($msg,)?
                );
            } else {
                tracing::$level!(
                    "http.request.method" = %method,
                    "url.path" = %path,
                    "url.query" = %query,
                    "http.response.status_code" = status,
                    duration_ms,
                    "client.address" = %client_address,
                    tenant = %tenant,
                    $($msg,)?
                );
            }
        };
    }

    if is_slow {
        emit!(warn, "slow request");
    } else if is_error {
        emit!(warn);
    } else {
        emit!(info);
    }

    response
}

/// Render the request's tenant for the log line, or `-` when none
/// resolved. Never blank, so "no tenant" does not look like a field
/// that went missing.
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

/// Resolve the client IP, in this order:
///
/// 1. With `trust_proxy_headers` on, the first hop of
///    `X-Forwarded-For` (the original client). Comma-separated,
///    whitespace trimmed.
/// 2. With `trust_proxy_headers` on and no XFF, `X-Real-IP`.
/// 3. The TCP peer from `ConnectInfo<SocketAddr>`, which axum only
///    sets for an app mounted with
///    `into_make_service_with_connect_info::<SocketAddr>()`.
/// 4. `None`, which the access log renders as `"-"`.
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

/// Query parameters whose values are redacted by default.
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
        // OAuth2 / OIDC. The framework's own `/sso/callback?code=…`
        // URLs carry credentials that none of the names above match.
        // Matching is exact, not substring, so `access_token` does
        // not cover `id_token`.
        "code".into(),
        "client_secret".into(),
        "id_token".into(),
        "code_verifier".into(),
        "state".into(),
        "assertion".into(),
        "session_state".into(),
    ]
}

/// Replace the values of redacted params with `[redacted]` in a raw
/// query string.
///
/// `pub(crate)` so [`crate::tracing_layer`] can redact the span's
/// `url.query` the same way. The span renders next to the event
/// fields, so a raw span value would put the secrets back on the line
/// this function just cleaned.
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

    /// The setter flips the flag, and the default is off so no
    /// project trusts forgeable headers by accident.
    #[test]
    fn trust_proxy_headers_defaults_off_and_setter_flips() {
        let l = AccessLogLayer::default();
        assert!(!l.trust_proxy_headers, "default is off (spoof-safe)");
        let l = AccessLogLayer::new().trust_proxy_headers(true);
        assert!(l.trust_proxy_headers);
    }

    /// `X-Forwarded-For` counts only when `trust_proxy_headers` is
    /// on. Otherwise we fall through to ConnectInfo, which this test
    /// does not set.
    #[test]
    fn resolve_client_ip_xff_only_when_proxy_trusted() {
        use axum::body::Body;
        let mut req = Request::builder()
            .uri("/")
            .header("x-forwarded-for", "203.0.113.7, 198.51.100.1, 10.0.0.5")
            .body(Body::empty())
            .unwrap();
        // Trust off: header ignored, no ConnectInfo, so None.
        assert_eq!(resolve_client_ip(&req, false), None);
        // Trust on: the first hop wins, i.e. the original client.
        assert_eq!(
            resolve_client_ip(&req, true).as_deref(),
            Some("203.0.113.7")
        );
        // Drop XFF, set X-Real-IP: the same trust gate applies.
        req.headers_mut().remove("x-forwarded-for");
        req.headers_mut()
            .insert("x-real-ip", "192.0.2.99".parse().unwrap());
        assert_eq!(resolve_client_ip(&req, false), None);
        assert_eq!(resolve_client_ip(&req, true).as_deref(), Some("192.0.2.99"));
    }

    /// `X-Forwarded-For` whitespace is trimmed, and a leading empty
    /// entry just falls through instead of breaking the parse.
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
        // An empty first hop falls through, and the access log
        // renders "-".
        assert_eq!(resolve_client_ip(&req, true), None);
    }

    /// With no proxy headers, the ConnectInfo extension wins. This is
    /// the usual single-host case.
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

    /// `with_audit_settings` extends the redaction list and never
    /// replaces it: TOML names pile on top of the defaults.
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

    /// An empty TOML list changes nothing.
    #[cfg(feature = "config")]
    #[test]
    fn with_audit_settings_empty_list_is_noop() {
        let s = crate::config::AuditSettings::default();
        let before = AccessLogLayer::new();
        let after = AccessLogLayer::new().with_audit_settings(&s);
        assert_eq!(before.redact_query_params, after.redact_query_params);
    }
}

// `runtime` is needed too: these tests read rendered output through
// `tracing_subscriber`, which only `runtime` pulls in. A build with
// the layers but no subscriber would fail to compile.
#[cfg(all(
    test,
    feature = "runtime",
    any(feature = "manage", feature = "tenancy")
))]
mod observability_mount_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt as _;
    use tracing_subscriber::fmt::MakeWriter;

    /// Tracing's callsite cache is process-global, so two tests
    /// installing subscribers at once would flake.
    fn lock() -> &'static std::sync::Mutex<()> {
        static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        M.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Drive one request through `mount_observability` and return what
    /// a handler's own `tracing::info!` rendered.
    #[cfg(feature = "admin")]
    async fn captured_handler_line(access_log: Option<AccessLogLayer>) -> (StatusCode, String) {
        let buf = Buf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        let _g = tracing::subscriber::set_default(subscriber);

        let app = mount_observability(
            Router::new().route(
                "/",
                get(|| async {
                    tracing::info!("in handler");
                    "ok"
                }),
            ),
            access_log,
        );
        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .expect("router answers");
        let status = resp.status();
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        (status, out)
    }

    /// `[logging] access_log = false` must not take the **span** with
    /// it. The span is the thing to assert here: a source scan or an
    /// `X-Request-Id` check both pass while the span is missing.
    #[cfg(feature = "admin")]
    #[tokio::test]
    async fn turning_off_the_access_log_keeps_the_request_span() {
        let _l = lock().lock().unwrap_or_else(|e| e.into_inner());
        let (status, out) = captured_handler_line(None).await;

        assert_eq!(status, StatusCode::OK);
        assert!(
            out.contains("in handler"),
            "the handler's own event never rendered, so this proves nothing:\n{out}"
        );
        assert!(
            out.contains("http.request"),
            "`access_log = false` stripped the request span: a handler event rendered \
             with no span context, so it carries no method, path, tenant or request \
             id. That setting names the log, not the trace context.\n{out}"
        );
    }

    /// The control: with a log configured, the span is there too.
    #[cfg(feature = "admin")]
    #[tokio::test]
    async fn the_normal_path_mounts_the_span_as_well() {
        let _l = lock().lock().unwrap_or_else(|e| e.into_inner());
        let (status, out) = captured_handler_line(Some(AccessLogLayer::default())).await;

        assert_eq!(status, StatusCode::OK);
        assert!(
            out.contains("http.request"),
            "no span context on the normal path either:\n{out}"
        );
    }
}
