//! Live integration tests for the tri-dialect `permissions::*_pool`
//! family on SQLite — proves slice 25's new `_pool` companions
//! (`has_perm_pool`, `has_any_perm_pool`, `has_all_perms_pool`,
//! `grant_role_perm_pool`, `revoke_role_perm_pool`,
//! `set_user_perm_pool`, `clear_user_perm_pool`,
//! `create_role_pool`, `get_or_create_role_pool`, `assign_role_pool`,
//! `remove_role_pool`, `user_roles_pool`, `user_permissions_pool`,
//! `user_roles_qs_pool`, `ensure_tables_pool`, and the typed
//! `*_for_model_pool` facade) actually work end-to-end on SQLite
//! instead of just compiling.

#![cfg(all(feature = "sqlite", feature = "tenancy"))]

use rustango::sql::{sqlx, Auto, Pool};
use rustango::tenancy::permissions::{
    assign_role_pool, clear_user_perm_pool, create_role_pool, ensure_tables_pool,
    get_or_create_role_pool, grant_role_perm_pool, has_all_perms_pool, has_any_perm_pool,
    has_perm_pool, remove_role_pool, revoke_role_perm_pool, set_user_perm_pool,
    user_permissions_pool, user_roles_pool, user_roles_qs_pool,
};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "perm_sqlite_blog_post")]
#[rustango(app = "perm_sqlite_blog")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

async fn sqlite_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory pool");
    // Bootstrap the rustango_users table the permissions engine
    // joins against. The `_pool` family doesn't auto-create it; in
    // production it's created by the tenant bootstrap migration.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS rustango_users (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            username TEXT NOT NULL UNIQUE, \
            password_hash TEXT NOT NULL DEFAULT '', \
            is_superuser INTEGER NOT NULL DEFAULT 0, \
            active INTEGER NOT NULL DEFAULT 1, \
            data TEXT NOT NULL DEFAULT '{}', \
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')), \
            password_changed_at TEXT)",
    )
    .execute(&p)
    .await
    .expect("create rustango_users");
    let pool = Pool::Sqlite(p);
    // Bootstrap roles + permissions + the join tables.
    ensure_tables_pool(&pool).await.expect("ensure_tables_pool");
    pool
}

async fn make_user(pool: &Pool, name: &str) -> i64 {
    let Pool::Sqlite(sq) = pool else {
        unreachable!()
    };
    sqlx::query("INSERT INTO rustango_users (username) VALUES (?)")
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

#[tokio::test]
async fn has_perm_pool_returns_false_when_no_grant() {
    let pool = sqlite_pool().await;
    let uid = make_user(&pool, "alice_none").await;
    let ok = has_perm_pool(uid, "perm_sqlite_blog_post.change", &pool)
        .await
        .expect("has_perm_pool");
    assert!(!ok, "user without any role should have no perms");
}

#[tokio::test]
async fn role_grant_flows_to_has_perm_pool() {
    let pool = sqlite_pool().await;
    let uid = make_user(&pool, "bob_editor").await;
    let role_id = create_role_pool("editor", "Edits posts", &pool)
        .await
        .expect("create_role_pool");
    grant_role_perm_pool(role_id, "perm_sqlite_blog_post.change", &pool)
        .await
        .expect("grant_role_perm_pool");
    assign_role_pool(uid, role_id, &pool)
        .await
        .expect("assign_role_pool");
    let ok = has_perm_pool(uid, "perm_sqlite_blog_post.change", &pool)
        .await
        .expect("has_perm_pool");
    assert!(ok, "user with editor role should have post.change");

    // Revoke + re-check.
    revoke_role_perm_pool(role_id, "perm_sqlite_blog_post.change", &pool)
        .await
        .expect("revoke_role_perm_pool");
    let ok = has_perm_pool(uid, "perm_sqlite_blog_post.change", &pool)
        .await
        .expect("has_perm_pool after revoke");
    assert!(!ok, "after revoke the user should lose the perm");
}

#[tokio::test]
async fn has_any_perm_pool_short_circuits_on_first_hit() {
    let pool = sqlite_pool().await;
    let uid = make_user(&pool, "carol_partial").await;
    let role_id = create_role_pool("partial", "", &pool)
        .await
        .expect("create_role_pool");
    grant_role_perm_pool(role_id, "perm_sqlite_blog_post.view", &pool)
        .await
        .expect("grant_role_perm_pool");
    assign_role_pool(uid, role_id, &pool)
        .await
        .expect("assign_role_pool");
    let ok = has_any_perm_pool(
        uid,
        &["perm_sqlite_blog_post.delete", "perm_sqlite_blog_post.view"],
        &pool,
    )
    .await
    .expect("has_any_perm_pool");
    assert!(ok, "should hit on post.view (second slot)");

    let none = has_any_perm_pool(
        uid,
        &["perm_sqlite_blog_post.delete", "perm_sqlite_blog_post.add"],
        &pool,
    )
    .await
    .expect("has_any_perm_pool none");
    assert!(!none, "no granted codename in list — should be false");
}

#[tokio::test]
async fn has_all_perms_pool_returns_true_only_when_every_codename_granted() {
    let pool = sqlite_pool().await;
    let uid = make_user(&pool, "dave_admin").await;
    let role_id = create_role_pool("admin", "", &pool)
        .await
        .expect("create_role_pool");
    for action in ["add", "change", "delete", "view"] {
        let codename = format!("perm_sqlite_blog_post.{action}");
        grant_role_perm_pool(role_id, &codename, &pool)
            .await
            .expect("grant_role_perm_pool");
    }
    assign_role_pool(uid, role_id, &pool)
        .await
        .expect("assign_role_pool");
    let all = has_all_perms_pool(
        uid,
        &[
            "perm_sqlite_blog_post.add",
            "perm_sqlite_blog_post.change",
            "perm_sqlite_blog_post.view",
        ],
        &pool,
    )
    .await
    .expect("has_all_perms_pool");
    assert!(all, "admin role grants all four CRUD codenames");

    // Add a codename the user does NOT have.
    let partial = has_all_perms_pool(
        uid,
        &[
            "perm_sqlite_blog_post.change",
            "perm_sqlite_blog_post.export",
        ],
        &pool,
    )
    .await
    .expect("has_all_perms_pool partial");
    assert!(!partial, "missing one codename should fail the check");
}

#[tokio::test]
async fn get_or_create_role_pool_is_idempotent() {
    let pool = sqlite_pool().await;
    let id1 = get_or_create_role_pool("auditor", "Read-only", &pool)
        .await
        .expect("first call");
    let id2 = get_or_create_role_pool("auditor", "doesn't matter", &pool)
        .await
        .expect("second call");
    assert_eq!(id1, id2, "second call should return existing role id");
}

#[tokio::test]
async fn user_perm_overrides_and_clear_work_via_pool() {
    let pool = sqlite_pool().await;
    let uid = make_user(&pool, "eve_override").await;
    // No role; explicit per-user grant.
    set_user_perm_pool(uid, "perm_sqlite_blog_post.publish", true, &pool)
        .await
        .expect("set_user_perm_pool grant");
    let ok = has_perm_pool(uid, "perm_sqlite_blog_post.publish", &pool)
        .await
        .expect("has_perm_pool after grant");
    assert!(ok, "explicit user grant should win");

    // Switch to explicit denial.
    set_user_perm_pool(uid, "perm_sqlite_blog_post.publish", false, &pool)
        .await
        .expect("set_user_perm_pool deny");
    let ok = has_perm_pool(uid, "perm_sqlite_blog_post.publish", &pool)
        .await
        .expect("has_perm_pool after deny");
    assert!(!ok, "explicit denial should override");

    // Clear and verify default-false behavior.
    clear_user_perm_pool(uid, "perm_sqlite_blog_post.publish", &pool)
        .await
        .expect("clear_user_perm_pool");
    let ok = has_perm_pool(uid, "perm_sqlite_blog_post.publish", &pool)
        .await
        .expect("has_perm_pool after clear");
    assert!(!ok, "after clear (and no role), user has no perm");
}

#[tokio::test]
async fn user_roles_pool_and_user_roles_qs_pool_return_consistent_results() {
    let pool = sqlite_pool().await;
    let uid = make_user(&pool, "frank_multi").await;
    let r1 = create_role_pool("role_a", "", &pool).await.expect("a");
    let r2 = create_role_pool("role_b", "", &pool).await.expect("b");
    assign_role_pool(uid, r1, &pool).await.expect("assign a");
    assign_role_pool(uid, r2, &pool).await.expect("assign b");

    let roles_typed = user_roles_pool(uid, &pool).await.expect("user_roles_pool");
    assert_eq!(roles_typed.len(), 2);
    let mut names: Vec<&str> = roles_typed.iter().map(|(_, n)| n.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["role_a", "role_b"]);

    let roles_qs = user_roles_qs_pool(uid, &pool)
        .await
        .expect("user_roles_qs_pool");
    assert_eq!(roles_qs.len(), 2);

    // Remove one and re-check.
    remove_role_pool(uid, r1, &pool).await.expect("remove a");
    let after = user_roles_pool(uid, &pool)
        .await
        .expect("user_roles_pool 2");
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].1, "role_b");
}

#[tokio::test]
async fn user_permissions_pool_unions_role_and_direct_grants() {
    let pool = sqlite_pool().await;
    let uid = make_user(&pool, "grace_union").await;
    let role_id = create_role_pool("union", "", &pool).await.expect("role");
    grant_role_perm_pool(role_id, "perm_sqlite_blog_post.view", &pool)
        .await
        .expect("grant view via role");
    assign_role_pool(uid, role_id, &pool)
        .await
        .expect("assign role");
    set_user_perm_pool(uid, "perm_sqlite_blog_post.export", true, &pool)
        .await
        .expect("direct grant export");
    let mut perms = user_permissions_pool(uid, &pool)
        .await
        .expect("user_permissions_pool");
    perms.sort();
    assert_eq!(
        perms,
        vec![
            "perm_sqlite_blog_post.export".to_owned(),
            "perm_sqlite_blog_post.view".to_owned(),
        ]
    );
}

#[tokio::test]
async fn typed_facade_has_perm_for_model_pool_routes_through_codename() {
    let pool = sqlite_pool().await;
    let uid = make_user(&pool, "henry_typed").await;
    let role_id = create_role_pool("typed", "", &pool).await.expect("role");
    // Build the codename via the typed facade's helper, grant via the
    // string codename, verify the typed lookup matches.
    let codename = rustango::permissions::codename_for::<Post>("change");
    assert_eq!(codename, "perm_sqlite_blog_post.change");
    grant_role_perm_pool(role_id, &codename, &pool)
        .await
        .expect("grant");
    assign_role_pool(uid, role_id, &pool).await.expect("assign");

    let ok = rustango::permissions::has_perm_for_model_pool::<Post>(uid, "change", &pool)
        .await
        .expect("has_perm_for_model_pool");
    assert!(ok, "typed facade should agree with raw codename lookup");

    // Same shape for grant + revoke.
    let role2 = create_role_pool("typed2", "", &pool).await.expect("role2");
    rustango::permissions::grant_role_perm_for_model_pool::<Post>(role2, "delete", &pool)
        .await
        .expect("grant typed");
    assign_role_pool(uid, role2, &pool).await.expect("assign 2");
    let ok = rustango::permissions::has_perm_for_model_pool::<Post>(uid, "delete", &pool)
        .await
        .expect("has typed delete");
    assert!(ok);
    rustango::permissions::revoke_role_perm_for_model_pool::<Post>(role2, "delete", &pool)
        .await
        .expect("revoke typed");
    let gone = rustango::permissions::has_perm_for_model_pool::<Post>(uid, "delete", &pool)
        .await
        .expect("has typed delete after revoke");
    assert!(!gone, "after typed revoke the perm should be gone");
}
