//! #2617 — API keys are stored as HMAC-SHA256 and checked without argon2;
//! an old argon2 key verifies once and is upgraded in place.

#![cfg(all(feature = "tenancy", feature = "testkit"))]

use std::sync::atomic::{AtomicU64, Ordering};

use axum::http::{header, request::Parts, Request};
use rustango::core::Column as _;
use rustango::sql::{FetcherPool as _, Pool};
use rustango::tenancy::auth_backends::{
    create_api_key, set_api_key_secret, ApiKey, ApiKeyBackend, AuthBackend,
};
use rustango::tenancy::{TenantSlug, User};
use rustango::tri_dialect_test;

/// The throttle and the hash queue are process-global.
static SUITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static UNIQ: AtomicU64 = AtomicU64::new(0);
/// Keys MACed with a sub-key derived from the server secret.
const MAC: &str = "hmac-sha256v2$";

async fn setup(pool: &Pool) {
    set_api_key_secret(rustango::session::SessionSecret::from_bytes(vec![7; 32]));
    // Shared with other suites: create what is missing, never drop.
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
}

fn unique(pool: &Pool, prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}-{}",
        pool.dialect().name(),
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::SeqCst)
    )
}

fn parts(token: &str) -> Parts {
    let n = UNIQ.fetch_add(1, Ordering::SeqCst);
    let addr: std::net::SocketAddr = format!("10.62.{}.{}:1", (n >> 8) & 255, n & 255)
        .parse()
        .unwrap();
    parts_at(token, addr)
}

fn parts_at(token: &str, addr: std::net::SocketAddr) -> Parts {
    Request::builder()
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .extension(TenantSlug::new(1, "api-key-hmac"))
        .extension(axum::extract::ConnectInfo(addr))
        .body(())
        .unwrap()
        .into_parts()
        .0
}

async fn seed_user(pool: &Pool, name: &str) -> i64 {
    let mut u = User {
        username: name.to_owned(),
        ..rustango::testkit::user()
    };
    u.insert_pool(pool).await.expect("seed user");
    *u.id.get().unwrap()
}

async fn row(pool: &Pool, prefix: &str) -> ApiKey {
    ApiKey::objects()
        .where_(ApiKey::key_prefix.eq(prefix.to_owned()))
        .fetch(pool)
        .await
        .unwrap()
        .remove(0)
}

async fn ok(pool: &Pool, token: &str) -> bool {
    matches!(
        ApiKeyBackend.authenticate(&parts(token), pool).await,
        Ok(Some(_))
    )
}

/// A new key is an HMAC, checked with no hashing slot free.
async fn a_new_key_is_an_hmac_and_skips_argon2(pool: &Pool) {
    let _g = SUITE.lock().await;
    let uid = seed_user(pool, &unique(pool, "hmac")).await;
    let token = create_api_key(uid, "t", None, pool).await.unwrap();
    let (prefix, _) = token.split_once('.').unwrap();
    assert!(
        row(pool, prefix).await.key_hash.starts_with(MAC),
        "the key was not stored as an HMAC"
    );

    let held = rustango::passwords::hold_all_hash_slots().await;
    let hit = ok(pool, &token).await;
    let wrong = ok(pool, &format!("{prefix}.{}", "0".repeat(32))).await;
    drop(held);
    assert!(hit, "an HMAC key ran argon2");
    assert!(!wrong, "a wrong secret was accepted");
}

/// An argon2 key from before #2617 verifies once and is rewritten.
async fn a_legacy_argon2_key_is_upgraded_on_use(pool: &Pool) {
    let _g = SUITE.lock().await;
    let uid = seed_user(pool, &unique(pool, "legacy")).await;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let prefix = format!("{:08x}", nanos ^ UNIQ.fetch_add(1, Ordering::SeqCst) as u32);
    let secret = "0123456789abcdef0123456789abcdef";
    let argon = rustango::tenancy::password::hash(secret).unwrap();
    let mut key = ApiKey {
        id: Default::default(),
        user_id: uid,
        key_prefix: prefix.clone(),
        key_hash: argon.clone(),
        label: "legacy".into(),
        expires_at: None,
        created_at: Default::default(),
    };
    key.save_pool(pool).await.unwrap();
    let token = format!("{prefix}.{secret}");

    assert!(!ok(pool, &format!("{prefix}.wrong")).await);
    assert_eq!(
        row(pool, &prefix).await.key_hash,
        argon,
        "a wrong secret upgraded"
    );

    assert!(ok(pool, &token).await, "the legacy key was refused");
    assert!(
        row(pool, &prefix).await.key_hash.starts_with(MAC),
        "the legacy key was not upgraded"
    );
    let held = rustango::passwords::hold_all_hash_slots().await;
    let hit = ok(pool, &token).await;
    drop(held);
    assert!(hit, "the upgraded key ran argon2");
}

fn random_prefix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("{:08x}", nanos ^ UNIQ.fetch_add(1, Ordering::SeqCst) as u32)
}

/// A key MACed with the server secret itself still verifies, and is kept
/// as is so an older server still accepts it.
async fn a_key_maced_with_the_raw_secret_still_verifies(pool: &Pool) {
    let _g = SUITE.lock().await;
    let uid = seed_user(pool, &unique(pool, "rawmac")).await;
    let prefix = random_prefix();
    let secret = "fedcba9876543210fedcba9876543210";
    let raw = rustango::session::SessionSecret::from_bytes(vec![7; 32]);
    let msg = [b"rustango-apikey-v1.".as_slice(), secret.as_bytes()].concat();
    let hex: String = rustango::session::sign(&raw, &msg)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let old = format!("hmac-sha256${hex}");
    let mut key = ApiKey {
        id: Default::default(),
        user_id: uid,
        key_prefix: prefix.clone(),
        key_hash: old.clone(),
        label: "raw".into(),
        expires_at: None,
        created_at: Default::default(),
    };
    key.save_pool(pool).await.unwrap();

    assert!(!ok(pool, &format!("{prefix}.{}", "0".repeat(32))).await);
    let token = format!("{prefix}.{secret}");
    let held = rustango::passwords::hold_all_hash_slots().await;
    let hit = ok(pool, &token).await;
    drop(held);
    assert!(hit, "the raw-secret key was refused");
    assert_eq!(
        row(pool, &prefix).await.key_hash,
        old,
        "the raw-secret key was rewritten"
    );
}

/// tenancy-001 — more parallel calls from one IP than the per-IP limit
/// all pass with a valid key.
async fn parallel_calls_from_one_ip_all_pass(pool: &Pool) {
    let _g = SUITE.lock().await;
    let uid = seed_user(pool, &unique(pool, "parallel")).await;
    let token = create_api_key(uid, "t", None, pool).await.unwrap();
    let addr: std::net::SocketAddr = "10.63.0.1:1".parse().unwrap();
    let n = rustango::login_throttle::DEFAULT_IP_LIMIT as usize + 5;
    let runs = (0..n).map(|_| {
        let parts = parts_at(&token, addr);
        async move { ApiKeyBackend.authenticate(&parts, pool).await }
    });
    for r in futures_util::future::join_all(runs).await {
        assert!(matches!(r, Ok(Some(_))), "a valid key was refused: {r:?}");
    }
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        a_new_key_is_an_hmac_and_skips_argon2,
        a_legacy_argon2_key_is_upgraded_on_use,
        a_key_maced_with_the_raw_secret_still_verifies,
        parallel_calls_from_one_ip_all_pass,
    ],
}
