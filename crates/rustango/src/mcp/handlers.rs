//! Method dispatch: one `match` over the JSON-RPC method name.
//!
//! `initialize` and `ping` need no agent. Every other method does,
//! and is refused without one. An unknown name returns
//! `method not found`.

use serde_json::{json, Value};

use super::pagination::paginate;
use super::router::McpState;
use super::tools::{list_tools, McpContext};

/// Read the `cursor` param of a `*/list` call, if it has one.
fn cursor_of(params: &Option<Value>) -> Option<&str> {
    params
        .as_ref()
        .and_then(|p| p.get("cursor"))
        .and_then(Value::as_str)
}
use super::types::{
    codes, Implementation, InitializeResult, JsonRpcError, ServerCapabilities, PROTOCOL_VERSION,
};

/// Run one JSON-RPC method and return its result, or an error.
///
/// `ctx` is the authenticated agent. It is `Some` only on a router
/// that authenticates. `initialize` and `ping` work without it;
/// everything else fails closed.
pub(crate) async fn dispatch(
    state: &McpState,
    method: &str,
    params: Option<Value>,
    ctx: Option<McpContext>,
    request_id: Option<&str>,
) -> Result<Value, JsonRpcError> {
    match method {
        "initialize" => initialize(params),
        "ping" => Ok(json!({})),
        "tools/list" => {
            let ctx = ctx.ok_or_else(auth_required)?;
            paginate(
                list_tools(&ctx.agent),
                "tools",
                cursor_of(&params),
                state.page_size,
                ctx.agent.agent_id,
            )
        }
        "tools/call" => {
            let ctx = ctx.ok_or_else(auth_required)?;
            super::tools::call_tool_with(ctx, params.unwrap_or_else(|| json!({})), request_id).await
        }
        "prompts/list" => {
            let ctx = ctx.ok_or_else(auth_required)?;
            let full = super::resources::list_prompts(&ctx).await?;
            let agent_id = ctx.agent.agent_id;
            paginate(
                full,
                "prompts",
                cursor_of(&params),
                state.page_size,
                agent_id,
            )
        }
        "prompts/get" => {
            let ctx = ctx.ok_or_else(auth_required)?;
            super::resources::get_prompt(&ctx, params.unwrap_or_else(|| json!({}))).await
        }
        "resources/list" => {
            let ctx = ctx.ok_or_else(auth_required)?;
            let full = super::resources::list_resources(&ctx).await?;
            let agent_id = ctx.agent.agent_id;
            paginate(
                full,
                "resources",
                cursor_of(&params),
                state.page_size,
                agent_id,
            )
        }
        "resources/read" => {
            let ctx = ctx.ok_or_else(auth_required)?;
            super::resources::read_resource(&ctx, params.unwrap_or_else(|| json!({}))).await
        }
        // No URI templates are exposed, but the method has to exist
        // and return an empty list rather than `method not found`.
        "resources/templates/list" => {
            ctx.ok_or_else(auth_required)?;
            Ok(json!({ "resourceTemplates": [] }))
        }
        "logging/setLevel" => {
            ctx.ok_or_else(auth_required)?;
            super::utilities::set_log_level(params.unwrap_or_else(|| json!({})))
        }
        "completion/complete" => {
            let ctx = ctx.ok_or_else(auth_required)?;
            super::utilities::complete(&ctx, params.unwrap_or_else(|| json!({}))).await
        }
        other => Err(JsonRpcError::method_not_found(other)),
    }
}

/// The error for a method that needs an agent when there is none. A
/// router that does not authenticate has nobody to authorize.
fn auth_required() -> JsonRpcError {
    JsonRpcError::new(
        codes::INVALID_REQUEST,
        "tools require an authenticated agent (mount the agent-guarded MCP router)",
    )
}

/// The `initialize` handshake: report our protocol version, our
/// capabilities and our identity.
///
/// A version mismatch is not an error here. We return ours and let
/// the client decide, as the MCP lifecycle spec says.
fn initialize(_params: Option<Value>) -> Result<Value, JsonRpcError> {
    let result = InitializeResult {
        protocol_version: PROTOCOL_VERSION,
        capabilities: ServerCapabilities {
            // `listChanged` is true because the server does emit
            // `*/list_changed` over SSE for in-process changes.
            tools: Some(json!({ "listChanged": true })),
            prompts: Some(json!({ "listChanged": true })),
            resources: Some(json!({ "listChanged": true, "subscribe": false })),
            logging: Some(json!({})),
            completions: Some(json!({})),
        },
        server_info: Implementation {
            name: "rustango",
            version: env!("CARGO_PKG_VERSION"),
        },
    };
    serde_json::to_value(result)
        .map_err(|e| JsonRpcError::new(super::types::codes::INTERNAL_ERROR, e.to_string()))
}
