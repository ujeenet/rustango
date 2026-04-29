//! `manage run-server` — Django-style `runserver` for rustango.
//!
//! Boots a complete operator + tenant admin stack with sensible
//! defaults from env. Users running `cargo run --bin manage --
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
use axum::http::{header, Request, Response};
use crate::sql::sqlx::PgPool;
use tower::ServiceExt as _;

use crate::tenancy::admin::TenantAdminBuilder;
use super::error::TenancyError;
use super::operator_console::{self, SessionSecret};
use super::pools::TenantPools;
use super::resolver::ChainResolver;

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
            operator_show_only: vec![
                "rustango_orgs".into(),
                "rustango_operators".into(),
            ],
        }
    }
}

impl ServerConfig {
    /// Read overrides from env: `RUSTANGO_BIND` and
    /// `RUSTANGO_APEX_DOMAIN`. Anything unset uses the default.
    #[must_use]
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(v) = std::env::var("RUSTANGO_BIND") {
            cfg.bind = v;
        }
        if let Ok(v) = std::env::var("RUSTANGO_APEX_DOMAIN") {
            cfg.apex_domain = v;
        }
        cfg
    }
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
pub async fn run<W: Write + Send>(
    pools: Arc<TenantPools>,
    registry_url: String,
    config: ServerConfig,
    writer: &mut W,
) -> Result<(), TenancyError> {
    if !no_operators_warn(pools.registry(), writer).await? {
        // No operator accounts — the user can still bring up the
        // server, but they can't log in. Loud warning, then proceed
        // (someone else may be planning to create an operator while
        // the server is up).
    }

    // --- Shared signing key for operator + tenant cookies ---
    // Both consoles use HMAC-SHA256 over distinct cookie names + payload
    // shapes; sharing the key is safe and lets a single
    // RUSTANGO_SESSION_SECRET cover both surfaces.
    let session_secret = SessionSecret::from_env_or_random();

    // --- Operator console at the apex ---
    let operator_console =
        operator_console::router(pools.registry().clone(), session_secret.clone());

    // --- Tenant admin at subdomains (with per-tenant auth) ---
    let resolver = ChainResolver::standard(config.apex_domain.clone());
    let tenant_admin = TenantAdminBuilder::new(pools.clone(), registry_url, resolver)
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
                let host = req
                    .headers()
                    .get(header::HOST)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.split(':').next().unwrap_or(s).to_owned())
                    .unwrap_or_default();
                let response: Response<Body> = if host == apex {
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
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| TenancyError::Validation(format!("server error: {e}")))?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!(target: "crate::tenancy::server", "shutdown signal received");
}

/// Print a loud warning if no operators exist — the operator UI
/// would be unreachable in that state. Returns `Ok(true)` when at
/// least one is present.
async fn no_operators_warn<W: Write + Send>(
    registry: &PgPool,
    w: &mut W,
) -> Result<bool, TenancyError> {
    use crate::core::Column as _;
    use crate::sql::Fetcher;
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
            "         `manage create-operator <username> --password <p>`."
        )?;
        writeln!(w)?;
        Ok(false)
    } else {
        Ok(true)
    }
}
