//! User-owned MCP keys + permission-driven capabilities (feat/mcp-user-keys).
//!
//! A member generates a personal key (a user-owned `Agent`); its MCP
//! capabilities are resolved from the owner's RBAC permissions via
//! skill↔permission mappings — not pinned onto the key. Exercised end-to-end
//! on SQLite through the public API:
//!
//!   create user → grant permission → create skill(+tool) → map skill↔perm
//!     → create_user_key → authenticate → resolve_user_agent_grants
//!     → issue token (carries `uid`) → verify → tools/list → tools/call
//!
//! Plus the negative path (no permission → no tools) and list/revoke.

#![cfg(all(feature = "sqlite", feature = "mcp", feature = "testkit"))]
#![allow(irrefutable_let_patterns)]

use std::sync::Arc;

use rustango::mcp::{
    call_tool, issue_agent_token, list_tools, verify_agent_token, McpContext, McpError,
};
use rustango::sql::{sqlx, Pool};
use rustango::tenancy::jwt_lifecycle::JwtLifecycle;
use rustango::tenancy::permissions::set_user_perm_pool;
use rustango::tenancy::{
    authenticate_agent_pool, create_skill_pool, create_user_key_pool, list_user_keys_pool,
    map_skill_to_permission_pool, resolve_user_agent_grants_pool, revoke_user_key_pool,
};
use serde_json::json;

rustango::register_mcp_tool!(
    "coach_log",
    "Record a coaching note",
    NoteInput,
    |_ctx: McpContext, input: NoteInput| async move {
        Ok::<_, McpError>(json!({ "logged": input.note }))
    },
);

#[derive(serde::Deserialize)]
struct NoteInput {
    note: String,
}

impl rustango::openapi::OpenApiSchema for NoteInput {
    fn openapi_schema() -> rustango::openapi::Schema {
        rustango::openapi::Schema::object()
            .property("note", rustango::openapi::Schema::string())
            .required(["note"])
    }
}

async fn world() -> Pool {
    let pool = Pool::Sqlite(
        sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("sqlite"),
    );
    // rustango_users + roles/permissions + join tables, via the real path.
    rustango::testkit::migrate_framework(&pool)
        .await
        .expect("migrate framework");
    pool
}

async fn make_user(pool: &Pool, name: &str) -> i64 {
    let Pool::Sqlite(sq) = pool else {
        unreachable!()
    };
    sqlx::query(
        "INSERT INTO rustango_users (username, password_hash, is_superuser, active, created_at) \
         VALUES (?, '', 0, 1, datetime('now'))",
    )
    .bind(name)
    .execute(sq)
    .await
    .expect("insert user");
    let (id,): (i64,) = sqlx::query_as("SELECT id FROM rustango_users WHERE username = ?")
        .bind(name)
        .fetch_one(sq)
        .await
        .expect("fetch id");
    id
}

fn ctx(pool: &Pool, agent: &rustango::mcp::McpAgent) -> McpContext {
    McpContext {
        pool: pool.clone(),
        agent: agent.clone(),
        progress: rustango::mcp::ProgressReporter::disabled(),
        cancel: rustango::mcp::CancelToken::never(),
    }
}

/// Permission-holding owner: the key can list + call the mapped tool, and its
/// token carries the owning `user_id`.
#[tokio::test]
async fn permission_grants_flow_into_a_user_key() {
    let pool = world().await;
    let uid = make_user(&pool, "alice").await;

    // Owner holds the coaching permission (direct grant).
    set_user_perm_pool(uid, "mcp.coach", true, &pool)
        .await
        .expect("grant perm");

    // A skill bundling the tool, mapped to that permission.
    create_skill_pool(
        &pool,
        "coach",
        "Coach",
        "logs coaching notes",
        "You are the member's coach.",
        &["coach_log".into()],
    )
    .await
    .expect("skill");
    map_skill_to_permission_pool(&pool, "coach", "mcp.coach")
        .await
        .expect("map");

    // Member generates a personal key (shown-once secret).
    let issued = create_user_key_pool(&pool, uid, "Alice's phone")
        .await
        .expect("key");
    assert_eq!(issued.agent.user_id, Some(uid));
    assert!(issued.token.contains('.'), "prefix.secret token");

    // Credential → identity (what `/token` does).
    let agent_row = authenticate_agent_pool(&pool, &issued.agent.name, &issued.token)
        .await
        .expect("auth")
        .expect("valid credential");
    let agent_id = agent_row.id.get().copied().unwrap();
    assert_eq!(agent_row.user_id, Some(uid));

    // Capabilities resolve from the owner's permissions.
    let (skills, tools) = resolve_user_agent_grants_pool(&pool, agent_id, uid)
        .await
        .expect("resolve");
    assert_eq!(skills, vec!["coach"]);
    assert_eq!(tools, vec!["coach_log"]);

    // Token carries the owner + the resolved tools.
    let jwt = Arc::new(JwtLifecycle::new(
        b"user-keys-secret-at-least-32-bytes!!".to_vec(),
    ));
    let token =
        issue_agent_token(&jwt, agent_id, "acme", &skills, &tools, Some(uid)).expect("issue");
    let agent = verify_agent_token(&jwt, &token, "acme").expect("verify");
    assert_eq!(agent.user_id, Some(uid));
    assert_eq!(agent.tools, vec!["coach_log"]);

    // tools/list is gated to the granted tool; tools/call runs it.
    let names: Vec<String> = list_tools(&agent)["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, vec!["coach_log"]);
    let out = call_tool(
        ctx(&pool, &agent),
        json!({ "name": "coach_log", "arguments": { "note": "great squat depth" } }),
    )
    .await
    .expect("call");
    assert_eq!(out["structuredContent"]["logged"], "great squat depth");
}

/// Without the permission, the same mapped skill yields no capabilities —
/// fail-closed.
#[tokio::test]
async fn without_the_permission_a_key_gets_nothing() {
    let pool = world().await;
    let uid = make_user(&pool, "bob").await;

    create_skill_pool(&pool, "coach", "Coach", "", "", &["coach_log".into()])
        .await
        .expect("skill");
    map_skill_to_permission_pool(&pool, "coach", "mcp.coach")
        .await
        .expect("map");

    let issued = create_user_key_pool(&pool, uid, "Bob's key")
        .await
        .expect("key");
    let agent_id = issued.agent.id.get().copied().unwrap();

    // Bob was never granted `mcp.coach`.
    let (skills, tools) = resolve_user_agent_grants_pool(&pool, agent_id, uid)
        .await
        .expect("resolve");
    assert!(skills.is_empty(), "no skills without the permission");
    assert!(tools.is_empty(), "no tools without the permission");
}

/// Keys are listable and revocable by their owner; revoking a key that isn't
/// yours is refused.
#[tokio::test]
async fn keys_list_and_revoke_by_owner() {
    let pool = world().await;
    let alice = make_user(&pool, "alice").await;
    let mallory = make_user(&pool, "mallory").await;

    let k1 = create_user_key_pool(&pool, alice, "laptop")
        .await
        .expect("k1");
    let _k2 = create_user_key_pool(&pool, alice, "phone")
        .await
        .expect("k2");

    let keys = list_user_keys_pool(&pool, alice).await.expect("list");
    assert_eq!(keys.len(), 2);

    // Another user cannot revoke Alice's key.
    let k1_id = k1.agent.id.get().copied().unwrap();
    assert!(
        revoke_user_key_pool(&pool, mallory, k1_id).await.is_err(),
        "cross-user revoke refused"
    );

    // The owner can.
    revoke_user_key_pool(&pool, alice, k1_id)
        .await
        .expect("revoke");
    let after = list_user_keys_pool(&pool, alice).await.expect("list");
    assert_eq!(after.len(), 1);
}
