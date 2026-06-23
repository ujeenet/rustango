//! Model Context Protocol (MCP) server — expose rustango apps to external
//! ML agents over JSON-RPC 2.0 / Streamable HTTP.
//!
//! Epic #1013. **Slice 1 (#1014)** ships the foundation: the `src/mcp/`
//! module + `mcp` Cargo feature, the JSON-RPC 2.0 envelope, the
//! Streamable-HTTP transport (`POST` request / `GET` SSE notification
//! stream), and the `initialize` + `ping` methods. No auth, tools,
//! skills, prompts, or resources yet — those land in Slices 2–6.
//!
//! ## Mounting
//!
//! ```ignore
//! // Single-tenant:
//! let api = axum::Router::new().nest("/mcp", rustango::mcp::router(pool));
//! rustango::manage::Cli::new().api(api).run().await?;
//!
//! // Multi-tenant (tenancy Builder injects the TenantContext):
//! let api = axum::Router::new().nest("/mcp", rustango::mcp::tenant_router());
//! ```
//!
//! An MCP client then completes the lifecycle handshake — `initialize`
//! (capability negotiation) followed by a `notifications/initialized`
//! ack — and can `ping` to check liveness, all over the single `POST`
//! endpoint.

pub mod auth;
mod handlers;
pub mod notifications;
pub mod pagination;
pub mod progress;
pub mod resources;
mod router;
pub mod tools;
mod transport;
mod types;
pub mod utilities;

pub use auth::{issue_agent_token, verify_agent_token, McpAgent};
pub use notifications::{
    notify_prompts_list_changed, notify_resources_list_changed, notify_tools_list_changed,
};
pub use progress::{cancel, CancelToken, ProgressReporter};
pub use resources::{get_prompt, list_prompts, list_resources, read_resource, McpResource};
#[cfg(feature = "config")]
pub use router::secure_tenant_router_from_settings;
pub use router::{router, secure_tenant_router, tenant_router, tenant_router_authed};
pub use tools::{
    call_tool, list_tools, McpContext, McpError, McpTool, McpToolFuture, McpToolHandler,
};
// Hidden, but must be `pub` for the `register_mcp_tool!` expansion.
#[doc(hidden)]
pub use tools::{JsonValue, __deserialize_args, __schema_of};
pub use types::{
    codes, Implementation, InitializeResult, JsonRpcError, JsonRpcRequest, JsonRpcResponse,
    ServerCapabilities, JSONRPC_VERSION, PROTOCOL_VERSION,
};
