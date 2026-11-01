//! #2617 — a verified HTTP Basic credential skips argon2 for a short
//! while, but a password change or a deactivation is never masked.

#![cfg(all(feature = "tenancy", feature = "admin", feature = "testkit"))]

use std::sync::atomic::{AtomicU64, Ordering};

use axum::http::{header, request::Parts, Request};
use rustango::core::Column as _;
use rustango::sql::{FetcherPool as _, Pool};
use rustango::tenancy::auth_backends::{AuthBackend, ModelBackend};
use rustango::tenancy::{TenantSlug, User};
use rustango::tri_dialect_test;

/// The cache, the throttle and the hash queue are process-global.
static SUITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static UNIQ: AtomicU64 = AtomicU64::new(0);

const PASS: &str = "right-pass-123";

fn unique(pool: &Pool, prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}-{}",
        pool.dialect().name(),
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::SeqCst)
    )
}

async fn setup(pool: &Pool) {
    // Shared with other suites: create what is missing, never drop.
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
}

fn parts(user: &str, pass: &str) -> Parts {
    let n = UNIQ.fetch_add(1, Ordering::SeqCst);
    let addr: std::net::SocketAddr = format!("10.61.{}.{}:1", (n >> 8) & 255, n & 255)
        .parse()
        .unwrap();
    parts_at(user, pass, addr)
}

fn parts_at(user: &str, pass: &str, addr: std::net::SocketAddr) -> Parts {
    use base64::Engine as _;
    let creds = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
    Request::builder()
        .header(header::AUTHORIZATION, format!("Basic {creds}"))
        .extension(TenantSlug::new(1, "basic-cache"))
        .extension(axum::extract::ConnectInfo(addr))
        .body(())
        .unwrap()
        .into_parts()
        .0
}

async fn seed(pool: &Pool, name: &str) -> User {
    let mut u = User {
        username: name.to_owned(),
        password_hash: rustango::tenancy::password::hash(PASS).unwrap(),
        ..rustango::testkit::user()
    };
    u.insert_pool(pool).await.expect("seed user");
    u
}

async fn reload(pool: &Pool, name: &str) -> User {
    User::objects()
        .where_(User::username.eq(name.to_owned()))
        .fetch(pool)
        .await
        .unwrap()
        .remove(0)
}

async fn ok(pool: &Pool, user: &str, pass: &str) -> bool {
    matches!(
        ModelBackend.authenticate(&parts(user, pass), pool).await,
        Ok(Some(_))
    )
}

/// A hit needs no hashing slot; a new password is checked again.
async fn a_hit_skips_argon2_until_the_password_changes(pool: &Pool) {
    let _g = SUITE.lock().await;
    let name = unique(pool, "cache");
    seed(pool, &name).await;
    assert!(ok(pool, &name, PASS).await);

    let held = rustango::passwords::hold_all_hash_slots().await;
    let hit = ok(pool, &name, PASS).await;
    drop(held);
    assert!(hit, "a verified credential ran argon2 again");

    let mut u = reload(pool, &name).await;
    u.password_hash = rustango::tenancy::password::hash("new-pass-456").unwrap();
    u.save_pool(pool).await.unwrap();
    assert!(
        !ok(pool, &name, PASS).await,
        "the cache masked a password change"
    );
    assert!(ok(pool, &name, "new-pass-456").await);
}

async fn a_deactivation_is_not_masked(pool: &Pool) {
    let _g = SUITE.lock().await;
    let name = unique(pool, "inactive");
    seed(pool, &name).await;
    assert!(ok(pool, &name, PASS).await);
    let mut u = reload(pool, &name).await;
    u.active = false;
    u.save_pool(pool).await.unwrap();
    assert!(
        !ok(pool, &name, PASS).await,
        "the cache masked a deactivation"
    );
}

/// security-002 — more parallel cold logins than the lock allows all
/// pass with the right password.
async fn parallel_cold_logins_all_pass(pool: &Pool) {
    let _g = SUITE.lock().await;
    let name = unique(pool, "parallel");
    seed(pool, &name).await;
    let addr: std::net::SocketAddr = "10.60.0.1:1".parse().unwrap();
    let runs = (0..8).map(|_| {
        let parts = parts_at(&name, PASS, addr);
        async move { ModelBackend.authenticate(&parts, pool).await }
    });
    for r in futures_util::future::join_all(runs).await {
        assert!(
            matches!(r, Ok(Some(_))),
            "a right password was refused: {r:?}"
        );
    }
}

/// security-003 — a locked account is refused even on a cache hit.
async fn a_locked_account_is_refused_on_a_cache_hit(pool: &Pool) {
    let _g = SUITE.lock().await;
    let name = unique(pool, "locked");
    seed(pool, &name).await;
    assert!(ok(pool, &name, PASS).await);
    for _ in 0..rustango::account_lockout::DEFAULT_MAX_ATTEMPTS {
        assert!(!ok(pool, &name, "wrong-pass").await);
    }
    assert!(
        !ok(pool, &name, PASS).await,
        "a cached credential skipped the lock"
    );
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        a_hit_skips_argon2_until_the_password_changes,
        a_deactivation_is_not_masked,
        parallel_cold_logins_all_pass,
        a_locked_account_is_refused_on_a_cache_hit,
    ],
}
