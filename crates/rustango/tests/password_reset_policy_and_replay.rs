//! The documented password-reset path applies the documented policy, and
//! can be made single-use (#1399).
//!
//! Two defects in the same function, both with a correct answer sitting a
//! few lines away in the same file:
//!
//! 1. the only check was `new_password.len() < 8`, so `12345678` was
//!    accepted at reset while `passwords::strength_score` — the policy
//!    `docs/auth-passwords.md` documents — rejects it. A user who could
//!    not set a weak password at registration could set one by resetting;
//! 2. the confirm helper called plain `verify`, so the reset link stayed
//!    replayable for its full TTL. `verify_single_use` existed, and
//!    `docs/auth-flows.md` recommended it *for reset* — the page
//!    contradicted the helper it documented.
//!
//! Replay is the sharper half. The link keeps working after the password
//! has changed, so a copy of the email — forwarded, archived, in a shared
//! inbox, pasted into a support ticket — is a working account takeover
//! for the rest of the TTL, after the legitimate user has finished and
//! has no reason to suspect anything.

#![cfg(all(
    feature = "sqlite",
    feature = "passwords",
    feature = "auth_flows",
    feature = "cache"
))]

use std::sync::Arc;
use std::time::Duration;

use rustango::auth_flows::{
    confirm_password_reset_pool_into, confirm_password_reset_single_use_into, AuthFlowError,
    PasswordReset,
};
use rustango::cache::{Cache, InMemoryCache};
use rustango::core::SqlValue;
use rustango::sql::Pool;

const SECRET: &[u8] = b"a-strong-32-byte-secret-key-here";
const STRONG: &str = "brand-new-strong-password-7!";

async fn pool_with_user() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE IF NOT EXISTS "prp_users" (
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
        r#"INSERT INTO "prp_users" ("username", "password_hash") VALUES (?, ?)"#,
        vec![
            SqlValue::String("alice".into()),
            SqlValue::String("OLD-HASH".into()),
        ],
    )
    .await
    .expect("seed user");
    pool
}

async fn current_hash(pool: &Pool) -> String {
    use sqlx::Row;
    let Pool::Sqlite(sq) = pool else {
        unreachable!("test is sqlite-only")
    };
    sqlx::query(r#"SELECT "password_hash" FROM "prp_users" WHERE "id" = 1"#)
        .fetch_one(sq)
        .await
        .expect("fetch hash")
        .try_get::<String, _>("password_hash")
        .unwrap()
}

fn link() -> String {
    PasswordReset::issue(
        "https://example.com/auth/reset",
        1,
        SECRET,
        Duration::from_secs(60),
    )
}

fn cache() -> Arc<dyn Cache> {
    Arc::new(InMemoryCache::new())
}

async fn confirm(pool: &Pool, url: &str, password: &str) -> Result<i64, AuthFlowError> {
    confirm_password_reset_pool_into(
        pool,
        url,
        password,
        SECRET,
        "prp_users",
        "id",
        "password_hash",
    )
    .await
}

async fn confirm_once(
    pool: &Pool,
    url: &str,
    password: &str,
    c: &Arc<dyn Cache>,
) -> Result<i64, AuthFlowError> {
    confirm_password_reset_single_use_into(
        pool,
        url,
        password,
        SECRET,
        c,
        "prp_users",
        "id",
        "password_hash",
    )
    .await
}

// ---------------------------------------------------------------- policy

/// #1399 (1) — the passwords the old length check let through.
///
/// Both are longer than the 8 characters that used to be the whole
/// policy, and both are refused by the framework's own `strength_score`.
#[tokio::test]
async fn passwords_the_documented_policy_rejects_are_rejected_here() {
    for weak in ["12345678", "password1", "abcdefghijkl"] {
        let pool = pool_with_user().await;
        let err = confirm(&pool, &link(), weak).await.unwrap_err();
        assert!(
            matches!(err, AuthFlowError::WeakPassword(_)),
            "{weak:?} must be refused by the documented policy, got {err:?}"
        );
        assert_eq!(
            current_hash(&pool).await,
            "OLD-HASH",
            "a rejected password must not be written"
        );
    }
}

/// The guard against "reject everything" — a strong password still lands.
#[tokio::test]
async fn a_strong_password_is_still_accepted() {
    let pool = pool_with_user().await;
    assert_eq!(confirm(&pool, &link(), STRONG).await.unwrap(), 1);

    let stored = current_hash(&pool).await;
    assert_ne!(stored, "OLD-HASH", "the hash must have rotated");
    assert!(rustango::passwords::verify(STRONG, &stored).unwrap());
}

// ---------------------------------------------------------------- replay

/// #1399 (2) — the second use of a reset link is refused.
#[tokio::test]
async fn a_reset_link_cannot_be_used_twice() {
    let pool = pool_with_user().await;
    let (url, c) = (link(), cache());

    assert_eq!(confirm_once(&pool, &url, STRONG, &c).await.unwrap(), 1);
    let after_first = current_hash(&pool).await;

    let err = confirm_once(&pool, &url, "second-password-9!", &c)
        .await
        .unwrap_err();
    assert!(
        matches!(err, AuthFlowError::AlreadyUsed),
        "a replayed reset link must be refused, got {err:?}"
    );
    assert_eq!(
        current_hash(&pool).await,
        after_first,
        "the replay must not have rewritten the password"
    );
}

/// A rejected password must not burn the link.
///
/// Pins the ordering: the policy is checked *before* the token is
/// consumed. Get it the other way round and a user who fumbles their new
/// password has to request a fresh email — a self-inflicted denial of
/// service that would look like the reset flow being broken.
#[tokio::test]
async fn a_weak_password_does_not_consume_the_link() {
    let pool = pool_with_user().await;
    let (url, c) = (link(), cache());

    let err = confirm_once(&pool, &url, "short", &c).await.unwrap_err();
    assert!(matches!(err, AuthFlowError::WeakPassword(_)));

    assert_eq!(
        confirm_once(&pool, &url, STRONG, &c).await.unwrap(),
        1,
        "the link must still work after a rejected password"
    );
}

/// Two caches are two blacklists — the same footgun as the JWT stores.
/// Pinned so nobody concludes single-use is a property of the token.
#[tokio::test]
async fn single_use_does_not_cross_cache_instances() {
    let pool = pool_with_user().await;
    let url = link();

    assert_eq!(
        confirm_once(&pool, &url, STRONG, &cache()).await.unwrap(),
        1
    );
    assert_eq!(
        confirm_once(&pool, &url, "another-password-3!", &cache())
            .await
            .unwrap(),
        1,
        "documenting the footgun: the used-marker lives in the cache, not the token"
    );
}

/// The replayable helper is deliberately unchanged, and documented as
/// such. Turning single-use on for every caller would need a cache the
/// old signature has nowhere to take.
#[tokio::test]
async fn the_plain_helper_is_still_replayable() {
    let pool = pool_with_user().await;
    let url = link();

    assert_eq!(confirm(&pool, &url, STRONG).await.unwrap(), 1);
    assert_eq!(
        confirm(&pool, &url, "another-password-3!").await.unwrap(),
        1,
        "unchanged on purpose — this is why the single-use variant exists"
    );
}
