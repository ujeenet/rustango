//! Postgres mirror of `mcp_user_keys_sqlite_live` — validates the user-owned
//! key + permission-driven capability resolver against **real Postgres** (the
//! `user_id` column, the `rustango_agent_skill_permissions` table, and the
//! `resolve_user_agent_grants_pool` join all rendered by the tri-dialect
//! engine). Reads `DATABASE_URL`; every test returns silently when unset, so
//! `cargo test` stays green offline. CI's `--all-features` Postgres job runs it.

#![cfg(all(feature = "postgres", feature = "mcp", feature = "testkit"))]

use rustango::sql::{sqlx, Pool};
use rustango::tenancy::permissions::set_user_perm_pool;
use rustango::tenancy::{
    create_skill_pool, create_user_key_pool, list_user_keys_pool, map_skill_to_permission_pool,
    resolve_user_agent_grants_pool, revoke_user_key_pool,
};
use tokio::sync::Mutex;

/// Suite-wide lock — every test shares the `DATABASE_URL` DB and resets the
/// agent tables, so they must not run concurrently.
fn live_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<Pool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool: Pool = sqlx::PgPool::connect(&url).await.ok()?.into();
    let pg = pool.as_postgres().unwrap();
    // Clean slate for the agent tables (child-first). `migrate_framework`
    // recreates them from the models, so we get the current DDL each run.
    for tbl in [
        "rustango_agent_skill_permissions",
        "rustango_agent_skill_tools",
        "rustango_agent_skill_resources",
        "rustango_agent_grants",
        "rustango_agent_skills",
        "rustango_agents",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{tbl}" CASCADE"#))
            .execute(pg)
            .await
            .expect("drop");
    }
    rustango::testkit::migrate_framework(&pool)
        .await
        .expect("framework schema");
    Some(pool)
}

async fn make_user(pool: &Pool, name: &str) -> i64 {
    let pg = pool.as_postgres().unwrap();
    sqlx::query(r#"DELETE FROM "rustango_users" WHERE username = $1"#)
        .bind(name)
        .execute(pg)
        .await
        .ok();
    let (id,): (i64,) = sqlx::query_as(
        r#"INSERT INTO "rustango_users" (username, password_hash, is_superuser, active, created_at)
           VALUES ($1, '', false, true, NOW()) RETURNING id"#,
    )
    .bind(name)
    .fetch_one(pg)
    .await
    .expect("insert user");
    id
}

#[tokio::test]
async fn user_key_capabilities_follow_permissions_on_pg() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return; // DATABASE_URL unset — skip offline.
    };

    // Owner holds the coaching permission; a mapped skill bundles the tool.
    let uid = make_user(&pool, "ukpg_alice").await;
    set_user_perm_pool(uid, "ukpg.coach", true, &pool)
        .await
        .expect("perm");
    create_skill_pool(&pool, "ukpg_coach", "Coach", "", "", &["coach_log".into()])
        .await
        .expect("skill");
    map_skill_to_permission_pool(&pool, "ukpg_coach", "ukpg.coach")
        .await
        .expect("map");

    // A personal key resolves its capabilities from the owner's permissions.
    let issued = create_user_key_pool(&pool, uid, "laptop")
        .await
        .expect("key");
    assert_eq!(issued.agent.user_id, Some(uid));
    let agent_id = issued.agent.id.get().copied().unwrap();
    let (skills, tools) = resolve_user_agent_grants_pool(&pool, agent_id, uid)
        .await
        .expect("resolve");
    assert_eq!(skills, vec!["ukpg_coach"]);
    assert_eq!(tools, vec!["coach_log"]);

    // Owner-scoped list + revoke.
    assert_eq!(
        list_user_keys_pool(&pool, uid).await.expect("list").len(),
        1
    );
    revoke_user_key_pool(&pool, uid, agent_id)
        .await
        .expect("revoke");
    assert!(list_user_keys_pool(&pool, uid)
        .await
        .expect("list")
        .is_empty());

    // Negative: no permission → no capabilities (fail-closed).
    let bob = make_user(&pool, "ukpg_bob").await;
    let issued2 = create_user_key_pool(&pool, bob, "phone")
        .await
        .expect("key2");
    let aid2 = issued2.agent.id.get().copied().unwrap();
    let (s2, t2) = resolve_user_agent_grants_pool(&pool, aid2, bob)
        .await
        .expect("resolve2");
    assert!(s2.is_empty() && t2.is_empty());
}
