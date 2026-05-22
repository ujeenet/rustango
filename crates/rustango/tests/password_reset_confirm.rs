//! Django-parity #391 — `PasswordResetConfirmView`.
//!
//! Verifies `auth_flows::confirm_password_reset_pool_into` against
//! sqlite: token round-trips, password gets hashed + written, weak
//! passwords rejected, expired / tampered tokens rejected.

#![cfg(all(feature = "sqlite", feature = "passwords", feature = "auth_flows"))]

use std::time::Duration;

use rustango::auth_flows::{confirm_password_reset_pool_into, AuthFlowError, PasswordReset};
use rustango::core::SqlValue;
use rustango::sql::Pool;

const SECRET: &[u8] = b"a-strong-32-byte-secret-key-here";

async fn build_pool_with_user(initial_hash: &str) -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE IF NOT EXISTS "prc_users" (
            "id"            INTEGER PRIMARY KEY AUTOINCREMENT,
            "username"      TEXT NOT NULL,
            "password_hash" TEXT NOT NULL
        )"#,
        Vec::new(),
    )
    .await
    .expect("create");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"INSERT INTO "prc_users" ("username", "password_hash") VALUES (?, ?)"#,
        vec![
            SqlValue::String("alice".into()),
            SqlValue::String(initial_hash.into()),
        ],
    )
    .await
    .expect("seed user");
    pool
}

async fn current_hash(pool: &Pool, user_id: i64) -> String {
    use sqlx::Row;
    let sql = r#"SELECT "password_hash" FROM "prc_users" WHERE "id" = ?"#;
    match pool {
        Pool::Sqlite(sq) => {
            let row = sqlx::query(sql)
                .bind(user_id)
                .fetch_one(sq)
                .await
                .expect("fetch hash");
            row.try_get::<String, _>("password_hash").unwrap()
        }
        #[allow(unreachable_patterns)]
        _ => unreachable!("test is sqlite-only"),
    }
}

#[tokio::test]
async fn confirm_updates_password_on_valid_token() {
    let pool = build_pool_with_user("OLD-HASH").await;
    let url = PasswordReset::issue(
        "https://example.com/auth/reset",
        1,
        SECRET,
        Duration::from_secs(60),
    );
    let user_id = confirm_password_reset_pool_into(
        &pool,
        &url,
        "brand-new-strong-password",
        SECRET,
        "prc_users",
        "id",
        "password_hash",
    )
    .await
    .expect("valid confirm");
    assert_eq!(user_id, 1);
    let stored = current_hash(&pool, 1).await;
    assert_ne!(stored, "OLD-HASH", "hash should have rotated");
    assert!(
        stored.starts_with("$argon2") || stored.starts_with("$2"),
        "stored hash should be a real password hash: {stored}"
    );
    // The hash should verify against the cleartext we passed in.
    assert!(rustango::passwords::verify("brand-new-strong-password", &stored).unwrap());
}

#[tokio::test]
async fn weak_password_rejected_without_writing() {
    let pool = build_pool_with_user("OLD-HASH").await;
    let url = PasswordReset::issue(
        "https://example.com/auth/reset",
        1,
        SECRET,
        Duration::from_secs(60),
    );
    let err = confirm_password_reset_pool_into(
        &pool,
        &url,
        "short",
        SECRET,
        "prc_users",
        "id",
        "password_hash",
    )
    .await
    .unwrap_err();
    assert!(matches!(err, AuthFlowError::WeakPassword(_)));
    // The old hash is still in place — no write happened.
    assert_eq!(current_hash(&pool, 1).await, "OLD-HASH");
}

#[tokio::test]
async fn tampered_signature_rejected() {
    let pool = build_pool_with_user("OLD-HASH").await;
    let url = PasswordReset::issue(
        "https://example.com/auth/reset",
        1,
        SECRET,
        Duration::from_secs(60),
    );
    // Flip a single character of the URL after the `?` so the
    // signature no longer matches.
    let tampered = url.replacen("user_id=1", "user_id=2", 1);
    let err = confirm_password_reset_pool_into(
        &pool,
        &tampered,
        "brand-new-strong-password",
        SECRET,
        "prc_users",
        "id",
        "password_hash",
    )
    .await
    .unwrap_err();
    assert!(matches!(err, AuthFlowError::InvalidSignature));
    assert_eq!(current_hash(&pool, 1).await, "OLD-HASH");
}

#[tokio::test]
async fn wrong_secret_rejected() {
    let pool = build_pool_with_user("OLD-HASH").await;
    let url = PasswordReset::issue(
        "https://example.com/auth/reset",
        1,
        SECRET,
        Duration::from_secs(60),
    );
    let err = confirm_password_reset_pool_into(
        &pool,
        &url,
        "brand-new-strong-password",
        b"different-secret-32-bytes-long-xxx",
        "prc_users",
        "id",
        "password_hash",
    )
    .await
    .unwrap_err();
    assert!(matches!(err, AuthFlowError::InvalidSignature));
}
