//! Method dispatch. Slice 1 (#1014) implements the two methods needed to
//! complete a handshake: `initialize` and `ping`. Everything else returns
//! a JSON-RPC `method not found`. Tools / prompts / resources dispatch
//! lands in later slices, keyed off the same `match`.

use serde_json::{json, Value};

use super::pagination::paginate;
use super::router::McpState;
use super::tools::{list_tools, McpContext};

/// Read the opaque `cursor` param for a `*/list` call, if present.
fn cursor_of(params: &Option<Value>) -> Option<&str> {
    params
        .as_ref()
        .and_then(|p| p.get("cursor"))
        .and_then(Value::as_str)
}
use super::types::{
    codes, Implementation, InitializeResult, JsonRpcError, ServerCapabilities, PROTOCOL_VERSION,
};

/// Dispatch one JSON-RPC method to its result (or a JSON-RPC error).
///
/// `ctx` is the authenticated agent context — `Some` on the agent-guarded
/// tenant router, `None` on the unauthed Slice-1 routers. `initialize` /
/// `ping` don't need it; the `tools/*` methods require it (fail-closed).
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
            )
        }
        "tools/call" => {
            let ctx = ctx.ok_or_else(auth_required)?;
            super::tools::call_tool_with(ctx, params.unwrap_or_else(|| json!({})), request_id).await
        }
        "prompts/list" => {
            let ctx = ctx.ok_or_else(auth_required)?;
            let full = super::resources::list_prompts(&ctx).await?;
            paginate(full, "prompts", cursor_of(&params), state.page_size)
        }
        "prompts/get" => {
            let ctx = ctx.ok_or_else(auth_required)?;
            super::resources::get_prompt(&ctx, params.unwrap_or_else(|| json!({}))).await
        }
        "resources/list" => {
            let ctx = ctx.ok_or_else(auth_required)?;
            let full = super::resources::list_resources(&ctx).await?;
            paginate(full, "resources", cursor_of(&params), state.page_size)
        }
        "resources/read" => {
            let ctx = ctx.ok_or_else(auth_required)?;
            super::resources::read_resource(&ctx, params.unwrap_or_else(|| json!({}))).await
        }
        // We expose no URI templates, but the spec method must exist (return an
        // empty list) rather than 404 with `method not found` (#1099).
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

/// The `tools/*` methods are only reachable with a verified agent — the
/// unauthed routers have no principal to authorize against.
fn auth_required() -> JsonRpcError {
    JsonRpcError::new(
        codes::INVALID_REQUEST,
        "tools require an authenticated agent (mount the agent-guarded MCP router)",
    )
}

/// The `initialize` handshake — advertise our protocol version,
/// capabilities (none in Slice 1), and identity. We do not hard-fail on a
/// client/server protocol-version mismatch: we return ours and let the
/// client decide, per the MCP lifecycle spec.
fn initialize(_params: Option<Value>) -> Result<Value, JsonRpcError> {
    let result = InitializeResult {
        protocol_version: PROTOCOL_VERSION,
        capabilities: ServerCapabilities {
            // Slices 3 + 5 light up tools / prompts / resources.
            // `listChanged` is wired by follow-up #1087; false until then.
            // Follow-up #1087: the server emits `*/list_changed` over SSE
            // for in-process changes, so `listChanged` is advertised true.
            tools: Some(json!({ "listChanged": true })),
            prompts: Some(json!({ "listChanged": true })),
            resources: Some(json!({ "listChanged": true, "subscribe": false })),
            // Follow-up #1091: logging + completion utilities.
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
