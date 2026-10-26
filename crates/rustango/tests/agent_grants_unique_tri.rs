//! A skill grant and a skill-to-permission mapping are unique, so racing
//! grants leave one row, on every backend.

#![cfg(all(feature = "tenancy", feature = "mcp", feature = "testkit"))]

use rustango::sql::{CounterPool as _, FetcherPool as _, Pool};
use rustango::tenancy::{
    create_agent_pool, create_skill_pool, grant_skill_pool, map_skill_to_permission_pool, Agent,
    AgentGrant, AgentSkill, AgentSkillPermission, AgentSkillResource, AgentSkillTool,
};
use rustango::testkit::matrix::{drop_table, fresh_table};
use rustango::tri_dialect_test;

async fn setup(pool: &Pool) {
    use rustango::core::Model as _;
    for t in [
        AgentSkillPermission::SCHEMA.table,
        AgentSkillTool::SCHEMA.table,
        AgentSkillResource::SCHEMA.table,
        AgentGrant::SCHEMA.table,
    ] {
        drop_table(pool, t).await;
    }
    fresh_table::<AgentSkill>(pool).await;
    fresh_table::<Agent>(pool).await;
    fresh_table::<AgentSkillTool>(pool).await;
    fresh_table::<AgentGrant>(pool).await;
    fresh_table::<AgentSkillPermission>(pool).await;
}

async fn concurrent_grants_leave_one_row(pool: &Pool) {
    create_agent_pool(pool, "bot").await.unwrap();
    create_skill_pool(pool, "s1", "S1", "", "", &[])
        .await
        .unwrap();
    let runs = (0..8).map(|_| grant_skill_pool(pool, "t", "bot", "s1"));
    for r in futures_util::future::join_all(runs).await {
        r.unwrap();
    }
    assert_eq!(AgentGrant::objects().count(pool).await.unwrap(), 1);

    let maps = (0..8).map(|_| map_skill_to_permission_pool(pool, "s1", "post.change"));
    for r in futures_util::future::join_all(maps).await {
        r.unwrap();
    }
    assert_eq!(
        AgentSkillPermission::objects().count(pool).await.unwrap(),
        1
    );
}

/// The key is in the schema, not just checked before the insert.
async fn a_duplicate_grant_row_is_refused(pool: &Pool) {
    use rustango::sql::Auto;
    let agent = create_agent_pool(pool, "bot2").await.unwrap();
    create_skill_pool(pool, "s2", "S2", "", "", &[])
        .await
        .unwrap();
    let skill_id = AgentSkill::objects()
        .filter("codename", "s2")
        .fetch(pool)
        .await
        .unwrap()
        .remove(0)
        .id
        .get()
        .copied()
        .unwrap();
    let agent_id = agent.agent.id.get().copied().unwrap();
    let row = || AgentGrant {
        id: Auto::default(),
        agent_id,
        skill_id,
        data: serde_json::json!({}),
    };
    row().insert_pool(pool).await.unwrap();
    assert!(row().insert_pool(pool).await.is_err());
    let perm = || AgentSkillPermission {
        id: Auto::default(),
        skill_id,
        permission_codename: "post.view".into(),
    };
    perm().insert_pool(pool).await.unwrap();
    assert!(perm().insert_pool(pool).await.is_err());
}

tri_dialect_test!(
    setup: setup,
    scenarios: [concurrent_grants_leave_one_row, a_duplicate_grant_row_is_refused]
);
