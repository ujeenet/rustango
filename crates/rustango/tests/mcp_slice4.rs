//! MCP Slice 4 (#1017) — skills: catalog + grants + tool authorization.
//!
//! Acceptance (sqlite live, no HTTP — e2e is Slice 6 / #1019):
//!   * granting a skill flattens its tools into `resolve_agent_grants_pool`,
//!     which is what the token's `tools` claim is built from;
//!   * revoking removes them;
//!   * an agent with no grants resolves to an empty tool set (fail-closed) →
//!     `list_tools` returns nothing.
#![cfg(all(feature = "sqlite", feature = "mcp"))]
#![allow(irrefutable_let_patterns)]

use rustango::mcp::{list_tools, McpAgent};
use rustango::sql::{sqlx, Pool};
use rustango::tenancy::{
    create_agent_pool, create_skill_pool, grant_skill_pool, list_skills_pool,
    resolve_agent_grants_pool, revoke_skill_pool, Agent,
};

async fn pool() -> Pool {
    Pool::Sqlite(
        sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("sqlite"),
    )
}

async fn agent_id(pool: &Pool, name: &str) -> i64 {
    use rustango::core::Column as _;
    use rustango::sql::FetcherPool as _;
    Agent::objects()
        .where_(Agent::name.eq(name))
        .fetch(pool)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .id
        .get()
        .copied()
        .unwrap()
}

fn agent_with(tools: Vec<String>) -> McpAgent {
    McpAgent {
        agent_id: 1,
        tenant: "acme".into(),
        skills: vec![],
        tools,
        jti: "t".into(),
    }
}

#[tokio::test]
async fn grant_resolves_tools_revoke_removes_them() {
    let pool = pool().await;
    create_agent_pool(&pool, "bot").await.expect("agent");
    create_skill_pool(
        &pool,
        "reader",
        "Reader",
        "read things",
        "You can read.",
        &["posts.list".into(), "posts.get".into()],
    )
    .await
    .expect("skill");
    let id = agent_id(&pool, "bot").await;

    // Before any grant → empty (fail-closed).
    let (skills, tools) = resolve_agent_grants_pool(&pool, id).await.unwrap();
    assert!(skills.is_empty() && tools.is_empty());

    // Grant → skill codename + its tools resolve.
    grant_skill_pool(&pool, "bot", "reader")
        .await
        .expect("grant");
    let (skills, mut tools) = resolve_agent_grants_pool(&pool, id).await.unwrap();
    tools.sort();
    assert_eq!(skills, vec!["reader"]);
    assert_eq!(tools, vec!["posts.get", "posts.list"]);

    // Granting again is idempotent (UNIQUE(agent_id, skill_id)).
    grant_skill_pool(&pool, "bot", "reader")
        .await
        .expect("re-grant");
    let (_, tools) = resolve_agent_grants_pool(&pool, id).await.unwrap();
    assert_eq!(tools.len(), 2);

    // Revoke → back to empty.
    revoke_skill_pool(&pool, "bot", "reader")
        .await
        .expect("revoke");
    let (skills, tools) = resolve_agent_grants_pool(&pool, id).await.unwrap();
    assert!(skills.is_empty() && tools.is_empty());
}

#[tokio::test]
async fn list_tools_is_empty_for_ungranted_agent() {
    // The resolved tool set drives `tools/list`: no grants → empty list.
    let listed = list_tools(&agent_with(vec![]));
    assert_eq!(listed["tools"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn list_skills_returns_created() {
    let pool = pool().await;
    create_skill_pool(&pool, "a", "A", "", "", &[])
        .await
        .unwrap();
    create_skill_pool(&pool, "b", "B", "", "", &[])
        .await
        .unwrap();
    let skills = list_skills_pool(&pool).await.unwrap();
    let codes: Vec<&str> = skills.iter().map(|s| s.codename.as_str()).collect();
    assert_eq!(codes, vec!["a", "b"]);
}
