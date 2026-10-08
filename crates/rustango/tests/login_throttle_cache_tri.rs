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

tri_dialect_test! {
    setup: noop,
    scenarios: [replicas_share_the_limits],
}
