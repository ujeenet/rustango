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
    // Bootstrap rustango_users — the `_pool` family doesn't auto-create
    // it; the production tenant bootstrap migration does.
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
    .execute(&pool)
    .await
    .expect("create users table");
    Pool::Sqlite(pool)
}

async fn seed_user(pool: &Pool, username: &str, plaintext_password: &str, active: bool) {
    let hash = rustango::tenancy::password::hash(plaintext_password).expect("hash");
    let Pool::Sqlite(sq) = pool else {
        unreachable!()
    };
    sqlx::query("INSERT INTO rustango_users (username, password_hash, active) VALUES (?, ?, ?)")
        .bind(username)
        .bind(&hash)
        .bind(if active { 1 } else { 0 })
        .execute(sq)
        .await
        .expect("seed user");
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
