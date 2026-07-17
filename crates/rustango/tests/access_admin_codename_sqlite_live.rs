#![allow(irrefutable_let_patterns)] // Pool is single-variant under sqlite-only builds.
//! Live SQLite regression for the `auth.access_admin` reserved
//! codename + the `permission_required` gate's underlying perm
//! resolution. Closes #311.
//!
//! Exercises:
//!   1. `seed_reserved_codename_pool` inserts a `rustango_permissions`
//!      row and is idempotent on re-seed.
//!   2. `has_perm_pool(uid, ACCESS_ADMIN_CODENAME, _)` returns false
//!      for a non-superuser without a grant.
//!   3. The superuser bypass returns true with no explicit grant.
//!   4. An explicit `set_user_perm_pool` grant flows through to
//!      `has_perm_pool`.
//!   5. `auto_create_permissions_pool` seeds the reserved row too
//!      (covers the boot-time path used by `manage migrate`).

#![cfg(all(feature = "sqlite", feature = "tenancy"))]

use rustango::sql::{sqlx, Pool};
use rustango::tenancy::permissions::{
    auto_create_permissions_pool, ensure_tables_pool, has_perm_pool, seed_reserved_codename_pool,
    set_user_perm_pool, ACCESS_ADMIN_CODENAME,
};

async fn fresh_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory pool");
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS rustango_users (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            username TEXT NOT NULL UNIQUE, \
            password_hash TEXT NOT NULL DEFAULT '', email TEXT, \
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
    ensure_tables_pool(&pool).await.expect("ensure_tables_pool");
    pool
}

async fn make_user(pool: &Pool, name: &str, is_superuser: bool) -> i64 {
    let Pool::Sqlite(sq) = pool else {
        unreachable!("test uses sqlite only")
    };
    sqlx::query("INSERT INTO rustango_users (username, is_superuser) VALUES (?, ?)")
        .bind(name)
        .bind(i64::from(is_superuser))
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

async fn count_rustango_permissions(pool: &Pool, codename: &str) -> i64 {
    let Pool::Sqlite(sq) = pool else {
        unreachable!()
    };
    let (n,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM rustango_permissions WHERE codename = ?")
            .bind(codename)
            .fetch_one(sq)
            .await
            .expect("count");
    n
}

#[tokio::test]
async fn seed_reserved_codename_pool_inserts_and_is_idempotent() {
    let pool = fresh_pool().await;
    assert_eq!(
        count_rustango_permissions(&pool, ACCESS_ADMIN_CODENAME).await,
        0
    );
    seed_reserved_codename_pool(
        &pool,
        "auth",
        ACCESS_ADMIN_CODENAME,
        "Can access framework admin",
    )
    .await
    .expect("first seed");
    assert_eq!(
        count_rustango_permissions(&pool, ACCESS_ADMIN_CODENAME).await,
        1
    );
    // Idempotent re-seed.
    seed_reserved_codename_pool(
        &pool,
        "auth",
        ACCESS_ADMIN_CODENAME,
        "Can access framework admin",
    )
    .await
    .expect("re-seed");
    assert_eq!(
        count_rustango_permissions(&pool, ACCESS_ADMIN_CODENAME).await,
        1
    );
}

#[tokio::test]
async fn has_perm_pool_returns_false_for_non_superuser_without_grant() {
    let pool = fresh_pool().await;
    let uid = make_user(&pool, "alice_no_grant", false).await;
    let ok = has_perm_pool(uid, ACCESS_ADMIN_CODENAME, &pool)
        .await
        .expect("has_perm_pool");
    assert!(!ok, "no grant + non-superuser ⇒ no access");
}

#[tokio::test]
async fn superuser_bypasses_access_admin_codename() {
    let pool = fresh_pool().await;
    let uid = make_user(&pool, "root_sb", true).await;
    let ok = has_perm_pool(uid, ACCESS_ADMIN_CODENAME, &pool)
        .await
        .expect("has_perm_pool");
    assert!(
        ok,
        "is_superuser=true ⇒ bypass every codename check (existing semantic)"
    );
}

#[tokio::test]
async fn explicit_grant_flows_through_to_has_perm_pool() {
    let pool = fresh_pool().await;
    let uid = make_user(&pool, "carol_granted", false).await;
    set_user_perm_pool(uid, ACCESS_ADMIN_CODENAME, true, &pool)
        .await
        .expect("set_user_perm_pool");
    let ok = has_perm_pool(uid, ACCESS_ADMIN_CODENAME, &pool)
        .await
        .expect("has_perm_pool");
    assert!(
        ok,
        "explicit per-user grant ⇒ access (no superuser bit needed)"
    );
}

#[tokio::test]
async fn auto_create_permissions_pool_seeds_reserved_codename() {
    let pool = fresh_pool().await;
    assert_eq!(
        count_rustango_permissions(&pool, ACCESS_ADMIN_CODENAME).await,
        0
    );
    auto_create_permissions_pool(&pool)
        .await
        .expect("auto_create_permissions_pool");
    assert_eq!(
        count_rustango_permissions(&pool, ACCESS_ADMIN_CODENAME).await,
        1,
        "auto_create_permissions_pool MUST also seed the reserved codename — \
         tenants that run `manage migrate` get the row without an extra call"
    );
}
