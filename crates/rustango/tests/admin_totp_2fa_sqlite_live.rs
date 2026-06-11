#![cfg(all(feature = "sqlite", feature = "admin", feature = "totp"))]
//! Live SQLite test for the admin TOTP 2FA store + gating logic —
//! issue #367. Covers the security-critical invariants the login
//! challenge relies on:
//! - only a **confirmed** device gates login (`confirmed_secret`
//!   returns `None` for a pending enrollment);
//! - the stored secret round-trips so a code generated against it
//!   verifies (this is exactly what `login_submit` does);
//! - re-enrollment replaces the device and drops back to pending.
//!
//! The RFC 6238 math itself is covered by `crate::totp`'s own tests;
//! here we exercise the persistence + the enroll → confirm → verify
//! lifecycle end-to-end against a real engine.

use rustango::admin::totp_store;
use rustango::sql::{sqlx, Pool};
use rustango::totp::{self, TotpSecret};

async fn pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    let pool: Pool = p.into();
    totp_store::ensure_table(&pool).await.expect("ensure_table");
    pool
}

#[tokio::test]
async fn enroll_confirm_verify_lifecycle() {
    let pool = pool().await;
    let uid = 42_i64;

    // No device yet → nothing gates login.
    assert!(totp_store::confirmed_secret(&pool, uid).await.is_none());
    assert!(totp_store::device(&pool, uid).await.is_none());

    // Start enrollment → a *pending* (unconfirmed) device exists, but it
    // must NOT gate login until confirmed.
    let secret = TotpSecret::generate();
    totp_store::start_enrollment(&pool, uid, &secret)
        .await
        .expect("start_enrollment");
    let dev = totp_store::device(&pool, uid).await.expect("device");
    assert!(!dev.confirmed, "fresh enrollment is pending");
    assert!(
        totp_store::confirmed_secret(&pool, uid).await.is_none(),
        "a pending device must not gate login"
    );

    // Confirm → now it gates login, and the stored secret round-trips.
    totp_store::confirm(&pool, uid).await.expect("confirm");
    let got = totp_store::confirmed_secret(&pool, uid)
        .await
        .expect("confirmed secret present");
    assert_eq!(got.0, secret.0, "stored secret round-trips byte-for-byte");

    // A code generated against the stored secret verifies (the exact
    // check `login_submit` performs); a wrong code does not.
    let t = 1_700_000_000_u64;
    let code = totp::generate_at(&got, t, 30, 6);
    assert!(
        totp::verify_at(&got, &code, t, 30, 6, 1),
        "valid code passes"
    );
    let bad = if code == "000000" { "111111" } else { "000000" };
    assert!(
        !totp::verify_at(&got, bad, t, 30, 6, 1),
        "wrong code rejected"
    );

    // Re-enrollment replaces the device and drops back to pending.
    let secret2 = TotpSecret::generate();
    totp_store::start_enrollment(&pool, uid, &secret2)
        .await
        .expect("re-enroll");
    assert!(
        totp_store::confirmed_secret(&pool, uid).await.is_none(),
        "re-enrollment is pending until confirmed again"
    );
    // Exactly one device per user (the old row was replaced, not added).
    totp_store::confirm(&pool, uid).await.unwrap();
    let got2 = totp_store::confirmed_secret(&pool, uid).await.unwrap();
    assert_eq!(got2.0, secret2.0, "new secret is the active one");
    assert_ne!(got2.0, secret.0, "old secret no longer stored");
}
