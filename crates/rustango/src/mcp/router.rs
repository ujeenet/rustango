//! Mountable axum routers + the shared handler state.
//!
//! Two entry points, mirroring the rest of the framework's web modules:
//!
//! - [`router`] — single-tenant: bakes the app's [`Pool`] into state.
//! - [`tenant_router`] — multi-tenant: the per-request tenant pool is
//!   resolved by the [`crate::extractors::Tenant`] extractor inside the
//!   handlers (wired in Slice 2); the router itself carries no pool.
//!
//! Both return a `Router<()>` you `.merge(...)` into your API router and
//! hand to `Cli::api(...)` / tenancy `Builder::api(...)`.

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;

use crate::tenancy::jwt_lifecycle::JwtLifecycle;

use super::auth::{agent_token, default_jwt, post_authed};
use super::transport::{post_handler, sse_handler};

/// Shared state for the MCP handlers.
#[derive(Clone)]
pub(crate) struct McpState {
    /// Single-tenant pool. `None` under [`tenant_router`], where the
    /// per-request pool comes from the `Tenant` extractor instead.
    /// Stashed now; first read by `tools/call` in Slice 3 (#1016).
    #[allow(dead_code)]
    pub(crate) pool: Option<crate::sql::Pool>,
    /// Agent-JWT lifecycle (Slice 2). `Some` on the authed tenant router;
    /// `None` on the unauthed Slice-1 routers.
    pub(crate) jwt: Option<Arc<JwtLifecycle>>,
    /// Page size for the `*/list` methods (`[mcp].max_tools_listed`).
    /// `None`/0 ⇒ pagination off (single page). Follow-up #1089.
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

/// Single-tenant MCP router. Mount it under your chosen prefix, e.g.
/// `Router::new().nest("/mcp", rustango::mcp::router(pool))`.
#[must_use]
pub fn router(pool: crate::sql::Pool) -> Router {
    routes(McpState::new(Some(pool)))
}

/// Multi-tenant MCP router — for tenancy `Builder::api(...)` mounts, where
/// each request resolves its own tenant pool via the `Tenant` extractor.
/// Unauthenticated transport only (Slice 1); use [`tenant_router_authed`]
/// for the agent-guarded surface.
#[must_use]
pub fn tenant_router() -> Router {
    routes(McpState::new(None))
}

/// Multi-tenant MCP router **with agent auth** (Slice 2). Adds
/// `POST {prefix}/token` (client-credentials `{name, secret}` → scoped
/// JWT) and guards the JSON-RPC `POST {prefix}` behind a tenant-pinned
/// agent token. Signs with `RUSTANGO_SESSION_SECRET` (see [`default_jwt`]).
#[must_use]
pub fn secure_tenant_router() -> Router {
    tenant_router_authed(default_jwt())
}

/// Defensive cap on a JSON-RPC request body (tighter than axum's 2 MiB
/// default). MCP messages are small; this bounds a hostile oversized payload.
const MCP_MAX_BODY_BYTES: usize = 1024 * 1024;

/// Agent-guarded tenant router configured from `[mcp]` settings. Applies the
/// full knob set (#1098): `token_ttl_secs` → agent-token lifetime,
/// `max_tools_listed` → `*/list` page size, `enable_sse` → whether the
/// `GET {prefix}` stream is mounted, `allowed_origins` → a CORS layer,
/// `rate_limit_per_minute` → a per-IP rate limit, plus a defensive body cap.
/// Signs with `RUSTANGO_SESSION_SECRET` (see [`default_jwt`]). Slice 6 (#1019).
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

    // CORS allow-list — empty means no layer (same-origin only).
    if !settings.allowed_origins.is_empty() {
        use crate::cors::{CorsLayer, CorsRouterExt};
        router = router.cors(CorsLayer::new().allow_origins(settings.allowed_origins.clone()));
    }
    // Per-IP rate limit (`capacity` requests per 60s window).
    if let Some(rpm) = settings.rate_limit_per_minute {
        use crate::rate_limit::{RateLimitLayer, RateLimitRouterExt};
        router = router.rate_limit(RateLimitLayer::per_ip(
            rpm,
            std::time::Duration::from_secs(60),
        ));
    }
    // Defensive body cap on every MCP route.
    {
        use crate::body_limit::{BodyLimitLayer, BodyLimitRouterExt};
        router = router.body_limit(BodyLimitLayer::new(MCP_MAX_BODY_BYTES));
    }
    router
}

/// Like [`secure_tenant_router`] but with a caller-supplied
/// [`JwtLifecycle`] — set a stable secret, custom TTLs, or a shared
/// (Redis/DB) `JtiStore` for multi-instance revocation. Also the seam the
/// tests issue + revoke through. SSE is enabled (use
/// [`secure_tenant_router_from_settings`] to gate it via `enable_sse`).
#[must_use]
pub fn tenant_router_authed(jwt: Arc<JwtLifecycle>) -> Router {
    let state = McpState {
        jwt: Some(jwt),
        ..McpState::new(None)
    };
    authed_routes(state, true)
}

/// The agent-guarded route set: JSON-RPC on `/` (plus the SSE stream when
/// `enable_sse`), the bespoke `/token`, and the OAuth 2.1 discovery +
/// `client_credentials` endpoints (#1088).
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
