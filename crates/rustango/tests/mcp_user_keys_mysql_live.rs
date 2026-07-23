//! MySQL mirror of `mcp_user_keys_sqlite_live` — validates the user-owned key
//! + permission-driven capability resolver against **real MySQL** (the
//! `user_id` column, the `rustango_agent_skill_permissions` table, and the
//! `resolve_user_agent_grants_pool` join all rendered by the tri-dialect
//! engine). Activated by `MYSQL_TEST_URL`; skips silently when unset.
//!
//!   export MYSQL_TEST_URL=mysql://rustango:rustango@127.0.0.1:3406/rustango_test
//!   cargo test -p rustango --features mysql,tenancy,mcp,testkit --test mcp_user_keys_mysql_live

#![cfg(all(feature = "mysql", feature = "mcp", feature = "testkit"))]

use rustango::sql::{sqlx, Pool};
use rustango::tenancy::permissions::set_user_perm_pool;
use rustango::tenancy::{
    create_skill_pool, create_user_key_pool, list_user_keys_pool, map_skill_to_permission_pool,
    resolve_user_agent_grants_pool, revoke_user_key_pool,
};
use tokio::sync::Mutex;

/// Suite-wide lock — every test shares the `MYSQL_TEST_URL` DB and resets the
/// agent tables, so they must not run concurrently.
fn live_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<Pool> {
    let url = std::env::var("MYSQL_TEST_URL").ok()?;
    let pool: Pool = sqlx::MySqlPool::connect(&url).await.ok()?.into();
    let my = pool.as_mysql().unwrap();
    // Clean slate for the agent tables (FK checks off so child-order doesn't
    // matter). `migrate_framework` recreates them from the models each run.
    sqlx::query("SET FOREIGN_KEY_CHECKS = 0")
        .execute(my)
        .await
        .expect("fk off");
    for tbl in [
        "rustango_agent_skill_permissions",
        "rustango_agent_skill_tools",
        "rustango_agent_skill_resources",
        "rustango_agent_grants",
        "rustango_agent_skills",
        "rustango_agents",
    ] {
        let _ = sqlx::query(&format!("DROP TABLE IF EXISTS `{tbl}`"))
            .execute(my)
            .await;
    }
    sqlx::query("SET FOREIGN_KEY_CHECKS = 1")
        .execute(my)
        .await
        .expect("fk on");
    rustango::testkit::migrate_framework(&pool)
        .await
        .expect("framework schema");
    Some(pool)
}

async fn make_user(pool: &Pool, name: &str) -> i64 {
    let my = pool.as_mysql().unwrap();
    sqlx::query("DELETE FROM `rustango_users` WHERE username = ?")
        .bind(name)
        .execute(my)
        .await
        .ok();
    sqlx::query(
        "INSERT INTO `rustango_users` (username, password_hash, is_superuser, active, created_at) \
         VALUES (?, '', 0, 1, NOW())",
    )
    .bind(name)
    .execute(my)
    .await
    .expect("insert user");
    let (id,): (i64,) = sqlx::query_as("SELECT id FROM `rustango_users` WHERE username = ?")
        .bind(name)
        .fetch_one(my)
        .await
        .expect("fetch id");
    id
}

#[tokio::test]
async fn user_key_capabilities_follow_permissions_on_mysql() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return; // MYSQL_TEST_URL unset — skip offline.
    };

    let uid = make_user(&pool, "ukmy_alice").await;
    set_user_perm_pool(uid, "ukmy.coach", true, &pool)
        .await
        .expect("perm");
    create_skill_pool(&pool, "ukmy_coach", "Coach", "", "", &["coach_log".into()])
        .await
        .expect("skill");
    map_skill_to_permission_pool(&pool, "ukmy_coach", "ukmy.coach")
        .await
        .expect("map");

    let issued = create_user_key_pool(&pool, uid, "laptop")
        .await
        .expect("key");
    assert_eq!(issued.agent.user_id, Some(uid));
    let agent_id = issued.agent.id.get().copied().unwrap();
    let (skills, tools) = resolve_user_agent_grants_pool(&pool, agent_id, uid)
        .await
        .expect("resolve");
    assert_eq!(skills, vec!["ukmy_coach"]);
    assert_eq!(tools, vec!["coach_log"]);

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
    let bob = make_user(&pool, "ukmy_bob").await;
    let issued2 = create_user_key_pool(&pool, bob, "phone")
        .await
        .expect("key2");
    let aid2 = issued2.agent.id.get().copied().unwrap();
    let (s2, t2) = resolve_user_agent_grants_pool(&pool, aid2, bob)
        .await
        .expect("resolve2");
    assert!(s2.is_empty() && t2.is_empty());
}
