//! Backing test for `docs/mcp.md` — the headline MCP flow:
//!   register a tool → `initialize` over HTTP → provision an agent + skill +
//!   grant → issue a token → `tools/list` (only granted) → `tools/call` →
//!   fail-closed for an ungranted agent.
//!
//! Run: `cargo test -p rustango --features sqlite,mcp --test mcp_doc`

#![cfg(all(feature = "sqlite", feature = "mcp", feature = "testkit"))]
#![allow(irrefutable_let_patterns)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::mcp::{
    call_tool, issue_agent_token, list_tools, verify_agent_token, CancelToken, McpAgent,
    McpContext, McpError, ProgressReporter,
};
use rustango::sql::{sqlx, Pool};
use rustango::tenancy::jwt_lifecycle::JwtLifecycle;
use rustango::tenancy::{
    create_agent_pool, create_skill_pool, grant_skill_pool, resolve_agent_grants_pool,
};
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

// ---- Step 2: define a tool -------------------------------------------------

rustango::register_mcp_tool!(
    "add",
    "Add two integers",
    AddInput,
    |_ctx: McpContext, input: AddInput| async move {
        Ok::<_, McpError>(json!({ "sum": input.a + input.b }))
    },
);

#[derive(serde::Deserialize)]
struct AddInput {
    a: i64,
    b: i64,
}

impl rustango::openapi::OpenApiSchema for AddInput {
    fn openapi_schema() -> rustango::openapi::Schema {
        rustango::openapi::Schema::object()
            .property("a", rustango::openapi::Schema::integer())
            .property("b", rustango::openapi::Schema::integer())
            .required(["a", "b"])
    }
}

fn ctx(pool: &Pool, agent: &McpAgent) -> McpContext {
    McpContext {
        pool: pool.clone(),
        agent: agent.clone(),
        progress: ProgressReporter::disabled(),
        cancel: CancelToken::never(),
    }
}

// ---- Step 3: the JSON-RPC handshake over HTTP ------------------------------

#[tokio::test]
async fn initialize_handshake_over_http() {
    let app = rustango::mcp::tenant_router();
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "doc-client", "version": "0" }
                }
            }))
            .unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(
        body["result"]["protocolVersion"],
        rustango::mcp::PROTOCOL_VERSION
    );
    assert_eq!(body["result"]["serverInfo"]["name"], "rustango");
    assert_eq!(body["result"]["capabilities"]["tools"]["listChanged"], true);
}

// ---- Step 4: provision → grant → token → list → call -----------------------

#[tokio::test]
async fn agent_lists_and_calls_only_its_granted_tools() {
    let pool = Pool::Sqlite(sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap());
    rustango::testkit::migrate_framework(&pool)
        .await
        .expect("migrate framework");

    // Provision an agent (returns a one-time credential), define a skill that
    // bundles the `add` tool, and grant it to the agent in tenant "acme".
    let issued = create_agent_pool(&pool, "calc-bot").await.unwrap();
    assert!(!issued.token.is_empty(), "one-time agent credential");
    create_skill_pool(
        &pool,
        "calculator",
        "Calculator",
        "does arithmetic",
        "You are a precise calculator.",
        &["add".into()],
    )
    .await
    .unwrap();
    grant_skill_pool(&pool, "acme", "calc-bot", "calculator")
        .await
        .unwrap();

    // Resolve the grant → mint a tenant-pinned, scoped agent JWT (what the
    // /token and /oauth/token endpoints do under the hood).
    let agent_id = {
        use rustango::core::Column as _;
        use rustango::sql::FetcherPool as _;
        rustango::tenancy::Agent::objects()
            .where_(rustango::tenancy::Agent::name.eq("calc-bot"))
            .fetch(&pool)
            .await
            .unwrap()[0]
            .id
            .get()
            .copied()
            .unwrap()
    };
    let (skills, tools) = resolve_agent_grants_pool(&pool, agent_id).await.unwrap();
    assert_eq!(tools, vec!["add"]);

    let jwt = Arc::new(JwtLifecycle::new(
        b"doc-secret-at-least-32-bytes-long!!".to_vec(),
    ));
    let token = issue_agent_token(&jwt, agent_id, "acme", &skills, &tools, None).unwrap();

    // The guarded endpoint verifies the token on every request (tenant-pinned).
    let agent = verify_agent_token(&jwt, &token, "acme")
        .await
        .expect("valid agent token");

    // tools/list shows ONLY the granted tool, with its JSON Schema.
    let listed = list_tools(&agent);
    let names: Vec<&str> = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["add"]);

    // tools/call runs the handler and returns a structured result.
    let out = call_tool(
        ctx(&pool, &agent),
        json!({ "name": "add", "arguments": { "a": 2, "b": 3 } }),
    )
    .await
    .expect("call succeeds");
    assert_eq!(out["structuredContent"]["sum"], 5);
    assert_eq!(out["isError"], false);
}

#[tokio::test]
async fn ungranted_agent_sees_nothing_and_is_refused() {
    let pool = Pool::Sqlite(sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap());
    rustango::testkit::migrate_framework(&pool)
        .await
        .expect("migrate framework");

    // An agent with NO grants — fail-closed: empty tool list, calls refused.
    let agent = McpAgent {
        agent_id: 1,
        tenant: "acme".into(),
        skills: vec![],
        tools: vec![],
        user_id: None,
        jti: "doc-jti".into(),
    };

    let listed = list_tools(&agent);
    assert!(
        listed["tools"].as_array().unwrap().is_empty(),
        "no tools granted"
    );

    let err = call_tool(
        ctx(&pool, &agent),
        json!({ "name": "add", "arguments": { "a": 1, "b": 2 } }),
    )
    .await
    .expect_err("ungranted tool call is refused");
    assert_eq!(err.code, rustango::mcp::codes::TOOL_FORBIDDEN);
}
