//! MCP Slice 2 (#1015) — Agent identity + scoped-JWT auth.
//!
//! Covers the four acceptance points without standing up a full HTTP
//! `TenantContext` (the end-to-end HTTP loop is Slice 6 / #1019):
//!   1. an agent exchanges its secret for a JWT          → `issue_agent_token`
//!   2. it authenticates within its tenant               → `verify_agent_token` ok
//!   3. a token whose tenant ≠ request tenant is rejected → tenant pin
//!   4. a revoked JTI is refused                          → `revoke` → verify None
//! plus the `rustango_agents` data layer (create / authenticate / rotate).
//!
//! Run: `cargo test -p rustango --no-default-features --features sqlite,mcp --test mcp_slice2`.
#![cfg(all(feature = "sqlite", feature = "mcp"))]
#![allow(irrefutable_let_patterns)] // Pool is single-variant in sqlite-only builds

use std::sync::Arc;

use rustango::sql::{sqlx, Pool};
use rustango::tenancy::jwt_lifecycle::JwtLifecycle;
use rustango::tenancy::{
    authenticate_agent_pool, create_agent_pool, list_agents_pool, rotate_agent_secret_pool,
    AgentError,
};

async fn sqlite_pool() -> Pool {
    let pool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory pool");
    Pool::Sqlite(pool)
}

// ----------------------------------------------------------- data layer

#[tokio::test]
async fn create_then_authenticate_roundtrip() {
    let pool = sqlite_pool().await;
    let issued = create_agent_pool(&pool, "ci-bot").await.expect("create");
    assert_eq!(issued.agent.name, "ci-bot");
    assert!(issued.token.contains('.'), "token is prefix.secret");
    assert!(issued.agent.active);

    // Full `prefix.secret` token authenticates.
    let ok = authenticate_agent_pool(&pool, "ci-bot", &issued.token)
        .await
        .expect("auth call");
    assert!(ok.is_some(), "correct secret authenticates");

    // The bare secret half also authenticates.
    let half = issued.token.rsplit('.').next().unwrap();
    assert!(authenticate_agent_pool(&pool, "ci-bot", half)
        .await
        .unwrap()
        .is_some());

    // Wrong secret + unknown name both fail-closed (Ok(None), not Err).
    assert!(authenticate_agent_pool(&pool, "ci-bot", "deadbeef.0000")
        .await
        .unwrap()
        .is_none());
    assert!(authenticate_agent_pool(&pool, "ghost", &issued.token)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn duplicate_name_is_rejected() {
    let pool = sqlite_pool().await;
    create_agent_pool(&pool, "dup").await.expect("first create");
    let res = create_agent_pool(&pool, "dup").await;
    assert!(matches!(res, Err(AgentError::Duplicate(name)) if name == "dup"));
}

#[tokio::test]
async fn rotate_invalidates_old_secret() {
    let pool = sqlite_pool().await;
    let first = create_agent_pool(&pool, "rotor").await.expect("create");
    let second = rotate_agent_secret_pool(&pool, "rotor")
        .await
        .expect("rotate");
    assert_ne!(first.token, second.token);

    // Old secret no longer authenticates; the new one does.
    assert!(authenticate_agent_pool(&pool, "rotor", &first.token)
        .await
        .unwrap()
        .is_none());
    assert!(authenticate_agent_pool(&pool, "rotor", &second.token)
        .await
        .unwrap()
        .is_some());
    assert!(second.agent.secret_rotated_at.is_some());
}

#[tokio::test]
async fn list_agents_returns_created() {
    let pool = sqlite_pool().await;
    create_agent_pool(&pool, "alpha").await.unwrap();
    create_agent_pool(&pool, "beta").await.unwrap();
    let agents = list_agents_pool(&pool).await.expect("list");
    let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "beta"]); // ordered by name
}

// --------------------------------------------------------- token logic

fn jwt() -> Arc<JwtLifecycle> {
    Arc::new(JwtLifecycle::new(
        b"test-secret-at-least-32-bytes-long!!".to_vec(),
    ))
}

#[tokio::test]
async fn token_issue_and_verify_within_tenant() {
    let jwt = jwt();
    let token = rustango::mcp::issue_agent_token(&jwt, 42, "acme", &[], &[], None).expect("issue");
    let agent = rustango::mcp::verify_agent_token(&jwt, &token, "acme").expect("verify");
    assert_eq!(agent.agent_id, 42);
    assert_eq!(agent.tenant, "acme");
    assert!(!agent.jti.is_empty());
}

#[tokio::test]
async fn token_for_other_tenant_is_rejected() {
    let jwt = jwt();
    let token = rustango::mcp::issue_agent_token(&jwt, 42, "acme", &[], &[], None).expect("issue");
    // Same valid signature, wrong tenant → refused (cross-tenant replay).
    assert!(rustango::mcp::verify_agent_token(&jwt, &token, "evilcorp").is_none());
}

#[tokio::test]
async fn revoked_token_is_refused() {
    let jwt = jwt();
    let token = rustango::mcp::issue_agent_token(&jwt, 7, "acme", &[], &[], None).expect("issue");
    assert!(rustango::mcp::verify_agent_token(&jwt, &token, "acme").is_some());
    assert!(jwt.revoke(&token), "revoke decodes + blacklists the jti");
    assert!(rustango::mcp::verify_agent_token(&jwt, &token, "acme").is_none());
}

#[tokio::test]
async fn non_agent_token_is_refused() {
    let jwt = jwt();
    // An access token with no `kind: agent` claim must not pass as an agent.
    let plain = jwt
        .issue_access_with(1, serde_json::Map::new())
        .expect("issue plain");
    assert!(rustango::mcp::verify_agent_token(&jwt, &plain, "acme").is_none());
}
