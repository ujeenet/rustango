//! MCP Slice 3 (#1016) — tool registry + `tools/list` + `tools/call`.
//!
//! Exercises the public `register_mcp_tool!` → `list_tools` / `call_tool`
//! path directly (an `McpContext` is built from a sqlite pool + a synthetic
//! `McpAgent`), so no HTTP `TenantContext` is needed — the full transport
//! e2e is Slice 6 (#1019). Acceptance:
//!   * a registered tool is callable end-to-end                → `add`
//!   * an unregistered tool name errors and never executes      → TOOL_NOT_FOUND
//!   * a tool outside the agent's set is refused (fail-closed)  → TOOL_FORBIDDEN
//!   * invalid arguments return a structured error              → INVALID_PARAMS
#![cfg(all(feature = "sqlite", feature = "mcp"))]
#![allow(irrefutable_let_patterns)]

use rustango::mcp::{call_tool, list_tools, McpAgent, McpContext, McpError};
use rustango::openapi::{OpenApiSchema, Schema};
use rustango::sql::{sqlx, Pool};
use serde_json::json;

// ----- a tool with a typed, schema-bearing input ------------------------

#[derive(serde::Deserialize)]
struct AddInput {
    a: i64,
    b: i64,
}

impl OpenApiSchema for AddInput {
    fn openapi_schema() -> Schema {
        Schema::object()
            .property("a", Schema::integer())
            .property("b", Schema::integer())
            .required(["a", "b"])
    }
}

rustango::register_mcp_tool!(
    "add",
    "Add two integers",
    AddInput,
    |_ctx: McpContext, input: AddInput| async move {
        Ok::<_, McpError>(json!({ "sum": input.a + input.b }))
    },
);

// A tool that panics — must be caught and surfaced as an internal error
// (#1096), not unwound into the transport.
rustango::register_mcp_tool!(
    "boom",
    "Panics on purpose",
    AddInput,
    |_ctx: McpContext, _input: AddInput| async move {
        panic!("kaboom");
        #[allow(unreachable_code)]
        Ok::<_, McpError>(json!({}))
    },
);

// A tool that ran and failed with a domain error — MCP isError semantics
// (#1099) surface this as a successful result with isError:true, not a
// JSON-RPC error.
rustango::register_mcp_tool!(
    "flaky",
    "Always fails at runtime",
    AddInput,
    |_ctx: McpContext, _input: AddInput| async move {
        Err::<serde_json::Value, _>(McpError::internal("upstream API unavailable"))
    },
);

async fn ctx_with_tools(tools: &[&str]) -> McpContext {
    let pool = Pool::Sqlite(
        sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("sqlite"),
    );
    McpContext {
        pool,
        agent: McpAgent {
            agent_id: 1,
            tenant: "acme".into(),
            skills: vec![],
            tools: tools.iter().map(|s| s.to_string()).collect(),
            jti: "test-jti".into(),
        },
        progress: rustango::mcp::ProgressReporter::disabled(),
        cancel: rustango::mcp::CancelToken::never(),
    }
}

#[tokio::test]
async fn registered_tool_appears_in_list_with_schema() {
    let agent = ctx_with_tools(&["add"]).await.agent;
    let listed = list_tools(&agent);
    let tools = listed["tools"].as_array().expect("tools array");
    let add = tools
        .iter()
        .find(|t| t["name"] == "add")
        .expect("`add` is registered");
    assert_eq!(add["description"], "Add two integers");
    // inputSchema is the OpenApiSchema-derived JSON Schema.
    assert_eq!(add["inputSchema"]["type"], "object");
    assert_eq!(add["inputSchema"]["properties"]["a"]["type"], "integer");
}

#[tokio::test]
async fn call_tool_runs_handler_and_wraps_result() {
    let ctx = ctx_with_tools(&["add"]).await; // agent granted the tool
    let out = call_tool(
        ctx,
        json!({ "name": "add", "arguments": { "a": 2, "b": 3 } }),
    )
    .await
    .expect("call ok");
    // CallToolResult: text content + structuredContent + isError:false.
    assert_eq!(out["isError"], false);
    assert_eq!(out["structuredContent"]["sum"], 5);
    assert_eq!(out["content"][0]["type"], "text");
    assert!(out["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("\"sum\":5"));
}

#[tokio::test]
async fn unknown_tool_errors_and_never_runs() {
    let ctx = ctx_with_tools(&[]).await;
    let err = call_tool(ctx, json!({ "name": "nope", "arguments": {} }))
        .await
        .expect_err("unknown tool errors");
    assert_eq!(err.code, rustango::mcp::codes::TOOL_NOT_FOUND);
}

#[tokio::test]
async fn tool_outside_agent_set_is_forbidden() {
    // Agent is granted only "other" — calling "add" is fail-closed refused.
    let ctx = ctx_with_tools(&["other"]).await;
    let err = call_tool(
        ctx,
        json!({ "name": "add", "arguments": { "a": 1, "b": 1 } }),
    )
    .await
    .expect_err("forbidden");
    assert_eq!(err.code, rustango::mcp::codes::TOOL_FORBIDDEN);
}

#[tokio::test]
async fn handler_error_becomes_iserror_result_not_jsonrpc_error() {
    let ctx = ctx_with_tools(&["flaky"]).await;
    let out = call_tool(
        ctx,
        json!({ "name": "flaky", "arguments": { "a": 1, "b": 2 } }),
    )
    .await
    .expect("a tool execution failure is a successful isError result");
    assert_eq!(out["isError"], true);
    assert_eq!(out["content"][0]["type"], "text");
    assert!(out["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("upstream API unavailable"));
    // No structuredContent on an error result.
    assert!(out.get("structuredContent").is_none());
}

#[tokio::test]
async fn panicking_handler_becomes_internal_error_not_unwind() {
    let ctx = ctx_with_tools(&["boom"]).await;
    let err = call_tool(
        ctx,
        json!({ "name": "boom", "arguments": { "a": 1, "b": 2 } }),
    )
    .await
    .expect_err("panic is caught and converted to an error");
    assert_eq!(err.code, rustango::mcp::codes::INTERNAL_ERROR);
    assert!(err.message.contains("panicked"));
}

#[tokio::test]
async fn invalid_arguments_return_structured_error() {
    let ctx = ctx_with_tools(&["add"]).await;
    // `b` missing → deserialize fails → INVALID_PARAMS, handler never runs.
    let err = call_tool(ctx, json!({ "name": "add", "arguments": { "a": 1 } }))
        .await
        .expect_err("invalid args");
    assert_eq!(err.code, rustango::mcp::codes::INVALID_PARAMS);
    assert!(err.message.contains("invalid arguments"));
}
