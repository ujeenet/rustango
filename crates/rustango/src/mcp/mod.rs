//! A Model Context Protocol server, so an outside agent can use your
//! app over JSON-RPC 2.0 and Streamable HTTP.
//!
//! What is here: the JSON-RPC envelope, the transport (`POST` for
//! requests, `GET` for an SSE notification stream), the
//! `initialize` and `ping` methods, agent authentication with scoped
//! JWTs, [`tools`](crate::mcp::tools), [`resources`](crate::mcp::resources)
//! and prompts, [`progress`](crate::mcp::progress) reporting with
//! cancellation, [`pagination`](crate::mcp::pagination),
//! [`oauth`](crate::mcp::oauth) discovery, and the
//! [`utilities`](crate::mcp::utilities) methods.
//!
//! Behind the `mcp` Cargo feature.
//!
//! ## Mounting
//!
//! ```ignore
//! // One tenant:
//! let api = axum::Router::new().nest("/mcp", rustango::mcp::router(pool));
//! rustango::manage::Cli::new().api(api).run().await?;
//!
//! // Many tenants, where the tenancy Builder injects the context:
//! let api = axum::Router::new().nest("/mcp", rustango::mcp::tenant_router());
//! ```
//!
//! Use [`secure_tenant_router`](crate::mcp::secure_tenant_router)
//! instead to require an agent token.
//! Either way, the client starts with `initialize` to agree on
//! capabilities, acks with `notifications/initialized`, and can then
//! `ping` to check the server is alive.

pub mod auth;
mod handlers;
pub mod notifications;
pub mod oauth;
pub mod pagination;
pub mod progress;
pub mod resources;
mod router;
pub mod tools;
mod transport;
mod types;
pub mod utilities;

pub use auth::{issue_agent_token, verify_agent_token, verify_raw_agent_credential, McpAgent};
pub use notifications::{
    notify_prompts_list_changed, notify_resources_list_changed, notify_tools_list_changed,
};
pub use oauth::{authorization_server_metadata, protected_resource_metadata};
pub use progress::{cancel, CancelToken, ProgressReporter};
pub use resources::{get_prompt, list_prompts, list_resources, read_resource, McpResource};
#[cfg(feature = "config")]
pub use router::secure_tenant_router_from_settings;
pub use router::{router, secure_tenant_router, tenant_router, tenant_router_authed};
pub use tools::{
    call_tool, list_tools, McpContext, McpError, McpTool, McpToolFuture, McpToolHandler,
};
// Hidden, but `pub` because `register_mcp_tool!` expands to it.
#[doc(hidden)]
pub use tools::{JsonValue, __deserialize_args, __schema_of};
pub use types::{
    codes, Implementation, InitializeResult, JsonRpcError, JsonRpcRequest, JsonRpcResponse,
    ServerCapabilities, JSONRPC_VERSION, PROTOCOL_VERSION,
};
