//! Method dispatch. Slice 1 (#1014) implements the two methods needed to
//! complete a handshake: `initialize` and `ping`. Everything else returns
//! a JSON-RPC `method not found`. Tools / prompts / resources dispatch
//! lands in later slices, keyed off the same `match`.

use serde_json::{json, Value};

use super::router::McpState;
use super::tools::{call_tool, list_tools, McpContext};
use super::types::{
    codes, Implementation, InitializeResult, JsonRpcError, ServerCapabilities, PROTOCOL_VERSION,
};

/// Dispatch one JSON-RPC method to its result (or a JSON-RPC error).
///
/// `ctx` is the authenticated agent context — `Some` on the agent-guarded
/// tenant router, `None` on the unauthed Slice-1 routers. `initialize` /
/// `ping` don't need it; the `tools/*` methods require it (fail-closed).
pub(crate) async fn dispatch(
    _state: &McpState,
    method: &str,
    params: Option<Value>,
    ctx: Option<McpContext>,
) -> Result<Value, JsonRpcError> {
    match method {
        "initialize" => initialize(params),
        "ping" => Ok(json!({})),
        "tools/list" => {
            let ctx = ctx.ok_or_else(auth_required)?;
            Ok(list_tools(&ctx.agent))
        }
        "tools/call" => {
            let ctx = ctx.ok_or_else(auth_required)?;
            call_tool(ctx, params.unwrap_or_else(|| json!({}))).await
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
            // Slice 3 lights up tools. `listChanged` is wired by the
            // follow-up #1087; advertise it as false until then.
            tools: Some(json!({ "listChanged": false })),
            ..ServerCapabilities::default()
        },
        server_info: Implementation {
            name: "rustango",
            version: env!("CARGO_PKG_VERSION"),
        },
    };
    serde_json::to_value(result)
        .map_err(|e| JsonRpcError::new(super::types::codes::INTERNAL_ERROR, e.to_string()))
}
