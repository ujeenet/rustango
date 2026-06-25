//! MCP follow-up #1090 — progress + cancellation for tools/call.
#![cfg(all(feature = "sqlite", feature = "mcp"))]
#![allow(irrefutable_let_patterns)]

use rustango::mcp::{call_tool, CancelToken, McpAgent, McpContext, McpError};
use rustango::sql::{sqlx, Pool};
use serde_json::json;

// A tool that reports progress and bails out if cancelled.
#[derive(serde::Deserialize)]
struct WorkInput {}

impl rustango::openapi::OpenApiSchema for WorkInput {
    fn openapi_schema() -> rustango::openapi::Schema {
        rustango::openapi::Schema::object()
    }
}

rustango::register_mcp_tool!(
    "work",
    "Long-running work that reports progress + honors cancellation",
    WorkInput,
    |ctx: McpContext, _input: WorkInput| async move {
        ctx.progress.report(0.5, Some(1.0), Some("halfway"));
        if ctx.cancel.is_cancelled() {
            return Err(McpError::new(-32004, "cancelled"));
        }
        ctx.progress.report(1.0, Some(1.0), None);
        Ok::<_, McpError>(json!({ "done": true }))
    },
);

async fn ctx() -> McpContext {
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
            tools: vec!["work".into()],
            jti: "t".into(),
        },
        progress: rustango::mcp::ProgressReporter::disabled(),
        cancel: rustango::mcp::CancelToken::never(),
    }
}

#[tokio::test]
async fn progress_notifications_are_emitted_for_a_progress_token() {
    let mut rx = rustango::mcp::notifications::bus().subscribe();
    let ctx = ctx().await;
    // `_meta.progressToken` activates the reporter.
    let out = call_tool(
        ctx,
        json!({ "name": "work", "arguments": {}, "_meta": { "progressToken": "pt-1" } }),
    )
    .await
    .expect("call");
    assert_eq!(out["structuredContent"]["done"], true);

    // Two progress frames should have been pushed for pt-1, scoped to this
    // agent (tenant "acme", agent_id 1 — see `ctx()`).
    let mut seen = 0;
    while let Ok(frame) = rx.try_recv() {
        assert_eq!(frame.tenant, "acme");
        assert_eq!(frame.agent_id, Some(1));
        let v: serde_json::Value = serde_json::from_str(&frame.body).unwrap();
        if v["method"] == "notifications/progress" && v["params"]["progressToken"] == "pt-1" {
            seen += 1;
        }
    }
    assert_eq!(seen, 2, "expected two progress notifications");
}

#[tokio::test]
async fn cancelled_call_observes_the_flag() {
    // A call whose cancel token is already tripped → the tool observes it
    // cooperatively and returns its cancelled error (the handler never
    // completes its work). The dispatcher registers + trips the token from
    // an inbound `notifications/cancelled`; here we inject a cancelled token
    // directly to exercise the handler-side contract.
    //
    // The handler returns an `Err` (code -32004), which is a *tool execution*
    // failure, so MCP `isError` semantics (#1099) make it a successful result
    // with `isError: true` rather than a JSON-RPC error.
    let mut ctx = ctx().await;
    ctx.cancel = CancelToken::cancelled();
    let out = call_tool(ctx, json!({ "name": "work", "arguments": {} }))
        .await
        .expect("cancelled call returns an isError result");
    assert_eq!(out["isError"], true);
    assert!(out["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("cancelled"));
}
