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

use axum::routing::post;
use axum::Router;

use crate::sse::EventBus;

use super::transport::{post_handler, sse_handler};

/// Shared state for the MCP handlers.
#[derive(Clone)]
pub(crate) struct McpState {
    /// Single-tenant pool. `None` under [`tenant_router`], where the
    /// per-request pool comes from the `Tenant` extractor instead.
    /// Stashed now; first read by `tools/call` in Slice 3 (#1016).
    #[allow(dead_code)]
    pub(crate) pool: Option<crate::sql::Pool>,
    /// Server→client notification bus. Frames pushed here are relayed to
    /// every open SSE stream. Empty in Slice 1.
    pub(crate) bus: EventBus<String>,
}

impl McpState {
    fn new(pool: Option<crate::sql::Pool>) -> Self {
        Self {
            pool,
            // 256-frame buffer: generous enough that a briefly-stalled
            // client doesn't immediately lag out of the broadcast window.
            bus: EventBus::new(256),
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
#[must_use]
pub fn tenant_router() -> Router {
    routes(McpState::new(None))
}
