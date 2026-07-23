//! MCP Slice 5 (#1018) — prompts + resources from skills.
//!
//! Acceptance (sqlite live): a granted skill's prompt is returned by
//! `prompts/get` and its resources by `resources/read`; an ungranted
//! skill's prompt/resources are neither listed nor readable (fail-closed);
//! a `register_mcp_resource!` static resource is always available.
#![cfg(all(feature = "sqlite", feature = "mcp"))]
#![allow(irrefutable_let_patterns)]

use rustango::mcp::{
    get_prompt, list_prompts, list_resources, read_resource, McpAgent, McpContext,
};
use rustango::sql::{sqlx, Pool};
use rustango::tenancy::{add_skill_resource_pool, create_skill_pool};
use serde_json::json;

rustango::register_mcp_resource!("rustango://about", "About", "text/plain", || {
    "rustango MCP server".to_string()
},);

async fn pool() -> Pool {
    Pool::Sqlite(
        sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("sqlite"),
    )
}

fn ctx(pool: Pool, skills: &[&str]) -> McpContext {
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

#[tokio::test]
async fn granted_skill_prompt_is_listed_and_readable() {
    let pool = pool().await;
    create_skill_pool(
        &pool,
        "writer",
        "Writer",
        "writes posts",
        "You write blog posts.",
        &[],
    )
    .await
    .expect("skill");

    let c = ctx(pool, &["writer"]);
    let prompts = list_prompts(&c).await.expect("list");
    let arr = prompts["prompts"].as_array().unwrap();
    assert!(arr.iter().any(|p| p["name"] == "writer"));

    let got = get_prompt(&c, json!({ "name": "writer" }))
        .await
        .expect("get");
    assert_eq!(
        got["messages"][0]["content"]["text"],
        "You write blog posts."
    );
}

#[tokio::test]
async fn ungranted_prompt_is_refused() {
    let pool = pool().await;
    create_skill_pool(&pool, "secret", "Secret", "", "classified", &[])
        .await
        .unwrap();
    // Agent granted nothing → not listed, not gettable.
    let c = ctx(pool, &[]);
    assert!(list_prompts(&c).await.unwrap()["prompts"]
        .as_array()
        .unwrap()
        .is_empty());
    let err = get_prompt(&c, json!({ "name": "secret" }))
        .await
        .expect_err("forbidden");
    assert_eq!(err.code, rustango::mcp::codes::TOOL_FORBIDDEN);
}

#[tokio::test]
async fn granted_skill_resource_reads_and_static_resource_always_reads() {
    let pool = pool().await;
    create_skill_pool(&pool, "docs", "Docs", "", "", &[])
        .await
        .unwrap();
    add_skill_resource_pool(&pool, "docs", "file://readme", "text/markdown", "# Hello")
        .await
        .expect("resource");

    let c = ctx(pool, &["docs"]);
    let listed = list_resources(&c).await.expect("list");
    let uris: Vec<&str> = listed["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap())
        .collect();
    assert!(uris.contains(&"file://readme")); // skill resource
    assert!(uris.contains(&"rustango://about")); // static framework resource

    // Granted skill resource reads.
    let read = read_resource(&c, json!({ "uri": "file://readme" }))
        .await
        .expect("read");
    assert_eq!(read["contents"][0]["text"], "# Hello");

    // Static resource always reads.
    let about = read_resource(&c, json!({ "uri": "rustango://about" }))
        .await
        .expect("read static");
    assert_eq!(about["contents"][0]["text"], "rustango MCP server");
}

#[tokio::test]
async fn ungranted_resource_is_refused() {
    let pool = pool().await;
    create_skill_pool(&pool, "priv", "Priv", "", "", &[])
        .await
        .unwrap();
    add_skill_resource_pool(&pool, "priv", "file://secret", "text/plain", "nope")
        .await
        .unwrap();
    // Agent without the "priv" grant cannot read its resource.
    let c = ctx(pool, &[]);
    let err = read_resource(&c, json!({ "uri": "file://secret" }))
        .await
        .expect_err("forbidden");
    assert_eq!(err.code, rustango::mcp::codes::TOOL_FORBIDDEN);
}
