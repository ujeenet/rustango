//! Live integration tests for the tri-dialect `permissions::*_pool`
//! family on MySQL 8+. Mirror of `permissions_sqlite_live.rs`.
//!
//! Activated by `MYSQL_TEST_URL` (e.g.
//! `mysql://rustango:rustango@127.0.0.1:3406/rustango_test`). Without
//! the env var every test short-circuits with `eprintln!` and passes.
//!
//! Spin up the test container:
//!
//!   docker compose up -d mysql
//!   export MYSQL_TEST_URL=mysql://rustango:rustango@127.0.0.1:3406/rustango_test
//!   cargo test -p rustango --features mysql,tenancy --test permissions_mysql_live

#![cfg(all(feature = "mysql", feature = "tenancy"))]

use rustango::sql::{sqlx, Auto, Pool};
use rustango::tenancy::permissions::{
    assign_role_pool, clear_user_perm_pool, create_role_pool, ensure_tables_pool,
    get_or_create_role_pool, grant_role_perm_pool, has_all_perms_pool, has_any_perm_pool,
    has_perm_pool, remove_role_pool, revoke_role_perm_pool, set_user_perm_pool,
    user_permissions_pool, user_roles_pool,
};
use rustango::Model;
use tokio::sync::Mutex;

/// Suite-wide lock. Every test in this file resets the shared
/// permission tables + manually `CREATE`s `rustango_users`; under
/// cargo's default parallel harness two tests race and the second
/// `CREATE TABLE rustango_users` trips MySQL error 1050.
fn live_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "perm_mysql_blog_post")]
#[rustango(app = "perm_mysql_blog")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

async fn mysql_pool_or_skip() -> Option<Pool> {
    let url = std::env::var("MYSQL_TEST_URL").ok()?;
    let p = sqlx::MySqlPool::connect(&url)
        .await
        .expect("connect to MYSQL_TEST_URL");
    // Each test gets a clean slate — drop the per-test tables.
    //
    // FK checks are disabled around the drops so prior CI steps (e.g.
    // `tenancy_manage_mysql_live`, `mysql_live`) that left framework
    // tables with FKs *into* `rustango_users` don't make the drop fail
    // silently and trip `Table 'rustango_users' already exists` on the
    // subsequent CREATE. The per-table `let _ =` swallows individual
    // drop errors by design (table may not exist on first run), so the
    // FK_CHECKS toggle itself must `expect` — otherwise the bypass
    // silently no-ops and the failure surfaces only later as a 42S01.
    sqlx::query("SET FOREIGN_KEY_CHECKS = 0")
        .execute(&p)
        .await
        .expect("disable FK checks");
    for tbl in [
        "rustango_user_permissions",
        "rustango_user_roles",
        "rustango_role_permissions",
        "rustango_roles",
        "rustango_permissions",
        "rustango_users",
    ] {
        let _ = sqlx::query(&format!("DROP TABLE IF EXISTS `{tbl}`"))
            .execute(&p)
            .await;
    }
    sqlx::query("SET FOREIGN_KEY_CHECKS = 1")
        .execute(&p)
        .await
        .expect("re-enable FK checks");
    // Bootstrap rustango_users — the `_pool` family doesn't auto-create
    // it; the production tenant bootstrap migration does.
    sqlx::query(
        "CREATE TABLE `rustango_users` (\
            `id` BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY, \
            `username` VARCHAR(150) NOT NULL UNIQUE, \
            `password_hash` VARCHAR(255) NOT NULL DEFAULT '', \
            `is_superuser` BOOLEAN NOT NULL DEFAULT FALSE, \
            `active` BOOLEAN NOT NULL DEFAULT TRUE, \
            `data` JSON NOT NULL, \
            `created_at` DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6), \
            `password_changed_at` DATETIME(6))",
    )
    .execute(&p)
    .await
    .expect("create rustango_users");
    let pool = Pool::Mysql(p);
    ensure_tables_pool(&pool).await.expect("ensure_tables_pool");
    Some(pool)
}

async fn make_user(pool: &Pool, name: &str) -> i64 {
    let Pool::Mysql(my) = pool else {
        unreachable!()
    };
    sqlx::query("INSERT INTO `rustango_users` (`username`, `data`) VALUES (?, '{}')")
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
async fn role_grant_flows_to_has_perm_pool_on_mysql() {
    let _g = live_lock().lock().await;
    let Some(pool) = mysql_pool_or_skip().await else {
        eprintln!("MYSQL_TEST_URL unset — skipping");
        return;
    };
    let uid = make_user(&pool, "bob_editor").await;
    let role_id = create_role_pool("editor", "Edits posts", &pool)
        .await
        .expect("create_role_pool");
    grant_role_perm_pool(role_id, "perm_mysql_blog_post.change", &pool)
        .await
        .expect("grant_role_perm_pool");
    assign_role_pool(uid, role_id, &pool)
        .await
        .expect("assign_role_pool");
    let ok = has_perm_pool(uid, "perm_mysql_blog_post.change", &pool)
        .await
        .expect("has_perm_pool");
    assert!(ok, "user with editor role should have post.change on MySQL");

    revoke_role_perm_pool(role_id, "perm_mysql_blog_post.change", &pool)
        .await
        .expect("revoke_role_perm_pool");
    let ok = has_perm_pool(uid, "perm_mysql_blog_post.change", &pool)
        .await
        .expect("has_perm_pool after revoke");
    assert!(!ok, "after revoke the user should lose the perm");
}

#[tokio::test]
async fn has_any_perm_and_has_all_perms_work_on_mysql() {
    let _g = live_lock().lock().await;
    let Some(pool) = mysql_pool_or_skip().await else {
        eprintln!("MYSQL_TEST_URL unset — skipping");
        return;
    };
    let uid = make_user(&pool, "dave_admin").await;
    let role_id = create_role_pool("admin", "", &pool)
        .await
        .expect("create_role_pool");
    for action in ["add", "change", "delete", "view"] {
        let codename = format!("perm_mysql_blog_post.{action}");
        grant_role_perm_pool(role_id, &codename, &pool)
            .await
            .expect("grant_role_perm_pool");
    }
    assign_role_pool(uid, role_id, &pool)
        .await
        .expect("assign_role_pool");
    let any = has_any_perm_pool(
        uid,
        &["perm_mysql_blog_post.missing", "perm_mysql_blog_post.view"],
        &pool,
    )
    .await
    .expect("has_any_perm_pool");
    assert!(any);
    let all = has_all_perms_pool(
        uid,
        &[
            "perm_mysql_blog_post.add",
            "perm_mysql_blog_post.change",
            "perm_mysql_blog_post.view",
        ],
        &pool,
    )
    .await
    .expect("has_all_perms_pool");
    assert!(all, "admin role should grant all four CRUD codenames");
}

// Framework gap: `set_user_perm_pool` emits `ConflictClause::DoUpdate`
// with `target = ["user_id", "codename"]`, which the MySQL writer
// intentionally rejects (`sql/mysql.rs:write_conflict_clause` — MySQL's
// `ON DUPLICATE KEY UPDATE` has no target-column list and can't be
// translated 1:1). Test is correct; the framework needs a separate
// emit path (or a new IR variant) for the MySQL upsert shape.
#[tokio::test]
#[ignore = "framework: ConflictClause::DoUpdate with target columns unsupported on MySQL writer"]
async fn user_perm_overrides_and_clear_work_via_pool_on_mysql() {
    let _g = live_lock().lock().await;
    let Some(pool) = mysql_pool_or_skip().await else {
        eprintln!("MYSQL_TEST_URL unset — skipping");
        return;
    };
    let uid = make_user(&pool, "eve_override").await;
    set_user_perm_pool(uid, "perm_mysql_blog_post.publish", true, &pool)
        .await
        .expect("set_user_perm_pool grant");
    let ok = has_perm_pool(uid, "perm_mysql_blog_post.publish", &pool)
        .await
        .expect("has after grant");
    assert!(ok);

    set_user_perm_pool(uid, "perm_mysql_blog_post.publish", false, &pool)
        .await
        .expect("set_user_perm_pool deny");
    let denied = has_perm_pool(uid, "perm_mysql_blog_post.publish", &pool)
        .await
        .expect("has after deny");
    assert!(!denied);

    clear_user_perm_pool(uid, "perm_mysql_blog_post.publish", &pool)
        .await
        .expect("clear_user_perm_pool");
    let gone = has_perm_pool(uid, "perm_mysql_blog_post.publish", &pool)
        .await
        .expect("has after clear");
    assert!(!gone);
}

// Same framework gap as `user_perm_overrides_and_clear_work_via_pool_on_mysql`
// — this test also drives `set_user_perm_pool` through the unsupported
// MySQL upsert path.
#[tokio::test]
#[ignore = "framework: ConflictClause::DoUpdate with target columns unsupported on MySQL writer"]
async fn user_roles_pool_and_user_permissions_pool_on_mysql() {
    let _g = live_lock().lock().await;
    let Some(pool) = mysql_pool_or_skip().await else {
        eprintln!("MYSQL_TEST_URL unset — skipping");
        return;
    };
    let uid = make_user(&pool, "frank_multi").await;
    let r1 = create_role_pool("role_a", "", &pool).await.expect("a");
    let r2 = create_role_pool("role_b", "", &pool).await.expect("b");
    assign_role_pool(uid, r1, &pool).await.expect("assign a");
    assign_role_pool(uid, r2, &pool).await.expect("assign b");

    let roles = user_roles_pool(uid, &pool).await.expect("user_roles_pool");
    assert_eq!(roles.len(), 2);

    grant_role_perm_pool(r1, "perm_mysql_blog_post.view", &pool)
        .await
        .expect("grant via role");
    set_user_perm_pool(uid, "perm_mysql_blog_post.export", true, &pool)
        .await
        .expect("direct grant");
    let mut perms = user_permissions_pool(uid, &pool)
        .await
        .expect("user_permissions_pool");
    perms.sort();
    assert_eq!(
        perms,
        vec![
            "perm_mysql_blog_post.export".to_owned(),
            "perm_mysql_blog_post.view".to_owned(),
        ]
    );

    remove_role_pool(uid, r1, &pool).await.expect("remove a");
    let after = user_roles_pool(uid, &pool).await.expect("after remove");
    assert_eq!(after.len(), 1);
}

#[tokio::test]
async fn get_or_create_role_pool_is_idempotent_on_mysql() {
    let _g = live_lock().lock().await;
    let Some(pool) = mysql_pool_or_skip().await else {
        eprintln!("MYSQL_TEST_URL unset — skipping");
        return;
    };
    let id1 = get_or_create_role_pool("auditor", "Read-only", &pool)
        .await
        .expect("first");
    let id2 = get_or_create_role_pool("auditor", "different desc", &pool)
        .await
        .expect("second");
    assert_eq!(id1, id2, "second call should return existing role id");
}

#[tokio::test]
async fn typed_facade_for_model_pool_routes_through_codename_on_mysql() {
    let _g = live_lock().lock().await;
    let Some(pool) = mysql_pool_or_skip().await else {
        eprintln!("MYSQL_TEST_URL unset — skipping");
        return;
    };
    let uid = make_user(&pool, "henry_typed").await;
    let role_id = create_role_pool("typed", "", &pool).await.expect("role");
    let codename = rustango::permissions::codename_for::<Post>("change");
    assert_eq!(codename, "perm_mysql_blog_post.change");
    grant_role_perm_pool(role_id, &codename, &pool)
        .await
        .expect("grant");
    assign_role_pool(uid, role_id, &pool).await.expect("assign");
    let ok = rustango::permissions::has_perm_for_model_pool::<Post>(uid, "change", &pool)
        .await
        .expect("has_perm_for_model_pool");
    assert!(ok);
}
