//! Raw-credential bearer (epic #1013) — the copy-paste key path.
//!
//! A user-owned key's show-once `prefix.secret` token works directly as the
//! Bearer credential: [`rustango::mcp::verify_raw_agent_credential`] verifies
//! the secret (argon2, cached), re-checks liveness, and resolves grants
//! **per request** — so capabilities always track the owner's live RBAC and
//! revocation is immediate. Exercised end-to-end on SQLite:
//!
//!   create user → grant permission → skill(+tool) → map skill↔perm
//!     → create_user_key → verify raw credential → scope reflects RBAC
//!     → permission change reflects immediately → revoke → refused
//!
//! Run: `cargo test -p rustango --no-default-features --features sqlite,mcp,testkit --test mcp_raw_key`.
#![cfg(all(feature = "sqlite", feature = "mcp", feature = "testkit"))]
#![allow(irrefutable_let_patterns)] // Pool is single-variant in sqlite-only builds

use rustango::mcp::verify_raw_agent_credential;
use rustango::sql::{sqlx, Pool};
use rustango::tenancy::permissions::set_user_perm_pool;
use rustango::tenancy::{
    create_skill_pool, create_user_key_pool, map_skill_to_permission_pool, revoke_user_key_pool,
};

async fn world() -> Pool {
    let pool = Pool::Sqlite(
        sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("sqlite"),
    );
    rustango::testkit::migrate_framework(&pool)
        .await
        .expect("migrate framework");
    pool
}

async fn make_user(pool: &Pool, name: &str) -> i64 {
    let Pool::Sqlite(sq) = pool else {
        unreachable!()
    };
    sqlx::query(
        "INSERT INTO rustango_users (username, password_hash, is_superuser, active, created_at) \
         VALUES (?, '', 0, 1, datetime('now'))",
    )
    .bind(name)
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

async fn skill_world(pool: &Pool) {
    create_skill_pool(
        pool,
        "editor",
        "Editor",
        "edits things",
        "You edit.",
        &["edit_thing".into()],
    )
    .await
    .expect("skill");
    map_skill_to_permission_pool(pool, "editor", "thing.edit")
        .await
        .expect("map");
}

#[tokio::test]
async fn raw_credential_resolves_live_rbac_scope() {
    let pool = world().await;
    skill_world(&pool).await;
    let uid = make_user(&pool, "alice").await;
    set_user_perm_pool(uid, "thing.edit", true, &pool)
        .await
        .expect("grant perm");

    let issued = create_user_key_pool(&pool, uid, "alice's key", &[])
        .await
        .expect("key");

    // The raw show-once token authenticates directly.
    let agent = verify_raw_agent_credential(&pool, "acme", &issued.token)
        .await
        .expect("raw credential verifies");
    assert_eq!(agent.user_id, Some(uid));
    assert_eq!(agent.tenant, "acme");
    assert_eq!(agent.skills, vec!["editor"]);
    assert_eq!(agent.tools, vec!["edit_thing"]);
    assert!(agent.jti.starts_with("raw:"));

    // RBAC change reflects on the very next request (grants are never
    // cached), including through the argon2-skip cache-hit path.
    set_user_perm_pool(uid, "thing.edit", false, &pool)
        .await
        .expect("deny perm");
    let narrowed = verify_raw_agent_credential(&pool, "acme", &issued.token)
        .await
        .expect("still authenticates");
    assert!(narrowed.skills.is_empty(), "denied perm drops the skill");
    assert!(narrowed.tools.is_empty());
}

#[tokio::test]
async fn revoked_key_is_refused_immediately() {
    let pool = world().await;
    skill_world(&pool).await;
    let uid = make_user(&pool, "bob").await;
    set_user_perm_pool(uid, "thing.edit", true, &pool)
        .await
        .expect("grant");
    let issued = create_user_key_pool(&pool, uid, "bob's key", &[])
        .await
        .expect("key");
    let agent_id = issued.agent.id.get().copied().unwrap();

    // Warm the verification cache with a successful call…
    assert!(verify_raw_agent_credential(&pool, "acme", &issued.token)
        .await
        .is_some());
    // …then revoke: the liveness check runs per request, so even a cached
    // verification is refused straight away.
    revoke_user_key_pool(&pool, uid, agent_id)
        .await
        .expect("revoke");
    assert!(
        verify_raw_agent_credential(&pool, "acme", &issued.token)
            .await
            .is_none(),
        "revoked key must be refused despite the warm cache"
    );
}

#[tokio::test]
async fn deactivated_owner_is_refused() {
    let pool = world().await;
    let uid = make_user(&pool, "carol").await;
    let issued = create_user_key_pool(&pool, uid, "carol's key", &[])
        .await
        .expect("key");
    assert!(verify_raw_agent_credential(&pool, "acme", &issued.token)
        .await
        .is_some());

    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };
    sqlx::query("UPDATE rustango_users SET active = 0 WHERE id = ?")
        .bind(uid)
        .execute(sq)
        .await
        .expect("deactivate");
    assert!(
        verify_raw_agent_credential(&pool, "acme", &issued.token)
            .await
            .is_none(),
        "a deactivated owner's keys must be refused"
    );
}

#[tokio::test]
async fn garbage_and_wrong_secrets_are_refused() {
    let pool = world().await;
    let uid = make_user(&pool, "dave").await;
    let issued = create_user_key_pool(&pool, uid, "dave's key", &[])
        .await
        .expect("key");

    // No dot → shape gate refuses without touching the DB.
    assert!(verify_raw_agent_credential(&pool, "acme", "not-a-key")
        .await
        .is_none());
    // Empty halves.
    assert!(verify_raw_agent_credential(&pool, "acme", ".")
        .await
        .is_none());
    // Right prefix, wrong secret.
    let prefix = issued.token.split('.').next().unwrap();
    let forged = format!("{prefix}.deadbeefdeadbeefdeadbeefdeadbeef");
    assert!(verify_raw_agent_credential(&pool, "acme", &forged)
        .await
        .is_none());
    // A JWT-shaped bearer (three dot-separated base64 parts) must not
    // authenticate as a raw credential either.
    assert!(verify_raw_agent_credential(&pool, "acme", "eyJx.eyJy.sig")
        .await
        .is_none());
}
