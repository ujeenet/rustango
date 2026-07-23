//! MCP follow-up #1091 — logging/setLevel + completion/complete.
#![cfg(all(feature = "sqlite", feature = "mcp"))]
#![allow(irrefutable_let_patterns)]

use rustango::mcp::utilities::{complete, set_log_level};
use rustango::mcp::{McpAgent, McpContext};
use rustango::sql::{sqlx, Pool};
use rustango::tenancy::{add_skill_resource_pool, create_skill_pool};
use serde_json::json;

async fn ctx(skills: &[&str]) -> McpContext {
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
            skills: skills.iter().map(|s| s.to_string()).collect(),
            tools: vec![],
            user_id: None,
            jti: "t".into(),
        },
        progress: rustango::mcp::ProgressReporter::disabled(),
        cancel: rustango::mcp::CancelToken::never(),
    }
}

#[test]
fn set_log_level_validates() {
    assert_eq!(
        set_log_level(json!({ "level": "info" })).unwrap(),
        json!({})
    );
    let bad = set_log_level(json!({ "level": "loud" })).unwrap_err();
    assert_eq!(bad.code, rustango::mcp::codes::INVALID_PARAMS);
    let missing = set_log_level(json!({})).unwrap_err();
    assert_eq!(missing.code, rustango::mcp::codes::INVALID_PARAMS);
}

#[tokio::test]
async fn completion_suggests_granted_prompts_and_resources_by_prefix() {
    let c = ctx(&["greeter"]).await;
    create_skill_pool(&c.pool, "greeter", "G", "", "", &[])
        .await
        .unwrap();
    add_skill_resource_pool(&c.pool, "greeter", "greet://welcome", "text/plain", "hi")
        .await
        .unwrap();

    // Prefix "gr" matches both the "greeter" prompt and "greet://welcome".
    let out = complete(&c, json!({ "argument": { "name": "x", "value": "gr" } }))
        .await
        .expect("complete");
    let values: Vec<&str> = out["completion"]["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(values.contains(&"greeter"));
    assert!(values.contains(&"greet://welcome"));
    assert_eq!(out["completion"]["hasMore"], false);

    // A non-matching prefix yields nothing.
    let none = complete(&c, json!({ "argument": { "value": "zzz" } }))
        .await
        .unwrap();
    assert_eq!(none["completion"]["values"].as_array().unwrap().len(), 0);
}
