//! JSON-RPC 2.0 envelope + the MCP `initialize` handshake types.
//!
//! Slice 1 (#1014) keeps this deliberately small: the request/response
//! envelope, the standard JSON-RPC error codes, and the `initialize`
//! result shape. Tools / prompts / resources types land in later slices.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// MCP protocol revision this server speaks. Echoed back from
/// `initialize`; clients compare against their own and decide whether to
/// proceed.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// The `"2.0"` literal every JSON-RPC message must carry.
pub const JSONRPC_VERSION: &str = "2.0";

// ----------------------------------------------------------------- envelope

/// An inbound JSON-RPC request (or notification, when `id` is absent).
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcRequest {
    #[allow(dead_code)] // validated in the transport layer, not read here
    #[serde(default)]
    pub jsonrpc: String,
    /// Present for requests (server must reply), absent for notifications
    /// (server must NOT reply, per JSON-RPC 2.0 §4.1).
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    /// A request with no `id` is a notification: the server processes it
    /// but sends no response.
    #[must_use]
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// A successful or failed JSON-RPC response. Exactly one of `result` /
/// `error` is `Some`.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    /// Build a success response for the given request `id`.
    #[must_use]
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Build an error response. `id` is `Value::Null` when the offending
    /// request couldn't be parsed far enough to recover its id.
    #[must_use]
    pub fn failure(id: Value, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// A JSON-RPC error object. `code` follows the JSON-RPC 2.0 reserved
/// ranges (see [`codes`]).
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    #[must_use]
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    #[must_use]
    pub fn parse_error() -> Self {
        Self::new(codes::PARSE_ERROR, "parse error: invalid JSON")
    }

    #[must_use]
    pub fn invalid_request(detail: impl Into<String>) -> Self {
        Self::new(codes::INVALID_REQUEST, detail)
    }

    #[must_use]
    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            codes::METHOD_NOT_FOUND,
            format!("method not found: {method}"),
        )
    }

    #[must_use]
    pub fn invalid_params(detail: impl Into<String>) -> Self {
        Self::new(codes::INVALID_PARAMS, detail)
    }
}

/// JSON-RPC 2.0 reserved error codes (§5.1) plus the MCP-specific range.
pub mod codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    // MCP server-defined range (-32000..-32099).
    /// `tools/call` named a tool that isn't registered.
    pub const TOOL_NOT_FOUND: i64 = -32002;
    /// `tools/call` named a tool the agent isn't authorized for.
    pub const TOOL_FORBIDDEN: i64 = -32003;
}

// ------------------------------------------------------------- initialize

/// Result of the `initialize` handshake.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: &'static str,
    pub capabilities: ServerCapabilities,
    pub server_info: Implementation,
}

/// Server capability advertisement. Every field is omitted when `None`,
/// so Slice 1 (which supports none yet) serializes to `{}`. Later slices
/// flip `tools` / `prompts` / `resources` / `logging` / `completions` on.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ServerCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logging: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completions: Option<Value>,
}

/// Name + version of an MCP party (server or client).
#[derive(Debug, Clone, Serialize)]
pub struct Implementation {
    pub name: &'static str,
    pub version: &'static str,
}
