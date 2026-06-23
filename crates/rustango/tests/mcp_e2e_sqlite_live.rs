//! MCP end-to-end (epic #1013, Slice 6 / #1019) — the full agent loop on
//! SQLite, exercised through the public API:
//!
//!   create-agent → create-skill(+tool,+resource) → grant-skill
//!     → resolve grants → issue token → verify token
//!     → tools/list (only granted) → tools/call → prompts/get → resources/read
//!
//! This is the "full agent loop is green" acceptance. (The JSON-RPC
//! transport itself — initialize/ping over HTTP — is covered in
//! `mcp_slice1`.)
#![cfg(all(feature = "sqlite", feature = "mcp"))]
#![allow(irrefutable_let_patterns)]

use std::sync::Arc;

use rustango::core::Column as _;
use rustango::mcp::{
    call_tool, get_prompt, issue_agent_token, list_resources, list_tools, read_resource,
    verify_agent_token, McpContext, McpError,
};
use rustango::sql::{sqlx, FetcherPool as _, Pool};
use rustango::tenancy::jwt_lifecycle::JwtLifecycle;
use rustango::tenancy::{
    add_skill_resource_pool, authenticate_agent_pool, create_agent_pool, create_skill_pool,
    grant_skill_pool, resolve_agent_grants_pool, Agent,
};
use serde_json::json;

rustango::register_mcp_tool!(
    "echo",
    "Echo a message back",
    EchoInput,
    |_ctx: McpContext, input: EchoInput| async move {
        Ok::<_, McpError>(json!({ "echo": input.message }))
    },
);

#[derive(serde::Deserialize)]
struct EchoInput {
    message: String,
}

impl rustango::openapi::OpenApiSchema for EchoInput {
    fn openapi_schema() -> rustango::openapi::Schema {
        rustango::openapi::Schema::object()
            .property("message", rustango::openapi::Schema::string())
            .required(["message"])
    }
}

fn ctx(pool: &Pool, agent: &rustango::mcp::McpAgent) -> McpContext {
    McpContext {
        pool: pool.clone(),
        agent: agent.clone(),
    }
}

#[tokio::test]
async fn full_agent_loop() {
    let pool = Pool::Sqlite(
        sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("sqlite"),
    );

    // 1. Provision an agent (one-time secret).
    let issued = create_agent_pool(&pool, "e2e-bot").await.expect("agent");

    // 2. Define a skill bundling the `echo` tool + a resource + a prompt.
    create_skill_pool(
        &pool,
        "greeter",
        "Greeter",
        "greets people",
        "You greet the user warmly.",
        &["echo".into()],
    )
    .await
    .expect("skill");
    add_skill_resource_pool(
        &pool,
        "greeter",
        "greet://welcome",
        "text/plain",
        "Welcome!",
    )
    .await
    .expect("resource");

    // 3. Grant the skill to the agent.
    grant_skill_pool(&pool, "e2e-bot", "greeter")
        .await
        .expect("grant");

    // 4. Credential → identity (what the `/token` endpoint does).
    let agent_row = authenticate_agent_pool(&pool, "e2e-bot", &issued.token)
        .await
        .expect("auth call")
        .expect("valid credential");
    let agent_id = agent_row.id.get().copied().unwrap();
    assert_eq!(
        agent_id,
        Agent::objects()
            .where_(Agent::name.eq("e2e-bot"))
            .fetch(&pool)
            .await
            .unwrap()[0]
            .id
            .get()
            .copied()
            .unwrap()
    );

    // 5. Resolve grants → issue a tenant-pinned scoped JWT.
    let (skills, tools) = resolve_agent_grants_pool(&pool, agent_id).await.unwrap();
    assert_eq!(skills, vec!["greeter"]);
    assert_eq!(tools, vec!["echo"]);
    let jwt = Arc::new(JwtLifecycle::new(
        b"e2e-secret-at-least-32-bytes-long!!".to_vec(),
    ));
    let token = issue_agent_token(&jwt, agent_id, "acme", &skills, &tools).expect("issue");

    // 6. Verify the token (what the guarded endpoint does each request).
    let agent = verify_agent_token(&jwt, &token, "acme").expect("verify");
    assert_eq!(agent.tools, vec!["echo"]);

    // 7. tools/list shows only the granted tool.
    let listed = list_tools(&agent);
    let names: Vec<&str> = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["echo"]);

    // 8. tools/call runs it.
    let out = call_tool(
        ctx(&pool, &agent),
        json!({ "name": "echo", "arguments": { "message": "hi" } }),
    )
    .await
    .expect("call");
    assert_eq!(out["structuredContent"]["echo"], "hi");

    // 9. prompts/get returns the skill's instructions.
    let prompt = get_prompt(&ctx(&pool, &agent), json!({ "name": "greeter" }))
        .await
        .expect("prompt");
    assert_eq!(
        prompt["messages"][0]["content"]["text"],
        "You greet the user warmly."
    );

    // 10. resources/list + resources/read return the skill resource.
    let res_list = list_resources(&ctx(&pool, &agent)).await.expect("res list");
    assert!(res_list["resources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["uri"] == "greet://welcome"));
    let read = read_resource(&ctx(&pool, &agent), json!({ "uri": "greet://welcome" }))
        .await
        .expect("read");
    assert_eq!(read["contents"][0]["text"], "Welcome!");
}
