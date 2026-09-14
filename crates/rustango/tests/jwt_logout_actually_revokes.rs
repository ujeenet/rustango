//! Logout ends the session (#1402), and `JwtBackend` still accepts the
//! tokens the docs tell you to pair it with.
//!
//! Three defects composed into a logout that did nothing:
//!
//! 1. the revocation list was never read on the authentication path, so
//!    `revoke()` wrote to a blacklist nothing consulted;
//! 2. the token type was never checked, and access and refresh tokens are
//!    wire-identical — so a refresh token authenticated as a bearer, with
//!    its much longer life;
//! 3. logout never touched the refresh token at all.
//!
//! A user clicked log out, the request succeeded, and the session survived
//! for the full refresh TTL — seven days by default — on a credential the
//! endpoint never looked at.
//!
//! One test here is a regression guard rather than part of #1402
//! (`the_backend_accepts_a_lifecycle_access_token`): #1397 made
//! `JwtLifecycle` issue three-segment JWTs, and `JwtBackend` required
//! *exactly one dot* before it would even attempt verification. Between
//! those two changes the documented pairing silently stopped
//! authenticating anyone.

#![cfg(all(feature = "tenancy", feature = "sqlite"))]

use std::sync::Arc;

use axum::http::Request;
use rustango::jti_store::InMemoryJtiStore;
use rustango::sql::Pool;
use rustango::tenancy::auth_backends::{AuthBackend, JwtBackend};
use rustango::tenancy::jwt_lifecycle::JwtLifecycle;

fn secret() -> Vec<u8> {
    vec![7u8; 32]
}

/// User 42 in an in-memory SQLite, from `User::SCHEMA` so the DDL can't drift.
async fn pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    rustango::testkit::create_tables_for::<rustango::tenancy::User>(&pool)
        .await
        .expect("create users");
    let Pool::Sqlite(sq) = &pool else {
        unreachable!("connected to sqlite")
    };
    sqlx::query(
        "INSERT INTO rustango_users (id, username, password_hash, is_superuser, active, created_at) \
         VALUES (42, 'ada', '', 0, 1, datetime('now'))",
    )
    .execute(sq)
    .await
    .expect("seed user");
    pool
}

async fn auth(backend: &JwtBackend, pool: &Pool, token: &str) -> Option<i64> {
    let req = Request::builder()
        .header("authorization", format!("Bearer {token}"))
        .body(())
        .unwrap();
    let (parts, ()) = req.into_parts();
    backend
        .authenticate(&parts, pool)
        .await
        .ok()
        .flatten()
        .map(|u| u.id)
}

/// The regression guard. `JwtLifecycle` issues three-segment JWTs since
/// #1397; `JwtBackend` demanded exactly one dot and returned "not my
/// token" before verifying. `docs/auth-jwt-api.md` tells you to pair
/// them, so that window broke the documented setup for everyone.
#[tokio::test]
async fn the_backend_accepts_a_lifecycle_access_token() {
    let pool = pool().await;
    let life = JwtLifecycle::new(secret());
    let backend = JwtBackend::new(secret());
    let pair = life.issue_pair(42);

    assert_eq!(pair.access.split('.').count(), 3, "a JWT is three segments");
    assert_eq!(
        auth(&backend, &pool, &pair.access).await,
        Some(42),
        "the backend must authenticate the tokens the docs pair it with"
    );
}

/// #1402 (2) — a refresh token is not an access credential.
///
/// The two are wire-identical apart from `typ`, so without the check a
/// stolen refresh token is a bearer credential with days of life instead
/// of minutes.
#[tokio::test]
async fn a_refresh_token_is_refused_as_a_bearer() {
    let pool = pool().await;
    let life = JwtLifecycle::new(secret());
    let backend = JwtBackend::new(secret());
    let pair = life.issue_pair(42);

    assert_eq!(
        auth(&backend, &pool, &pair.refresh).await,
        None,
        "a refresh token must never authenticate as a bearer"
    );
}

/// #1402 (1) — the blacklist is read on the authentication path.
///
/// Without this, `revoke()` writes to a store nothing consults: a
/// deployment can wire a shared Redis `JtiStore`, watch it fill with
/// revoked JTIs, and have every one of those tokens still work.
#[tokio::test]
async fn a_revoked_access_token_stops_authenticating() {
    let pool = pool().await;
    let store = Arc::new(InMemoryJtiStore::new());
    let life = JwtLifecycle::new(secret()).with_jti_store(store.clone());
    let backend = JwtBackend::new(secret()).with_jti_store(store);
    let pair = life.issue_pair(42);

    assert_eq!(
        auth(&backend, &pool, &pair.access).await,
        Some(42),
        "valid before revocation"
    );

    life.revoke(&pair.access).await;

    assert_eq!(
        auth(&backend, &pool, &pair.access).await,
        None,
        "a revoked token must stop authenticating — this is the whole point of logout"
    );
}

/// Revocation is opt-in, and the default is unchanged.
///
/// Pinned deliberately: turning enforcement on silently would change what
/// an existing deployment's live tokens do, which is not a patch-release
/// thing to do. A backend with no store behaves exactly as before.
#[tokio::test]
async fn without_a_store_the_old_behaviour_is_unchanged() {
    let pool = pool().await;
    let store = Arc::new(InMemoryJtiStore::new());
    let life = JwtLifecycle::new(secret()).with_jti_store(store);
    let backend = JwtBackend::new(secret()); // no store wired
    let pair = life.issue_pair(42);

    life.revoke(&pair.access).await;

    assert_eq!(
        auth(&backend, &pool, &pair.access).await,
        Some(42),
        "no store means no enforcement — unchanged, and documented as such"
    );
}

/// The two stores must be the same one. Wiring separate stores means
/// logout writes to one and verification reads the other — which looks
/// configured and enforces nothing, the exact shape of the original bug.
#[tokio::test]
async fn two_separate_stores_do_not_share_revocations() {
    let pool = pool().await;
    let life = JwtLifecycle::new(secret()).with_jti_store(Arc::new(InMemoryJtiStore::new()));
    let backend = JwtBackend::new(secret()).with_jti_store(Arc::new(InMemoryJtiStore::new()));
    let pair = life.issue_pair(42);

    life.revoke(&pair.access).await;

    assert_eq!(
        auth(&backend, &pool, &pair.access).await,
        Some(42),
        "documenting the footgun: revocation does not cross store instances"
    );
}

/// #1402 (3) — revoking the refresh token ends the session.
///
/// This is the mechanism `/api/auth/logout` now invokes for the refresh
/// half of the pair, and the reason the endpoint had to start reading a
/// body at all. `revoke` decodes without checking `typ`, so it accepts a
/// refresh token; `refresh()` re-verifies through the blacklist, so a
/// revoked one mints nothing.
///
/// Without it, logout revoked a 15-minute access token and left a
/// seven-day credential that could mint replacements the whole time —
/// the session outliving the logout by a week.
#[tokio::test]
async fn a_revoked_refresh_token_cannot_mint_new_access_tokens() {
    let life = JwtLifecycle::new(secret());
    let pair = life.issue_pair(42);

    assert!(
        life.refresh(&pair.refresh).await.is_some(),
        "mints a new pair before revocation"
    );

    let second = life.issue_pair(42);
    assert!(
        life.revoke(&second.refresh).await,
        "revoke accepts a refresh token"
    );
    assert!(
        life.refresh(&second.refresh).await.is_none(),
        "a revoked refresh token must not mint access tokens — logout has to end the session, not half of it"
    );
}

/// A token with no `typ` — what `JwtBackend::issue` emits — still works.
/// The type check must refuse refresh tokens without refusing the
/// backend's own.
#[tokio::test]
async fn the_backends_own_tokens_still_authenticate() {
    let pool = pool().await;
    let backend = JwtBackend::new(secret());
    let token = backend.issue(42);

    assert_eq!(
        token.split('.').count(),
        2,
        "issue() emits the legacy shape"
    );
    assert_eq!(auth(&backend, &pool, &token).await, Some(42));
}
