//! Method dispatch. Slice 1 (#1014) implements the two methods needed to
//! complete a handshake: `initialize` and `ping`. Everything else returns
//! a JSON-RPC `method not found`. Tools / prompts / resources dispatch
//! lands in later slices, keyed off the same `match`.

use serde_json::{json, Value};

use super::router::McpState;
use super::types::{
    Implementation, InitializeResult, JsonRpcError, ServerCapabilities, PROTOCOL_VERSION,
};

/// Dispatch one JSON-RPC method to its result (or a JSON-RPC error).
///
/// `_state` carries the tenant pool + notification bus; Slice 1's two
/// methods don't need it, but later slices (`tools/call`, …) will.
pub(crate) async fn dispatch(
    _state: &McpState,
    method: &str,
    params: Option<Value>,
) -> Result<Value, JsonRpcError> {
    match method {
        "initialize" => initialize(params),
        "ping" => Ok(json!({})),
        other => Err(JsonRpcError::method_not_found(other)),
    }
}

/// The `initialize` handshake — advertise our protocol version,
/// capabilities (none in Slice 1), and identity. We do not hard-fail on a
/// client/server protocol-version mismatch: we return ours and let the
/// client decide, per the MCP lifecycle spec.
fn initialize(_params: Option<Value>) -> Result<Value, JsonRpcError> {
    let result = InitializeResult {
        protocol_version: PROTOCOL_VERSION,
        capabilities: ServerCapabilities::default(),
        server_info: Implementation {
            name: "rustango",
            version: env!("CARGO_PKG_VERSION"),
        },
    };
    serde_json::to_value(result)
        .map_err(|e| JsonRpcError::new(super::types::codes::INTERNAL_ERROR, e.to_string()))
}
