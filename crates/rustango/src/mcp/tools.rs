//! Tool registry + `tools/list` / `tools/call` (epic #1013, Slice 3 / #1016).
//!
//! Tools are registered at compile time with [`register_mcp_tool!`], which
//! submits an [`McpTool`] into an `inventory` collection — the same
//! const-constructible fn-pointer pattern as `register_admin_view!`
//! (handlers are bare `fn`s wrapped in a non-capturing inner fn, since
//! `inventory::submit!` storage can't hold captures).
//!
//! Each tool declares a typed input (`T: OpenApiSchema + DeserializeOwned`):
//! `OpenApiSchema` produces the MCP `inputSchema`, and incoming JSON args
//! are validated by deserializing into `T` before the handler runs — a
//! malformed call is rejected and the handler never executes.

use std::future::Future;
use std::pin::Pin;

use serde_json::{json, Value};

use crate::sql::Pool;

use super::auth::McpAgent;
use super::types::{codes, JsonRpcError};

#[doc(hidden)]
pub type JsonValue = Value;

/// Per-request context handed to every tool handler: the resolved tenant
/// pool + the authenticated agent principal.
pub struct McpContext {
    /// The request's tenant pool.
    pub pool: Pool,
    /// The verified, tenant-pinned agent making the call.
    pub agent: McpAgent,
    /// Progress sink for this call (active only when the caller sent a
    /// `progressToken`). Follow-up #1090.
    pub progress: super::progress::ProgressReporter,
    /// Cooperative cancellation flag — poll `is_cancelled()` at await
    /// points. Follow-up #1090.
    pub cancel: super::progress::CancelToken,
}

/// Error a tool handler may return. Converts to a JSON-RPC error response.
#[derive(Debug)]
pub struct McpError {
    pub code: i64,
    pub message: String,
}

impl McpError {
    #[must_use]
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    /// Bad/invalid tool arguments (`-32602`).
    #[must_use]
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(codes::INVALID_PARAMS, message)
    }
    /// A tool-side internal failure (`-32603`).
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(codes::INTERNAL_ERROR, message)
    }
    fn into_jsonrpc(self) -> JsonRpcError {
        JsonRpcError::new(self.code, self.message)
    }
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for McpError {}

/// DB errors inside a handler surface as internal tool errors.
impl From<crate::sql::ExecError> for McpError {
    fn from(e: crate::sql::ExecError) -> Self {
        Self::internal(e.to_string())
    }
}

/// Boxed future a tool handler returns.
pub type McpToolFuture = Pin<Box<dyn Future<Output = Result<Value, McpError>> + Send + 'static>>;

/// Stored handler — a bare `fn` pointer (const-constructible for
/// `inventory::submit!`); [`register_mcp_tool!`] wraps the user's closure.
pub type McpToolHandler = fn(McpContext, Value) -> McpToolFuture;

/// A compile-time-registered MCP tool.
pub struct McpTool {
    /// Unique tool name (the `tools/call` `name`).
    pub name: &'static str,
    /// Human-readable description shown in `tools/list`.
    pub description: &'static str,
    /// Produces the JSON-Schema `inputSchema` (from the tool's typed input).
    pub input_schema: fn() -> Value,
    /// The (arg-validating) handler.
    pub handler: McpToolHandler,
}

inventory::collect!(McpTool);

// Hidden helpers the macro calls so its expansion never has to name
// `serde_json` / `OpenApiSchema` directly (works in any downstream crate).
#[doc(hidden)]
pub fn __schema_of<T: crate::openapi::OpenApiSchema>() -> Value {
    serde_json::to_value(T::openapi_schema()).unwrap_or_else(|_| json!({ "type": "object" }))
}

#[doc(hidden)]
pub fn __deserialize_args<T: serde::de::DeserializeOwned>(raw: Value) -> Result<T, McpError> {
    serde_json::from_value(raw)
        .map_err(|e| McpError::invalid_params(format!("invalid arguments: {e}")))
}

/// Look up a registered tool by name.
#[must_use]
pub(crate) fn find_tool(name: &str) -> Option<&'static McpTool> {
    inventory::iter::<McpTool>
        .into_iter()
        .find(|t| t.name == name)
}

/// `tools/list` result — `{ "tools": [ {name, description, inputSchema} ] }`.
/// Fail-closed (Slice 4): only the tools in the agent's granted set are
/// listed. An agent with no grants sees an empty list.
#[must_use]
pub fn list_tools(agent: &McpAgent) -> Value {
    let tools: Vec<Value> = inventory::iter::<McpTool>
        .into_iter()
        .filter(|t| agent.tools.iter().any(|n| n == t.name))
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": (t.input_schema)(),
            })
        })
        .collect();
    json!({ "tools": tools })
}

/// `tools/call` — find the tool, authorize it for the agent (fail-closed),
/// validate args (inside the handler), run it, and wrap the result as an
/// MCP `CallToolResult`. Records a best-effort audit entry.
///
/// # Errors
/// JSON-RPC errors for a missing/forbidden tool or invalid arguments; the
/// tool never executes in those cases.
pub async fn call_tool(ctx: McpContext, params: Value) -> Result<Value, JsonRpcError> {
    call_tool_with(ctx, params, None).await
}

/// [`call_tool`] plus the JSON-RPC `request_id`, used by the dispatcher to
/// register the call for cancellation (`notifications/cancelled`) and to
/// wire the `progressToken`. Follow-up #1090.
pub(crate) async fn call_tool_with(
    mut ctx: McpContext,
    params: Value,
    request_id: Option<&str>,
) -> Result<Value, JsonRpcError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| JsonRpcError::invalid_params("tools/call requires a string `name`"))?
        .to_owned();
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let tool = find_tool(&name)
        .ok_or_else(|| JsonRpcError::new(codes::TOOL_NOT_FOUND, format!("unknown tool: {name}")))?;

    // Fail-closed authorization: the agent's granted tool set (resolved from
    // its skills at token-issue) is authoritative. A tool outside it — or any
    // tool for an agent with no grants — is refused and never executed.
    if !ctx.agent.tools.iter().any(|n| n == &name) {
        return Err(JsonRpcError::new(
            codes::TOOL_FORBIDDEN,
            format!("tool `{name}` is not authorized for this agent"),
        ));
    }

    // Keep what we need (audit + scoping) before `ctx` moves into the tool.
    let pool = ctx.pool.clone();
    let tenant = ctx.agent.tenant.clone();
    let agent_id = ctx.agent.agent_id;

    // Wire progress (from `_meta.progressToken`) + cancellation, both scoped to
    // the calling agent. The cancel registration is RAII (`CancelGuard`) keyed
    // by `(tenant, agent_id, request_id)`, so it can't be tripped by another
    // agent and is removed on drop even if the handler panics (#1095).
    ctx.progress = super::progress::ProgressReporter::for_agent(
        super::progress::progress_token(&params),
        tenant.clone(),
        agent_id,
    );
    let mut _cancel_guard = None;
    if let Some(id) = request_id {
        let guard = super::progress::CancelGuard::register(&tenant, agent_id, id);
        ctx.cancel = guard.token();
        _cancel_guard = Some(guard);
    }

    // Run the handler under a panic guard: a buggy `register_mcp_tool!` handler
    // that panics must not unwind into the transport (dropping the connection /
    // DoS) — convert it to an internal error instead (#1096). The `_cancel_guard`
    // still deregisters when this fn returns.
    let result = match catch_unwind((tool.handler)(ctx, args.clone())).await {
        Ok(r) => r,
        Err(_panic) => {
            tracing::error!(tool = %name, agent_id, "mcp tool handler panicked");
            Err(McpError::internal("tool handler panicked"))
        }
    };
    let result = result.map_err(McpError::into_jsonrpc)?;

    audit_tool_call(&pool, agent_id, &name, &args).await;
    Ok(call_tool_result(result))
}

/// Poll `fut` to completion, catching a panic from any individual `poll` and
/// returning it as `Err`. A dependency-free async `catch_unwind` (we don't pull
/// in `futures` just for this); the tool future is `Send + 'static`, and a
/// caught panic surfaces as a `std::thread::Result::Err`. (#1096)
async fn catch_unwind<F: Future>(fut: F) -> std::thread::Result<F::Output> {
    use std::task::Poll;
    let mut fut = Box::pin(fut);
    std::future::poll_fn(move |cx| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fut.as_mut().poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(v)) => Poll::Ready(Ok(v)),
            Err(panic) => Poll::Ready(Err(panic)),
        }
    })
    .await
}

/// Wrap a handler's JSON result in an MCP `CallToolResult`. The value is
/// rendered both as a text content block (spec-required) and as
/// `structuredContent` for clients that consume it directly.
fn call_tool_result(value: Value) -> Value {
    let text = match &value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": value,
        "isError": false,
    })
}

/// Key names whose values are scrubbed before a tool call's arguments are
/// written to the audit log — secrets must never land in the audit table
/// (#1097). Mirrors the framework's access-log redaction set
/// (`access_log::default_redact_params`) plus a few MCP-relevant names.
const SENSITIVE_ARG_KEYS: &[&str] = &[
    "password",
    "passwd",
    "token",
    "secret",
    "api_key",
    "apikey",
    "access_token",
    "refresh_token",
    "signature",
    "auth",
    "authorization",
    "client_secret",
    "private_key",
];

/// Recursively redact the values of sensitive keys in `value`, returning a
/// scrubbed clone. Key matching is case-insensitive and exact (so `token_count`
/// is *not* redacted); a matched key's entire value becomes `"[redacted]"`.
fn redact_json(value: &Value) -> Value {
    fn is_sensitive(key: &str) -> bool {
        let k = key.to_ascii_lowercase();
        SENSITIVE_ARG_KEYS.iter().any(|s| *s == k)
    }
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    if is_sensitive(k) {
                        (k.clone(), Value::String("[redacted]".into()))
                    } else {
                        (k.clone(), redact_json(v))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact_json).collect()),
        other => other.clone(),
    }
}

/// Best-effort audit of a tool invocation (never fails the call). Arguments are
/// redacted (#1097) so a tool taking a `password`/`token`/… never leaks it into
/// the audit table.
async fn audit_tool_call(pool: &Pool, agent_id: i64, tool: &str, args: &Value) {
    let entry = crate::audit::PendingEntry {
        entity_table: "rustango_agents",
        entity_pk: agent_id.to_string(),
        operation: crate::audit::AuditOp::Action,
        source: crate::audit::AuditSource::Custom(format!("mcp:agent:{agent_id}")),
        changes: json!({ "tool": tool, "arguments": redact_json(args) }),
    };
    if let Err(e) = crate::audit::emit_one_pool(pool, &entry).await {
        tracing::debug!(error = %e, tool, "mcp tools/call audit not recorded");
    }
}

/// Register a tool at compile time.
///
/// ```ignore
/// #[derive(serde::Deserialize)]
/// struct AddInput { a: i64, b: i64 }
/// impl rustango::openapi::OpenApiSchema for AddInput { /* ... */ }
///
/// rustango::register_mcp_tool!(
///     "add", "Add two integers", AddInput,
///     |_ctx: rustango::mcp::McpContext, input: AddInput| async move {
///         Ok(serde_json::json!({ "sum": input.a + input.b }))
///     },
/// );
/// ```
#[macro_export]
macro_rules! register_mcp_tool {
    ($name:expr, $description:expr, $input:ty, $handler:expr $(,)?) => {
        $crate::inventory::submit! {
            $crate::mcp::McpTool {
                name: $name,
                description: $description,
                input_schema: {
                    fn __rustango_mcp_input_schema() -> $crate::mcp::JsonValue {
                        $crate::mcp::__schema_of::<$input>()
                    }
                    __rustango_mcp_input_schema
                },
                handler: {
                    fn __rustango_mcp_tool_handler(
                        ctx: $crate::mcp::McpContext,
                        raw: $crate::mcp::JsonValue,
                    ) -> $crate::mcp::McpToolFuture {
                        ::std::boxed::Box::pin(async move {
                            let input = $crate::mcp::__deserialize_args::<$input>(raw)?;
                            ($handler)(ctx, input).await
                        })
                    }
                    __rustango_mcp_tool_handler
                },
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_json_scrubs_sensitive_keys_recursively() {
        let args = json!({
            "username": "alice",
            "password": "hunter2",
            "API_KEY": "sk-123",
            "token_count": 42,                       // not sensitive (exact match)
            "nested": { "client_secret": "shh", "ok": true },
            "list": [ { "secret": "s" }, { "keep": "v" } ],
        });
        let red = redact_json(&args);
        assert_eq!(red["username"], "alice");
        assert_eq!(red["password"], "[redacted]");
        assert_eq!(red["API_KEY"], "[redacted]"); // case-insensitive
        assert_eq!(red["token_count"], 42); // exact-match: kept
        assert_eq!(red["nested"]["client_secret"], "[redacted]");
        assert_eq!(red["nested"]["ok"], true);
        assert_eq!(red["list"][0]["secret"], "[redacted]");
        assert_eq!(red["list"][1]["keep"], "v");
    }
}
