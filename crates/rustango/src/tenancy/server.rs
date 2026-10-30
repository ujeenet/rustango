//! `manage run-server` — the development server.
//!
//! Boots a complete operator + tenant admin stack with sensible
//! defaults from env. Users running `cargo run --
//! run-server` get:
//!
//! * Operator console at the apex (form login, sidebar layout) —
//!   only `rustango_orgs` and `rustango_operators` visible.
//! * Tenant admin at every subdomain — every `#[derive(Model)]`
//!   linked into the binary is automatically visible.
//! * Host-based dispatch matching the production routing story.
//! * Graceful shutdown on Ctrl-C.
//!
//! ## Env vars consumed
//!
//! | Var | Default | Effect |
//! |-----|---------|--------|
//! | `RUSTANGO_BIND` | `0.0.0.0:8080` | listener address |
//! | `RUSTANGO_APEX_DOMAIN` | `localhost` | apex host (operator surface); subdomains route to tenants |
//! | `RUSTANGO_SESSION_SECRET` | random + warn | HMAC key for operator session cookies |
//!
//! ## When to use it
//!
//! When your operator-experience defaults are good enough — read-only
//! operator on `rustango_orgs` + `rustango_operators`, every other
//! `#[derive(Model)]` available under tenant subdomains. If you need
//! per-model allow-lists, custom resolver chains, operator routes that
//! mutate, or arbitrary middleware, hand-roll your binary using
//! `operator_console::router` + `admin::TenantAdminBuilder` directly
//! (`examples/multitenant_demo.rs` shows the full shape).

use std::io::Write;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, Response};
use tower::ServiceExt as _;

use super::error::TenancyError;
use super::operator_console;
use super::pools::TenantPools;
use crate::sql::sqlx::Database;
use crate::tenancy::admin::TenantAdminBuilder;

/// Defaults for `manage run-server`. All fields are env-overridable;
/// the struct is exposed so programmatic callers can tweak them.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind: String,
    pub apex_domain: String,
    pub operator_show_only: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: crate::env::default_bind(&crate::env::tier_from_env()).into(),
            apex_domain: "localhost".into(),
            operator_show_only: vec!["rustango_orgs".into(), "rustango_operators".into()],
        }
    }
}

impl ServerConfig {
    /// Read overrides from env: `RUSTANGO_BIND` and the apex from
    /// [`apex_domain`]. Anything unset uses the default.
    #[must_use]
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(v) = std::env::var("RUSTANGO_BIND") {
            cfg.bind = v;
        }
        if let Some(v) = configured_apex_domain() {
            cfg.apex_domain = v;
        }
        cfg
    }
}

/// `[tenancy] apex_domain`, recorded at boot by `Cli::with_settings`.
static APEX_FROM_SETTINGS: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

/// Serializes tests that touch [`APEX_FROM_SETTINGS`].
#[cfg(test)]
pub(crate) static APEX_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Record `[tenancy] apex_domain`. The first call wins, like the other boot
/// globals; a later, different value is logged rather than dropped silently.
#[cfg(any(test, all(feature = "config", feature = "manage")))]
pub(crate) fn set_apex_domain_setting(apex: &str) {
    if apex.is_empty() {
        return;
    }
    let mut slot = APEX_FROM_SETTINGS
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match slot.as_deref() {
        None => *slot = Some(apex.to_owned()),
        Some(kept) if kept != apex => tracing::warn!(
            target: "rustango::tenancy",
            kept,
            ignored = apex,
            "[tenancy] apex_domain was already set by an earlier Cli::with_settings; keeping the first"
        ),
        Some(_) => {}
    }
}

/// Forget the recorded apex, so each test starts clean.
#[cfg(test)]
pub(crate) fn reset_apex_domain_setting() {
    *APEX_FROM_SETTINGS
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// The recorded `[tenancy] apex_domain`, ignoring the env var.
pub(crate) fn apex_domain_setting() -> Option<String> {
    APEX_FROM_SETTINGS
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// `RUSTANGO_APEX_DOMAIN`, else `[tenancy] apex_domain`; env wins, as for `bind`.
pub(crate) fn configured_apex_domain() -> Option<String> {
    pick_apex(
        std::env::var("RUSTANGO_APEX_DOMAIN").ok(),
        apex_domain_setting(),
    )
}

fn pick_apex(env: Option<String>, setting: Option<String>) -> Option<String> {
    env.or(setting)
}

/// The apex host, defaulting to `localhost`.
pub(crate) fn apex_domain() -> String {
    configured_apex_domain().unwrap_or_else(|| "localhost".into())
}

/// The apex tenant links point at: a `server::Builder`'s own, which its
/// dispatch matches, else [`apex_domain`].
#[derive(Clone, Debug, Default)]
pub(crate) struct LinkApex(Option<String>);

impl LinkApex {
    pub(crate) fn new(apex: impl Into<String>) -> Self {
        Self(Some(apex.into()))
    }

    pub(crate) fn get(&self) -> String {
        self.0.clone().unwrap_or_else(apex_domain)
    }
}

/// `RUSTANGO_TENANT_SCHEME`, empty counted as unset. The one reader of it.
pub(crate) fn tenant_scheme_setting() -> Option<String> {
    std::env::var("RUSTANGO_TENANT_SCHEME")
        .ok()
        .filter(|s| !s.is_empty())
}

/// `{scheme}://{host}{port_suffix}` for a link into a tenant host. The
/// scheme is [`tenant_scheme_setting`], else https wherever cookies are
/// `Secure`, so a handoff token is not sent in clear by default (#2425).
pub(crate) fn tenant_origin(host: &str, port_suffix: &str) -> String {
    let scheme = pick_tenant_scheme(
        tenant_scheme_setting(),
        crate::session::secure_cookies(),
        host,
    );
    format!("{scheme}://{host}{port_suffix}")
}

/// A link to `path` on `org`'s tenant host. `request_host` is the
/// caller's own `Host`, whose port is the last fallback.
pub(crate) fn tenant_link(
    org: &super::Org,
    path: &str,
    request_host: Option<&str>,
    apex: &LinkApex,
) -> String {
    let prefix = tenant_path_prefix(org.path_prefix.as_deref());
    let host = match org.host_pattern.as_deref().filter(|s| !s.is_empty()) {
        Some(pattern) => pattern.to_owned(),
        // A path-prefix tenant lives on the apex, any other on `<slug>.<apex>`.
        None if prefix.is_empty() => {
            if org.port.is_none() {
                tracing::warn!(
                    target: "rustango::tenancy",
                    tenant = %org.slug,
                    "tenant has no host, path prefix or port: the standard resolvers \
                     do not route its `<slug>.<apex>` link"
                );
            }
            format!("{}.{}", org.slug, apex.get())
        }
        None => apex.get(),
    };
    let port = tenant_port_suffix(
        org.port,
        std::env::var("RUSTANGO_TENANT_PORT").ok(),
        request_host,
    );
    format!(
        "{}{prefix}{}",
        tenant_origin(&host, &port),
        path.trim_end_matches('/')
    )
}

/// The org's path prefix for a tenant link, or `""` when the stored
/// value is not one segment: `//evil.example` would redirect off-site.
pub(crate) fn tenant_path_prefix(path_prefix: Option<&str>) -> &str {
    path_prefix
        .map(|p| p.trim_end_matches('/'))
        .filter(|p| crate::tenancy::provision::validate_path_prefix(p).is_ok())
        .unwrap_or("")
}

/// Port for a tenant link. A port-routed org's own port wins;
/// then `RUSTANGO_TENANT_PORT`; then the caller's request port.
pub(crate) fn tenant_port_suffix(
    org_port: Option<i32>,
    env_port: Option<String>,
    host: Option<&str>,
) -> String {
    let usable = |p: &str| !p.is_empty() && p != "80" && p != "443";
    if let Some(p) = org_port {
        let p = p.to_string();
        return if usable(&p) {
            format!(":{p}")
        } else {
            String::new()
        };
    }
    env_port
        .filter(|p| usable(p))
        .or_else(|| {
            host.and_then(|h| crate::urls::split_host_port(h)?.1)
                .map(|p| p.to_string())
                .filter(|p| usable(p))
        })
        .map(|p| format!(":{p}"))
        .unwrap_or_default()
}

/// A loopback host stays http: it never crosses the network, and browsers
/// keep `Secure` cookies on it.
fn pick_tenant_scheme(setting: Option<String>, secure: bool, host: &str) -> String {
    if let Some(scheme) = setting.filter(|s| !s.is_empty()) {
        return scheme;
    }
    if secure && !crate::urls::is_loopback_host(host) {
        "https"
    } else {
        "http"
    }
    .into()
}

/// Run the server until the process is signalled (Ctrl-C / SIGTERM).
///
/// Builds the operator console + tenant admin, wires host-based
/// dispatch, binds the listener, and serves. Returns when the
/// shutdown signal fires.
///
/// Prints a banner with the bound URLs to `writer` so users running
/// from the manage CLI see "open http://acme.localhost:8080/" and
/// know where to go.
///
/// # Errors
///
/// Returns [`TenancyError::Driver`] for connection / bind failures,
/// or [`TenancyError::Validation`] for malformed config.
pub async fn run<DB: Database, W: Write + Send>(
    pools: Arc<TenantPools<DB>>,
    registry_url: String,
    config: ServerConfig,
    writer: &mut W,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    if !no_operators_warn(&pools.registry_pool(), writer).await? {
        // No operator accounts — the user can still bring up the
        // server, but they can't log in. Loud warning, then proceed
        // (someone else may be planning to create an operator while
        // the server is up).
    }

    // The same keys and files as `server::Builder`, so a link the CLI signs
    // with the tenant key verifies here. Prod requires the env var.
    let keys = super::session::ServerKeys::load();

    let app = app(pools, registry_url, &config.apex_domain, keys);

    // --- Listener + banner ---
    let listener = crate::shutdown::bind_announced(&config.bind, Some(&config.apex_domain)).await?;
    let bound = listener.local_addr()?;
    writeln!(writer, "==> rustango operator + tenant server")?;
    writeln!(writer, "    bound to {bound}")?;
    writeln!(
        writer,
        "    operator UI    http://{}:{}/",
        config.apex_domain,
        bound.port()
    )?;
    writeln!(
        writer,
        "    tenant URLs    http://<slug>.{}:{}/<table>",
        config.apex_domain,
        bound.port()
    )?;
    writeln!(
        writer,
        "    apex domain    {}  (override with RUSTANGO_APEX_DOMAIN)",
        config.apex_domain
    )?;
    writeln!(writer, "    Ctrl-C to stop")?;
    writeln!(writer)?;
    writer.flush()?;

    // --- Serve until Ctrl-C ---
    crate::shutdown::serve_until_drained(listener, app, crate::shutdown::DEFAULT_DRAIN_TIMEOUT)
        .await
        .map_err(|e| TenancyError::Validation(format!("server error: {e}")))?;
    Ok(())
}

/// The operator console on the apex and the tenant admin, dispatched as
/// `server::Builder` does: a path-prefix tenant is reached at `<apex>/<prefix>`.
pub(crate) fn app<DB: Database>(
    pools: Arc<TenantPools<DB>>,
    registry_url: String,
    apex: &str,
    keys: super::session::ServerKeys,
) -> axum::Router
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let registry = pools.registry_pool();
    // Links (end-impersonation, first admin, tenant links) use this apex.
    let link_apex = LinkApex::new(apex);
    let tenant_admin =
        TenantAdminBuilder::<DB>::new(pools, registry_url, crate::server::apex_resolver(apex))
            .link_apex(link_apex.clone())
            .with_session(keys.tenant)
            .build();
    let apex = apex.to_owned();
    let resolver = Arc::new(crate::server::apex_resolver(&apex));
    // The console's routes win on the apex: only a path it does not serve
    // reaches a path-prefix tenant, so only such a path is looked up.
    let operator_console = operator_console::router(registry.clone(), keys.operator)
        .layer(axum::Extension(link_apex))
        .fallback_service(tower::service_fn({
            let tenants = tenant_admin.clone();
            let (resolver, registry) = (resolver.clone(), registry.clone());
            move |req: Request<Body>| {
                let mut tenants = tenants.clone();
                let (resolver, registry) = (resolver.clone(), registry.clone());
                async move {
                    let (req, resolution) = crate::server::resolve(req, &resolver, &registry).await;
                    if resolution == crate::server::Resolution::PrefixTenant {
                        tenants.as_service().oneshot(req).await
                    } else {
                        Ok(axum::response::IntoResponse::into_response(
                            axum::http::StatusCode::NOT_FOUND,
                        ))
                    }
                }
            }
        }));

    axum::Router::new().fallback_service(tower::service_fn(move |req: Request<Body>| {
        let mut operator = operator_console.clone();
        let mut tenants = tenant_admin.clone();
        let apex = apex.clone();
        async move {
            let response: Response<Body> =
                if super::resolver::host_is_apex(req.headers(), req.uri(), &apex) {
                    operator.as_service().oneshot(req).await
                } else {
                    tenants.as_service().oneshot(req).await
                }
                .map_err(|e| -> std::convert::Infallible {
                    panic!("axum router service is Infallible: {e}")
                })?;
            Ok::<_, std::convert::Infallible>(response)
        }
    }))
}

/// Print a loud warning if no operators exist — the operator UI
/// would be unreachable in that state. Returns `Ok(true)` when at
/// least one is present.
async fn no_operators_warn<W: Write + Send>(
    registry: &crate::sql::Pool,
    w: &mut W,
) -> Result<bool, TenancyError> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool;
    let active: Vec<super::auth::Operator> = super::auth::Operator::objects()
        .where_(super::auth::Operator::active.eq(true))
        .fetch(registry)
        .await?;
    if active.is_empty() {
        writeln!(w, "WARNING: no active operators in `rustango_operators` —")?;
        writeln!(
            w,
            "         the operator UI will reject every login until you run"
        )?;
        writeln!(
            w,
            "         `cargo run -- create-operator <username> --password <p>`."
        )?;
        writeln!(w)?;
        Ok(false)
    } else {
        Ok(true)
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod app_tests {
    use super::*;
    use crate::sql::Auto;
    use crate::tenancy::branding;
    use axum::http::StatusCode;

    /// `run-server`'s app on `apex.test` with one tenant at `/acme`.
    async fn prefix_app(
        logo_path: Option<String>,
    ) -> (axum::Router, crate::sql::Pool, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
        let registry = sqlx::SqlitePool::connect(&url).await.unwrap();
        let pools = Arc::new(TenantPools::<sqlx::Sqlite>::new(registry));
        let migrations = tmp.path().join("migrations");
        std::fs::create_dir_all(&migrations).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        crate::tenancy::manage::run_with_writer(
            pools.as_ref(),
            &url,
            &migrations,
            vec!["migrate-registry".to_owned()],
            &mut buf,
        )
        .await
        .expect("migrate-registry");
        let tenant_url = format!("sqlite://{}?mode=rwc", tmp.path().join("t.db").display());
        let mut org = super::super::Org {
            id: Auto::default(),
            slug: "pfx-acme".into(),
            display_name: "Acme".into(),
            storage_mode: "database".into(),
            backend_kind: "sqlite".into(),
            database_url: Some(tenant_url),
            schema_name: None,
            host_pattern: None,
            port: None,
            path_prefix: Some("/acme".into()),
            active: true,
            created_at: chrono::Utc::now(),
            brand_name: None,
            brand_tagline: None,
            logo_path,
            favicon_path: None,
            primary_color: None,
            theme_mode: None,
        };
        org.insert_pool(&pools.registry_pool()).await.unwrap();
        let secret = crate::session::SessionSecret::from_bytes(vec![9; 32]);
        let keys = crate::tenancy::session::ServerKeys {
            tenant: secret.clone(),
            operator: secret,
        };
        let registry = pools.registry_pool();
        (app(pools, url, "apex.test", keys), registry, tmp)
    }

    async fn get(app: &axum::Router, path: &str) -> Response<Body> {
        let req = Request::builder()
            .uri(path)
            .header("host", "apex.test")
            .body(Body::empty())
            .unwrap();
        app.clone().oneshot(req).await.unwrap()
    }

    /// A path-prefix tenant is reached at `<apex>/<prefix>`, not the console.
    #[tokio::test]
    async fn a_path_prefix_tenant_is_served_on_the_apex() {
        let (app, registry, _tmp) = prefix_app(None).await;
        let req = Request::builder()
            .uri("/acme/admin")
            .header("host", "apex.test")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            resp.status().is_redirection() && location.starts_with("/acme/"),
            "{} -> {location:?}",
            resp.status()
        );

        // A console route is served without a tenant lookup; an unknown
        // segment is looked up once and cached as a miss.
        for path in ["/login", "/no-such-tenant"] {
            let req = Request::builder()
                .uri(path)
                .header("host", "apex.test")
                .body(Body::empty())
                .unwrap();
            let _ = app.clone().oneshot(req).await.unwrap();
        }
        assert!(
            !crate::tenancy::prefix_looked_up(&registry, "/login"),
            "a console path was resolved"
        );
        assert!(crate::tenancy::prefix_looked_up(
            &registry,
            "/no-such-tenant"
        ));
    }

    /// The logo the prefix tenant's sign-in page links loads on the apex.
    #[tokio::test]
    async fn a_path_prefix_tenants_logo_loads_on_the_apex() {
        let brand = tempfile::tempdir().unwrap();
        std::env::set_var(branding::BRAND_STORAGE_ROOT_ENV, brand.path());
        let logo = branding::save_brand_asset(
            "pfx-acme",
            branding::BrandAssetKind::Logo,
            b"\x89PNG\r\n\x1a\n",
            Some("image/png"),
            &branding::default_brand_storage(),
        )
        .await
        .expect("save logo");
        let (app, _registry, _tmp) = prefix_app(Some(logo)).await;
        std::env::remove_var(branding::BRAND_STORAGE_ROOT_ENV);

        let resp = get(&app, "/acme/login").await;
        let html = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&html).replace("&#x2F;", "/");
        let src = html
            .split('"')
            .find(|s| s.contains("/_brand/"))
            .expect("logo linked");
        assert!(src.starts_with("/acme/_brand/pfx-acme/"), "{src}");
        assert_eq!(get(&app, src).await.status(), StatusCode::OK, "GET {src}");
    }
}

#[cfg(test)]
mod apex_tests {
    use super::*;

    #[test]
    fn env_wins_over_the_setting() {
        let s = || Some("toml.test".to_owned());
        assert_eq!(
            pick_apex(Some("env.test".into()), s()).as_deref(),
            Some("env.test")
        );
        assert_eq!(pick_apex(None, s()).as_deref(), Some("toml.test"));
    }

    /// #2425 — a handoff link defaults to https where cookies are Secure.
    #[test]
    fn tenant_links_default_to_https_when_cookies_are_secure() {
        assert_eq!(pick_tenant_scheme(None, true, "acme.example.com"), "https");
        assert_eq!(pick_tenant_scheme(None, false, "acme.example.com"), "http");
        // Loopback dev keeps http; the env var always wins.
        assert_eq!(pick_tenant_scheme(None, true, "acme.localhost"), "http");
        assert_eq!(pick_tenant_scheme(None, true, "127.0.0.1"), "http");
        assert_eq!(
            pick_tenant_scheme(Some("http".into()), true, "acme.example.com"),
            "http"
        );
        assert_eq!(
            pick_tenant_scheme(Some(String::new()), true, "a.example.com"),
            "https"
        );
    }

    /// #2425 — loopback is an address or `localhost`, port or not; a
    /// public name that starts with `127.` is not loopback.
    #[test]
    fn only_real_loopback_hosts_keep_http() {
        for host in [
            "localhost",
            "localhost:8080",
            "acme.localhost:8080",
            "127.0.0.1",
            "127.0.0.1:8000",
            "::1",
            "[::1]",
            "[::1]:8080",
        ] {
            assert!(crate::urls::is_loopback_host(host), "{host}");
        }
        for host in [
            "127.example.com",
            "acme.example.com:443",
            "[2001:db8::1]:80",
            "10.0.0.1",
        ] {
            assert!(!crate::urls::is_loopback_host(host), "{host}");
        }
    }

    /// A second, different apex keeps the first (and warns) (#2225).
    #[test]
    fn the_first_apex_setting_wins() {
        let _g = APEX_TEST_LOCK.blocking_lock();
        reset_apex_domain_setting();
        set_apex_domain_setting("one.test");
        set_apex_domain_setting("two.test");
        assert_eq!(apex_domain_setting().as_deref(), Some("one.test"));
        reset_apex_domain_setting();
    }
}
