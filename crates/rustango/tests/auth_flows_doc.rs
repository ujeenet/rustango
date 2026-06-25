//! Backing test for `docs/auth-flows.md` — signed URLs (the substrate) plus the
//! password-reset confirm flow. Pure sign/verify + an in-memory SQLite
//! reset-confirm round-trip.
//!
//! Run: `cargo test -p rustango --features sqlite --test auth_flows_doc`

#![cfg(all(feature = "sqlite", feature = "passwords", feature = "auth_flows"))]

use std::time::Duration;

use rustango::auth_flows::{confirm_password_reset_pool_into, AuthFlowError, PasswordReset};
use rustango::core::SqlValue;
use rustango::signed_url::{sign, sign_at, verify, verify_at, SignedUrlError};
use rustango::sql::Pool;

const SECRET: &[u8] = b"a-strong-32-byte-secret-key-here!";

// ----------------------------------------------------------- signed URLs

#[test]
fn signed_url_roundtrip_and_tamper() {
    let url = "https://app.example.com/files/42?user_id=7";
    let signed = sign(url, SECRET, None); // None = never expires
    assert!(signed.contains("signature="));
    assert!(verify(&signed, SECRET).is_ok());

    // Flip a byte of the signed payload → signature no longer matches.
    let tampered = signed.replace("user_id=7", "user_id=8");
    assert_eq!(
        verify(&tampered, SECRET),
        Err(SignedUrlError::InvalidSignature)
    );

    // A different secret can't verify it either.
    assert_eq!(
        verify(&signed, b"some-other-secret-key-of-len-32!"),
        Err(SignedUrlError::InvalidSignature)
    );
}

#[test]
fn signed_url_expiry_is_deterministic() {
    let url = "https://app.example.com/reset";
    // sign_at / verify_at take explicit unix seconds — no wall-clock flake.
    let signed = sign_at(url, SECRET, Some(100)); // expires at t=100
    assert!(verify_at(&signed, SECRET, 50).is_ok()); // before expiry: ok
    assert_eq!(
        verify_at(&signed, SECRET, 1000), // after expiry: rejected
        Err(SignedUrlError::Expired)
    );
}

// ------------------------------------------------- password reset confirm

async fn pool_with_user(initial_hash: &str) -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE IF NOT EXISTS "users" (
            "id" INTEGER PRIMARY KEY AUTOINCREMENT,
            "username" TEXT NOT NULL,
            "password_hash" TEXT NOT NULL
        )"#,
        Vec::new(),
    )
    .await
    .expect("create");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"INSERT INTO "users" ("username", "password_hash") VALUES (?, ?)"#,
        vec![
            SqlValue::String("alice".into()),
            SqlValue::String(initial_hash.into()),
        ],
    )
    .await
    .expect("seed");
    pool
}

async fn stored_hash(pool: &Pool, id: i64) -> String {
    use sqlx::Row;
    let Pool::Sqlite(sq) = pool else {
        unreachable!()
    };
    sqlx::query(r#"SELECT "password_hash" FROM "users" WHERE "id" = ?"#)
        .bind(id)
        .fetch_one(sq)
        .await
        .expect("fetch")
        .try_get::<String, _>("password_hash")
        .unwrap()
}

#[tokio::test]
async fn password_reset_confirm_rotates_the_hash() {
    let pool = pool_with_user("OLD-PLACEHOLDER-HASH").await;

    // 1. Issue a reset link (you'd email this). Token encodes user_id + purpose.
    let url = PasswordReset::issue(
        "https://app.example.com/auth/reset",
        1,
        SECRET,
        Duration::from_secs(3600),
    );

    // 2. User submits a new password → verify token + rotate the stored hash.
    let user_id = confirm_password_reset_pool_into(
        &pool,
        &url,
        "a-brand-new-strong-password",
        SECRET,
        "users",
        "id",
        "password_hash",
    )
    .await
    .expect("confirm");
    assert_eq!(user_id, 1);

    let new_hash = stored_hash(&pool, 1).await;
    assert_ne!(new_hash, "OLD-PLACEHOLDER-HASH", "hash was rotated");
    assert!(new_hash.starts_with("$argon2"), "argon2id PHC string");
    assert!(rustango::passwords::verify("a-brand-new-strong-password", &new_hash).unwrap());
}

#[tokio::test]
async fn password_reset_rejects_weak_and_tampered() {
    // Weak password → rejected, nothing written.
    let pool = pool_with_user("KEEP-ME").await;
    let url = PasswordReset::issue(
        "https://app.example.com/auth/reset",
        1,
        SECRET,
        Duration::from_secs(3600),
    );
    let err = confirm_password_reset_pool_into(
        &pool,
        &url,
        "short",
        SECRET,
        "users",
        "id",
        "password_hash",
    )
    .await
    .unwrap_err();
    assert!(matches!(err, AuthFlowError::WeakPassword(_)));
    assert_eq!(
        stored_hash(&pool, 1).await,
        "KEEP-ME",
        "no write on weak pw"
    );

    // Tampered token (user_id 1 → 2) → InvalidSignature.
    let tampered = url.replace("user_id=1", "user_id=2");
    let err = confirm_password_reset_pool_into(
        &pool,
        &tampered,
        "a-brand-new-strong-password",
        SECRET,
        "users",
        "id",
        "password_hash",
    )
    .await
    .unwrap_err();
    assert!(matches!(err, AuthFlowError::InvalidSignature));
}
