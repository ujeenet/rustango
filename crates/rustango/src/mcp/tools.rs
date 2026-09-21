//! The tool registry, plus `tools/list` and `tools/call`.
//!
//! [`register_mcp_tool!`] registers a tool at compile time by
//! submitting an [`McpTool`] into an `inventory` collection, the same
//! way `register_admin_view!` works. A handler is a bare `fn` wrapped
//! in a non-capturing inner one, because `inventory` cannot store a
//! closure that captures.
//!
//! Every tool names an input type. Its `OpenApiSchema` impl produces
//! the MCP `inputSchema`, and incoming arguments are checked by
//! deserializing into that type first. A malformed call is refused
//! and the handler never runs.
//!
//! [`register_mcp_tool!`]: crate::register_mcp_tool

use std::future::Future;
use std::pin::Pin;

use serde_json::{json, Value};

use crate::sql::Pool;

use super::auth::McpAgent;
use super::types::{codes, JsonRpcError};

#[doc(hidden)]
pub type JsonValue = Value;

/// What every tool handler gets for its call.
pub struct McpContext {
    /// The request's tenant pool.
    pub pool: Pool,
    /// The agent making the call, verified and pinned to one tenant.
    pub agent: McpAgent,
    /// Where to report progress. Active only when the caller sent a
    /// `progressToken`.
    pub progress: super::progress::ProgressReporter,
    /// The cancellation flag. Poll `is_cancelled()` at await points.
    pub cancel: super::progress::CancelToken,
}

/// What a tool handler can fail with. It becomes a JSON-RPC error.
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
    /// The arguments were wrong.
    #[must_use]
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(codes::INVALID_PARAMS, message)
    }
    /// Something went wrong inside the tool.
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

/// A database error inside a handler becomes an internal error.
impl From<crate::sql::ExecError> for McpError {
    fn from(e: crate::sql::ExecError) -> Self {
        Self::internal(e.to_string())
    }
}

/// The future a tool handler returns.
pub type McpToolFuture = Pin<Box<dyn Future<Output = Result<Value, McpError>> + Send + 'static>>;

/// A stored handler. It is a plain `fn` pointer, because `inventory`
/// needs something it can build in a const.
pub type McpToolHandler = fn(McpContext, Value) -> McpToolFuture;

/// One registered tool.
pub struct McpTool {
    /// The name `tools/call` looks up. It must be unique.
    pub name: &'static str,
    /// The description `tools/list` shows.
    pub description: &'static str,
    /// Builds the `inputSchema` from the tool's input type.
    pub input_schema: fn() -> Value,
    /// The handler, which checks its arguments first.
    pub handler: McpToolHandler,
}

inventory::collect!(McpTool);

// The macro calls these, so its expansion never has to name
// `serde_json` or `OpenApiSchema`. That way it works in any crate.
#[doc(hidden)]
pub fn __schema_of<T: crate::openapi::OpenApiSchema>() -> Value {
    serde_json::to_value(T::openapi_schema()).unwrap_or_else(|_| json!({ "type": "object" }))
}

#[doc(hidden)]
pub fn __deserialize_args<T: serde::de::DeserializeOwned>(raw: Value) -> Result<T, McpError> {
    serde_json::from_value(raw)
        .map_err(|e| McpError::invalid_params(format!("invalid arguments: {e}")))
}

/// Find a registered tool by name.
#[must_use]
pub(crate) fn find_tool(name: &str) -> Option<&'static McpTool> {
    inventory::iter::<McpTool>
        .into_iter()
        .find(|t| t.name == name)
}

/// The `tools/list` result. It names only the tools this agent was
/// granted, so an agent with no grants sees an empty list.
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

/// `tools/call`: find the tool, check the agent may use it, check
/// the arguments, run it, and wrap the result as a `CallToolResult`.
/// It also writes an audit entry, best effort.
///
/// # Errors
/// A JSON-RPC error when the tool is unknown or not granted, or the
/// arguments are wrong. In those cases the tool never runs.
pub async fn call_tool(ctx: McpContext, params: Value) -> Result<Value, JsonRpcError> {
    call_tool_with(ctx, params, None).await
}

/// [`call_tool`] plus the JSON-RPC `request_id`. The dispatcher
/// passes that id so the call can be cancelled, and so the
/// `progressToken` reaches the handler.
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

    // The granted set, resolved from the agent's skills when its
    // token was issued, decides. Anything outside it is refused and
    // never runs, and an agent with no grants can call nothing.
    if !ctx.agent.tools.iter().any(|n| n == &name) {
        return Err(JsonRpcError::new(
            codes::TOOL_FORBIDDEN,
            format!("tool `{name}` is not authorized for this agent"),
        ));
    }

    // Copy what the audit and the scoping need, before `ctx` moves
    // into the tool.
    let pool = ctx.pool.clone();
    let tenant = ctx.agent.tenant.clone();
    let agent_id = ctx.agent.agent_id;

    // Set up progress, from `_meta.progressToken`, and cancellation.
    // Both are scoped to the calling agent, so no other agent can
    // touch them, and the guard clears the registry entry on drop
    // even when the handler panics.
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

    // Catch a panic here. A buggy handler must not unwind into the
    // transport, which would drop the connection.
    let outcome = catch_unwind((tool.handler)(ctx, args.clone())).await;

    // Audit every call that ran, whether it worked or not, with the
    // arguments redacted.
    audit_tool_call(&pool, agent_id, &name, &args).await;

    // A tool that ran and failed returns a successful
    // `CallToolResult` with `isError: true`, so the client can show
    // or retry it. A rejection before the tool ran stays a JSON-RPC
    // error, and so does a panic.
    match outcome {
        Ok(Ok(value)) => Ok(call_tool_result(value)),
        Ok(Err(e)) if is_protocol_error(e.code) => Err(e.into_jsonrpc()),
        Ok(Err(e)) => Ok(call_tool_error_result(&e.message)),
        Err(_panic) => {
            tracing::error!(tool = %name, agent_id, "mcp tool handler panicked");
            Err(JsonRpcError::new(
                codes::INTERNAL_ERROR,
                "tool handler panicked",
            ))
        }
    }
}

/// Drive `fut` to completion, turning a panic in any single `poll`
/// into an `Err`. This is `catch_unwind` for a future, written here
/// so the crate need not depend on `futures` for it alone.
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

/// Wrap a handler's JSON in a `CallToolResult`. The value appears
/// twice: as a text block, which the spec requires, and as
/// `structuredContent` for a client that can read it directly.
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

/// A `CallToolResult` for a tool that ran but failed: the message as
/// a text block, plus `isError: true`. The JSON-RPC response itself
/// is a success, because the call worked and the tool reported the
/// failure.
fn call_tool_error_result(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true,
    })
}

/// The codes that mean the request itself was rejected, so the tool
/// produced no result. These stay JSON-RPC errors. Any other error
/// means the tool ran and failed, which becomes an `isError` result.
fn is_protocol_error(code: i64) -> bool {
    matches!(
        code,
        codes::PARSE_ERROR
            | codes::INVALID_REQUEST
            | codes::METHOD_NOT_FOUND
            | codes::INVALID_PARAMS
            | codes::TOOL_NOT_FOUND
            | codes::TOOL_FORBIDDEN
    )
}

/// Argument names whose values are scrubbed before a call is
/// audited, so no secret reaches the audit table. It is the
/// access-log list, `access_log::default_redact_params`, plus a few
/// names that matter here.
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

/// Return a copy of `value` with every sensitive key's value
/// replaced by `"[redacted]"`, at any depth. A key matches whole and
/// ignoring case, so `token_count` is left alone.
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

/// Audit a tool call, best effort: a failure here never fails the
/// call. Arguments are redacted, so a tool that takes a `password`
/// or a `token` cannot leak it into the audit table.
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
