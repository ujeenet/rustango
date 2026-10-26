//! Login per-IP and global limits on a database cache are shared by every
//! replica on it (#1809).

#![cfg(all(
    feature = "admin",
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]

use std::sync::Arc;

use rustango::cache::{BoxedCache, DatabaseCache};
use rustango::login_throttle::{ClientIp, LoginLimits, LoginScope, LoginThrottle};
use rustango::sql::Pool;
use rustango::tri_dialect_test;

async fn noop(_: &Pool) {}

async fn fresh(pool: &Pool, table: &str) -> DatabaseCache {
    let cache = DatabaseCache::new(pool.clone(), table);
    let _ = cache.drop_table().await;
    cache.ensure_table().await.expect("ensure_table");
    cache
}

fn ip(s: &str) -> ClientIp {
    let mut ext = axum::http::Extensions::new();
    let addr: std::net::SocketAddr = format!("{s}:1").parse().unwrap();
    ext.insert(axum::extract::ConnectInfo(addr));
    ClientIp::from_parts(&ext, &axum::http::HeaderMap::new())
}

async fn replicas_share_the_limits(pool: &Pool) {
    let db = fresh(pool, "rustango_login_throttle_tri").await;
    let cache: BoxedCache = Arc::new(db);
    let limits = LoginLimits {
        ip_limit: 2,
        global_limit: 3,
        ..LoginLimits::default()
    };
    let a = LoginThrottle::with_cache(limits, cache.clone());
    let b = LoginThrottle::with_cache(limits, cache.clone());
    let s = LoginScope::Tenant("tri".into());
    let one = ip("10.7.0.1");
    a.begin(&s, &one, "u").await.unwrap().failed().await;
    b.begin(&s, &one, "v").await.unwrap().failed().await;
    assert!(a.begin(&s, &one, "w").await.is_err(), "per-IP not shared");
    // A success gives its token back on the shared counter.
    let two = ip("10.7.0.2");
    b.begin(&s, &two, "x").await.unwrap().succeeded().await;
    b.begin(&s, &two, "x").await.unwrap().failed().await;
    assert!(
        a.begin(&s, &ip("10.7.0.3"), "y").await.is_err(),
        "global not shared"
    );
    assert!(!a.is_process_local());
    let _ = DatabaseCache::new(pool.clone(), "rustango_login_throttle_tri")
        .drop_table()
        .await;
}

/// A sign-in stored on the database cache lets that user and IP past a
/// spent global limit, on another replica too.
async fn a_known_client_passes_a_spent_global_limit(pool: &Pool) {
    let table = "rustango_login_known_tri";
    let cache: BoxedCache = Arc::new(fresh(pool, table).await);
    let limits = LoginLimits {
        global_limit: 1,
        ..LoginLimits::default()
    };
    let a = LoginThrottle::with_cache(limits, cache.clone());
    let b = LoginThrottle::with_cache(limits, cache.clone());
    let s = LoginScope::Tenant("tri-known".into());
    let home = ip("10.7.1.1");
    a.begin(&s, &home, "alice").await.unwrap().succeeded().await;
    a.begin(&s, &ip("10.7.1.2"), "x")
        .await
        .unwrap()
        .failed()
        .await;
    assert!(
        b.begin(&s, &ip("10.7.1.3"), "y").await.is_err(),
        "global open"
    );
    let r = b.begin(&s, &home, "alice").await;
    r.expect("a known client was locked out").succeeded().await;
    let _ = DatabaseCache::new(pool.clone(), table).drop_table().await;
}

tri_dialect_test! {
    setup: noop,
    scenarios: [replicas_share_the_limits, a_known_client_passes_a_spent_global_limit],
}
