//! The axum routers you can mount, and the state their handlers
//! share.
//!
//! - [`router`] for one tenant: it holds the app's
//!   [`crate::sql::Pool`].
//! - [`tenant_router`] for many: it holds no pool, because the
//!   [`crate::extractors::Tenant`] extractor resolves one per
//!   request.
//! - [`secure_tenant_router`], and
//!   `secure_tenant_router_from_settings` behind the `config`
//!   feature, do the same but require an agent token.
//!
//! Each returns a `Router<()>` to `.merge(...)` into your API router
//! and pass to `Cli::api(...)` or the tenancy `Builder::api(...)`.

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;

use crate::tenancy::jwt_lifecycle::JwtLifecycle;

use super::auth::{agent_token, default_jwt, post_authed};
use super::transport::{post_handler, sse_handler};

/// Shared state for the MCP handlers.
#[derive(Clone)]
pub(crate) struct McpState {
    /// The app's pool. `None` under [`tenant_router`], where the
    /// `Tenant` extractor supplies one per request.
    #[allow(dead_code)]
    pub(crate) pool: Option<crate::sql::Pool>,
    /// Agent-token lifecycle. `Some` only on a router that
    /// authenticates.
    pub(crate) jwt: Option<Arc<JwtLifecycle>>,
    /// Page size for the `*/list` methods, from
    /// `[mcp].max_tools_listed`. `None` or 0 means one page with
    /// everything in it.
    pub(crate) page_size: Option<usize>,
}

impl McpState {
    fn new(pool: Option<crate::sql::Pool>) -> Self {
        Self {
            pool,
            jwt: None,
            page_size: None,
        }
    }
}

fn routes(state: McpState) -> Router {
    Router::new()
        .route("/", post(post_handler).get(sse_handler))
        .with_state(state)
}

/// The MCP router for a single-tenant app. Mount it under the prefix
/// you want: `Router::new().nest("/mcp", rustango::mcp::router(pool))`.
#[must_use]
pub fn router(pool: crate::sql::Pool) -> Router {
    routes(McpState::new(Some(pool)))
}

/// The MCP router for a multi-tenant app, mounted through the
/// tenancy `Builder::api(...)`. Each request resolves its own pool
/// with the `Tenant` extractor.
///
/// This one does **not** authenticate. Use [`secure_tenant_router`]
/// or [`tenant_router_authed`] to require an agent token.
#[must_use]
pub fn tenant_router() -> Router {
    routes(McpState::new(None))
}

/// [`tenant_router`] **plus agent auth.** It adds
/// `POST {prefix}/token`, which trades a `{name, secret}` pair for a
/// scoped JWT, and puts the JSON-RPC endpoint behind a token pinned
/// to one tenant. Tokens are signed with `RUSTANGO_SESSION_SECRET`;
/// see [`default_jwt`].
#[must_use]
pub fn secure_tenant_router() -> Router {
    tenant_router_authed(default_jwt())
}

/// [`secure_tenant_router`], configured from the `[mcp]` settings
/// section. Every field there applies: `token_ttl_secs` sets the
/// token lifetime, `max_tools_listed` the `*/list` page size,
/// `enable_sse` whether the notification stream is mounted,
/// `allowed_origins` a CORS layer, `rate_limit_per_minute` a per-IP
/// limit, and `max_body_bytes` a body cap, which defaults to 1 MiB.
#[cfg(feature = "config")]
#[must_use]
pub fn secure_tenant_router_from_settings(settings: &crate::config::McpSettings) -> Router {
    let jwt = Arc::new(
        JwtLifecycle::new(super::auth::jwt_secret()).with_access_ttl(settings.token_ttl_secs()),
    );
    let state = McpState {
        jwt: Some(jwt),
        page_size: settings.max_tools_listed,
        ..McpState::new(None)
    };
    let mut router = authed_routes(state, settings.sse_enabled());

    // An empty allow-list means no CORS layer, so same-origin only.
    if !settings.allowed_origins.is_empty() {
        use crate::cors::{CorsLayer, CorsRouterExt};
        router = router.cors(CorsLayer::new().allow_origins(settings.allowed_origins.clone()));
    }
    // Per-IP limit, counted over a 60-second window.
    if let Some(rpm) = settings.rate_limit_per_minute {
        use crate::rate_limit::{RateLimitLayer, RateLimitRouterExt};
        router = router.rate_limit(RateLimitLayer::per_ip(
            rpm,
            std::time::Duration::from_secs(60),
        ));
    }
    // Body cap on every MCP route.
    {
        use crate::body_limit::{BodyLimitLayer, BodyLimitRouterExt};
        router = router.body_limit(BodyLimitLayer::new(settings.max_body_bytes()));
    }
    router
}

/// [`secure_tenant_router`] with a [`JwtLifecycle`] you supply, for a
/// stable secret, your own TTLs, or a shared `JtiStore` so revoking a
/// token affects every instance. Tests issue and revoke through this.
///
/// The notification stream is always mounted here. Use the settings
/// variant to gate it.
#[must_use]
pub fn tenant_router_authed(jwt: Arc<JwtLifecycle>) -> Router {
    let state = McpState {
        jwt: Some(jwt),
        ..McpState::new(None)
    };
    authed_routes(state, true)
}

/// The authenticated route set: JSON-RPC on `/`, the notification
/// stream when `enable_sse`, `/token`, and the OAuth 2.1 discovery
/// and `client_credentials` endpoints.
fn authed_routes(state: McpState, enable_sse: bool) -> Router {
    use super::oauth;
    let root = if enable_sse {
        post(post_authed).get(sse_handler)
    } else {
        post(post_authed)
    };
    Router::new()
        .route("/", root)
        .route("/token", post(agent_token))
        .route("/oauth/token", post(oauth::oauth_token))
        .route(
            "/.well-known/oauth-protected-resource",
            get(oauth::well_known_protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(oauth::well_known_authorization_server),
        )
        .with_state(state)
}
