#![allow(irrefutable_let_patterns)] // Pool enum is single-variant in sqlite-only builds; pattern is refutable on multi-backend builds.
//! Live integration tests for the tri-dialect tenant-auth `_pool`
//! helpers on SQLite — slice 25 / 27.
//!
//! Specifically:
//!   * `tenancy::auth::authenticate_user_pool` — verifies a tenant
//!     user's password via the unified `crate::sql::Pool` enum (was
//!     PG-only `&mut PgConnection` before slice 25).
//!   * `tenancy::auth_backends::ensure_api_keys_table_pool` — bootstraps
//!     the `rustango_api_keys` table per-dialect.

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "passwords"))]

use rustango::sql::{sqlx, Pool};

async fn sqlite_pool() -> Pool {
    let pool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory pool");
    let pool = Pool::Sqlite(pool);
    // Bootstrap rustango_users from `User::SCHEMA` via the same DDL
    // emitter the migration runner uses — no hand-written CREATE TABLE
    // to drift when the model gains a column.
    rustango::testkit::create_tables_for::<rustango::tenancy::User>(&pool)
        .await
        .expect("create users table");
    pool
}

async fn seed_user(pool: &Pool, username: &str, plaintext_password: &str, active: bool) {
    let hash = rustango::tenancy::password::hash(plaintext_password).expect("hash");
    // Insert through the model (sets created_at etc. from the struct)
    // rather than a raw INSERT that leaned on hand-written column
    // defaults.
    let mut u = rustango::tenancy::User {
        username: username.into(),
        password_hash: hash,
        active,
        ..rustango::testkit::user()
    };
    u.insert_pool(pool).await.expect("seed user");
}

#[tokio::test]
async fn authenticate_user_pool_returns_some_for_valid_credentials() {
    let pool = sqlite_pool().await;
    seed_user(&pool, "valid_user", "secret123", true).await;
    let user = rustango::tenancy::authenticate_user_pool(&pool, "valid_user", "secret123")
        .await
        .expect("authenticate_user_pool");
    let user = user.expect("user should authenticate");
    assert_eq!(user.username, "valid_user");
    assert!(user.active);
}

#[tokio::test]
async fn authenticate_user_pool_returns_none_for_wrong_password() {
    let pool = sqlite_pool().await;
    seed_user(&pool, "wrong_pw", "secret123", true).await;
    let user = rustango::tenancy::authenticate_user_pool(&pool, "wrong_pw", "WRONG")
        .await
        .expect("authenticate_user_pool");
    assert!(
        user.is_none(),
        "wrong password should return None, not an error"
    );
}

#[tokio::test]
async fn authenticate_user_pool_returns_none_for_unknown_user() {
    let pool = sqlite_pool().await;
    let user = rustango::tenancy::authenticate_user_pool(&pool, "ghost", "doesnotmatter")
        .await
        .expect("authenticate_user_pool");
    assert!(user.is_none(), "unknown user should return None");
}

#[tokio::test]
async fn authenticate_user_pool_returns_none_for_inactive_user() {
    let pool = sqlite_pool().await;
    seed_user(&pool, "inactive", "secret123", false).await;
    let user = rustango::tenancy::authenticate_user_pool(&pool, "inactive", "secret123")
        .await
        .expect("authenticate_user_pool");
    assert!(
        user.is_none(),
        "inactive user should be rejected even with correct password"
    );
}

#[cfg(feature = "api_keys")]
#[tokio::test]
async fn ensure_api_keys_table_pool_creates_and_is_idempotent_on_sqlite() {
    let pool = sqlite_pool().await;
    rustango::tenancy::auth_backends::ensure_api_keys_table_pool(&pool)
        .await
        .expect("first call");
    // Second call should be a no-op (IF NOT EXISTS semantics).
    rustango::tenancy::auth_backends::ensure_api_keys_table_pool(&pool)
        .await
        .expect("second call idempotent");
    // Probe — the table exists.
    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='rustango_api_keys'",
    )
    .fetch_one(sq)
    .await
    .expect("count");
    assert_eq!(
        count, 1,
        "rustango_api_keys should exist after ensure_api_keys_table_pool"
    );
}
