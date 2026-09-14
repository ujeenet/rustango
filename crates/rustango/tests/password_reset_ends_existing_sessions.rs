//! A password reset stamps `password_changed_at`, so sessions issued
//! before it stop validating (#1449).
//!
//! The reset helpers rotated the hash and nothing else. `password_changed_at`
//! — the column the session middleware compares `iat` against — was never
//! written, and `NULL` is specifically the value that middleware reads as
//! "never rotated, do not enforce". So the check was not merely stale, it
//! was disabled.
//!
//! Password reset is what someone does when they believe their account is
//! compromised. It is the one flow where "sign me out everywhere" is the
//! whole point, and it was the flow that did not do it: the attacker's
//! session survived the victim's reset.
//!
//! The admin change-password path stamped it all along
//! (`tenancy/admin.rs`), so the same account reached two documented ways
//! got two different security outcomes — and the weaker one was the path
//! `docs/auth-flows.md` walks you through.
//!
//! ## What this file proves, and what it leans on
//!
//! Two links in one chain:
//!
//! 1. **reset -> column stamped** — here.
//! 2. **column stamped -> session rejected** — already pinned end-to-end by
//!    `admin_change_password_ui_live::session_minted_before_password_rotation_is_rejected`,
//!    which builds a real cookie with a past `iat`, stamps the column, and
//!    asserts the next request bounces to login.
//!
//! Asserting (1) here rather than rebuilding (2)'s Postgres router harness
//! keeps this test runnable on in-memory SQLite with no service. The column
//! is not a proxy for the behaviour — it is the input the middleware reads,
//! and the test above is what establishes that.

#![cfg(all(
    feature = "sqlite",
    feature = "passwords",
    feature = "auth_flows",
    feature = "tenancy"
))]

use std::time::Duration;

use rustango::auth_flows::{
    confirm_password_reset_pool, confirm_password_reset_pool_into, PasswordReset,
};
use rustango::sql::Pool;

const SECRET: &[u8] = b"a-strong-32-byte-secret-key-here";
const STRONG: &str = "brand-new-strong-password-7!";

/// `rustango_users` from `User::SCHEMA`, so the column set cannot drift
/// away from the one the middleware reads.
async fn pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    rustango::testkit::create_tables_for::<rustango::tenancy::User>(&pool)
        .await
        .expect("create rustango_users");
    let Pool::Sqlite(sq) = &pool else {
        unreachable!("connected to sqlite")
    };
    sqlx::query(
        "INSERT INTO rustango_users \
         (id, username, password_hash, is_superuser, active, created_at, password_changed_at) \
         VALUES (1, 'ada', 'OLD-HASH', 0, 1, datetime('now'), NULL)",
    )
    .execute(sq)
    .await
    .expect("seed user");
    pool
}

async fn password_changed_at(pool: &Pool) -> Option<String> {
    use sqlx::Row;
    let Pool::Sqlite(sq) = pool else {
        unreachable!()
    };
    sqlx::query("SELECT password_changed_at FROM rustango_users WHERE id = 1")
        .fetch_one(sq)
        .await
        .expect("fetch")
        .try_get::<Option<String>, _>("password_changed_at")
        .expect("column")
}

fn link() -> String {
    PasswordReset::issue(
        "https://example.com/auth/reset",
        1,
        SECRET,
        Duration::from_secs(60),
    )
}

/// The fix. A reset through the framework's own table ends the session.
#[tokio::test]
async fn a_reset_stamps_password_changed_at() {
    let pool = pool().await;
    assert_eq!(
        password_changed_at(&pool).await,
        None,
        "precondition: NULL means the middleware does not enforce"
    );

    confirm_password_reset_pool(&pool, &link(), STRONG, SECRET)
        .await
        .expect("reset");

    assert!(
        password_changed_at(&pool).await.is_some(),
        "a reset must stamp password_changed_at — otherwise every session \
         issued before it stays valid, including an attacker's, which is \
         the exact thing the user reset their password to stop"
    );
}

/// The caller-named form is deliberately unchanged.
///
/// It takes an arbitrary table and cannot assume a rotation column
/// exists, so writing one would break every custom-schema caller. Pinned
/// so the asymmetry is a decision on record rather than an oversight
/// someone "fixes" into a runtime error.
#[tokio::test]
async fn the_caller_named_form_does_not_stamp() {
    let pool = pool().await;

    confirm_password_reset_pool_into(
        &pool,
        &link(),
        STRONG,
        SECRET,
        "rustango_users",
        "id",
        "password_hash",
    )
    .await
    .expect("reset");

    assert_eq!(
        password_changed_at(&pool).await,
        None,
        "`_into` writes only the columns it was given — documented, and why \
         the defaults form is the one to use for rustango_users"
    );
}

/// The rotation must not cost the password write. Both land or neither
/// does — they are one UPDATE.
#[tokio::test]
async fn the_hash_still_rotates_alongside_the_stamp() {
    use sqlx::Row;
    let pool = pool().await;

    confirm_password_reset_pool(&pool, &link(), STRONG, SECRET)
        .await
        .expect("reset");

    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };
    let stored: String = sqlx::query("SELECT password_hash FROM rustango_users WHERE id = 1")
        .fetch_one(sq)
        .await
        .expect("fetch")
        .try_get("password_hash")
        .expect("column");

    assert_ne!(stored, "OLD-HASH", "the hash must have rotated");
    assert!(
        rustango::passwords::verify(STRONG, &stored).unwrap(),
        "and must verify against the new password"
    );
    assert!(password_changed_at(&pool).await.is_some());
}
