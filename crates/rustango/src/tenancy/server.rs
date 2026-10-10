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
use super::resolver::ChainResolver;
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
            bind: "0.0.0.0:8080".into(),
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

/// A loopback host stays http: it never crosses the network, and browsers
/// keep `Secure` cookies on it.
fn pick_tenant_scheme(setting: Option<String>, secure: bool, host: &str) -> String {
    if let Some(scheme) = setting.filter(|s| !s.is_empty()) {
        return scheme;
    }
    if secure && !is_loopback_host(host) {
        "https"
    } else {
        "http"
    }
    .into()
}

/// `localhost`, `*.localhost`, or a loopback IP, with or without a port.
fn is_loopback_host(host: &str) -> bool {
    use std::net::IpAddr;
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else if host.parse::<IpAddr>().is_ok() {
        host
    } else {
        host.rsplit_once(':')
            .filter(|(_, port)| port.bytes().all(|b| b.is_ascii_digit()))
            .map_or(host, |(name, _)| name)
    };
    let name = name.to_ascii_lowercase();
    name == "localhost"
        || name.ends_with(".localhost")
        || name.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
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

    // --- Shared signing key for operator + tenant cookies ---
    // Both consoles use HMAC-SHA256 over distinct cookie names + payload
    // shapes; sharing the key is safe and lets a single
    // RUSTANGO_SESSION_SECRET cover both surfaces.
    //
    // Persist the dev-mode random secret to disk so cargo-watch /
    // cargo run cycles don't sign every operator out on restart
    // (#69). Production should still set `RUSTANGO_SESSION_SECRET`.
    // Matches the Builder path at `server/builder.rs:302` so both
    // entrypoints behave the same.
    // Audit M2 — on the prod tier (RUSTANGO_ENV) this requires a valid
    // RUSTANGO_SESSION_SECRET and panics otherwise (fail closed); dev /
    // staging keep the disk-persisted key for restart-stable local runs.
    let session_secret = crate::session::load_session_secret_for_tier(
        &crate::session::tier_from_env(),
        std::path::Path::new("./var/.rustango_session.key"),
    );

    // --- Operator console at the apex ---
    // v0.38 — `registry_pool()` (unified Pool enum) instead of the
    // PG-only `registry()` accessor so the operator console runs on
    // whichever backend `TenantPools<DB>` was built with.
    let operator_console = operator_console::router(pools.registry_pool(), session_secret.clone());

    // --- Tenant admin at subdomains (with per-tenant auth) ---
    let resolver = ChainResolver::standard(config.apex_domain.clone());
    let tenant_admin = TenantAdminBuilder::<DB>::new(pools.clone(), registry_url, resolver)
        .with_session(session_secret)
        .build();

    // --- Host-based dispatch (matches multitenant_demo's design) ---
    let apex = config.apex_domain.clone();
    let app = axum::Router::new().fallback_service(tower::service_fn({
        let operator = operator_console.clone();
        let tenants = tenant_admin.clone();
        move |req: Request<Body>| {
            let mut operator = operator.clone();
            let mut tenants = tenants.clone();
            let apex = apex.clone();
            async move {
                let on_apex = super::resolver::host_is_apex(req.headers(), req.uri(), &apex);
                let response: Response<Body> = if on_apex {
                    operator.as_service().oneshot(req).await
                } else {
                    tenants.as_service().oneshot(req).await
                }
                .map_err(|e| -> std::convert::Infallible {
                    panic!("axum router service is Infallible: {e}")
                })?;
                Ok::<_, std::convert::Infallible>(response)
            }
        }
    }));

    // --- Listener + banner ---
    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
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
            assert!(is_loopback_host(host), "{host}");
        }
        for host in [
            "127.example.com",
            "acme.example.com:443",
            "[2001:db8::1]:80",
            "10.0.0.1",
        ] {
            assert!(!is_loopback_host(host), "{host}");
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
