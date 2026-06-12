#![cfg(all(feature = "sqlite", feature = "passkey"))]
//! Live SQLite test for the passkey credential store — issue #392
//! (foundation slice). The store uses only standard column types
//! (i64 / String / BLOB / timestamp), so it round-trips on SQLite with
//! no PostGIS / crypto involved — that's the ceremony layer's concern.
//!
//! `max_connections(1)` keeps DDL + writes + reads on one in-memory DB.

use rustango::passkey;
use rustango::sql::{sqlx, Pool};

async fn pool() -> Pool {
    let p = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite");
    let pool: Pool = p.into();
    passkey::ensure_table(&pool).await.expect("ensure_table");
    pool
}

#[tokio::test]
async fn ensure_table_is_idempotent() {
    let p = pool().await;
    passkey::ensure_table(&p)
        .await
        .expect("second create is a no-op");
    assert!(passkey::for_user(&p, 1).await.unwrap().is_empty());
}

#[tokio::test]
async fn register_lookup_and_bump_sign_count() {
    let p = pool().await;

    // Two passkeys for user 7, one for user 9.
    passkey::register(&p, 7, "cred-laptop", vec![1, 2, 3], 0, "Laptop")
        .await
        .unwrap();
    passkey::register(&p, 7, "cred-phone", vec![4, 5, 6], 0, "Phone")
        .await
        .unwrap();
    passkey::register(&p, 9, "cred-key", vec![7, 8, 9], 0, "YubiKey")
        .await
        .unwrap();

    // for_user lists only that user's credentials.
    let u7 = passkey::for_user(&p, 7).await.unwrap();
    assert_eq!(u7.len(), 2, "user 7 has two passkeys");
    assert_eq!(passkey::for_user(&p, 9).await.unwrap().len(), 1);
    assert!(passkey::for_user(&p, 42).await.unwrap().is_empty());

    // by_credential_id finds the exact credential + round-trips the key.
    let found = passkey::by_credential_id(&p, "cred-phone")
        .await
        .unwrap()
        .expect("cred-phone exists");
    assert_eq!(found.user_id, 7);
    assert_eq!(found.public_key, vec![4, 5, 6]);
    assert_eq!(found.sign_count, 0);
    assert_eq!(found.label, "Phone");
    assert!(passkey::by_credential_id(&p, "nope")
        .await
        .unwrap()
        .is_none());

    // Sign-count bump (clone/replay tracking) persists.
    passkey::update_sign_count(&p, "cred-phone", 5)
        .await
        .unwrap();
    let bumped = passkey::by_credential_id(&p, "cred-phone")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bumped.sign_count, 5);
    // The other credential is untouched.
    let laptop = passkey::by_credential_id(&p, "cred-laptop")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(laptop.sign_count, 0);
}

#[tokio::test]
async fn credential_id_is_unique() {
    let p = pool().await;
    passkey::register(&p, 1, "dup", vec![0], 0, "first")
        .await
        .unwrap();
    // Same credential_id again violates the UNIQUE constraint.
    let err = passkey::register(&p, 2, "dup", vec![1], 0, "second").await;
    assert!(err.is_err(), "duplicate credential_id must be rejected");
}
