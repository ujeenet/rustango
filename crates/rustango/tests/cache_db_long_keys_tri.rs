//! `DatabaseCache` keys longer than MySQL's `VARCHAR(255)` round-trip
//! and stay distinct, on every dialect (#1674), and compare exactly (#1757).

#![cfg(all(
    feature = "cache",
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]

use std::time::Duration;

use rustango::cache::{Cache, DatabaseCache};
use rustango::sql::Pool;
use rustango::tri_dialect_test;

async fn noop(_: &Pool) {}

/// A fresh cache on `table`. Each scenario names its own, so parallel
/// runs on one live database never drop each other's table (#1945).
async fn fresh(pool: &Pool, table: &str) -> DatabaseCache {
    let cache = DatabaseCache::new(pool.clone(), table);
    let _ = cache.drop_table().await;
    cache.ensure_table().await.expect("ensure_table");
    cache
}

/// A long key round-trips through add/get/delete/set/touch/incr, and a
/// key sharing its first 255 bytes stays a different key.
async fn long_keys_round_trip(pool: &Pool) {
    let cache = fresh(pool, "rustango_cache_long_rt").await;
    let shared = "k".repeat(255);
    let a = format!("{shared}-alpha-{}", "a".repeat(40));
    let b = format!("{shared}-beta-{}", "b".repeat(40));

    assert!(cache.add(&a, "token-a", None).await.unwrap(), "add");
    assert_eq!(cache.get(&a).await.unwrap().as_deref(), Some("token-a"));
    assert!(
        cache.add(&b, "token-b", None).await.unwrap(),
        "shared prefix"
    );
    assert_eq!(cache.get(&b).await.unwrap().as_deref(), Some("token-b"));
    assert_eq!(cache.get(&a).await.unwrap().as_deref(), Some("token-a"));

    cache.delete(&a).await.unwrap();
    assert_eq!(cache.get(&a).await.unwrap(), None, "delete");
    assert_eq!(cache.get(&b).await.unwrap().as_deref(), Some("token-b"));

    cache
        .set(&a, "v", Some(Duration::from_secs(60)))
        .await
        .unwrap();
    assert_eq!(cache.get(&a).await.unwrap().as_deref(), Some("v"));
    assert!(cache.touch(&a, None).await.unwrap(), "touch");
    let c = format!("{shared}-counter");
    assert_eq!(cache.incr(&c, 2, None).await.unwrap(), 2);
    assert_eq!(cache.incr(&c, 3, None).await.unwrap(), 5);

    // Short keys keep their stored form, so existing rows still hit.
    cache.set("short", "s", None).await.unwrap();
    assert_eq!(cache.get("short").await.unwrap().as_deref(), Some("s"));
    let _ = cache.drop_table().await;
}

/// A prefix clear reaches long keys under that prefix, and only those.
async fn prefix_delete_reaches_long_keys(pool: &Pool) {
    let cache = fresh(pool, "rustango_cache_long_prefix").await;
    let long = "k".repeat(255);
    let acme = format!("tenant:acme:{long}");
    cache.set(&acme, "x", None).await.unwrap();
    cache.set("tenant:globex:x", "y", None).await.unwrap();
    cache.delete_prefix("tenant:acme:").await.unwrap();
    assert_eq!(cache.get(&acme).await.unwrap(), None);
    assert_eq!(
        cache.get("tenant:globex:x").await.unwrap().as_deref(),
        Some("y")
    );

    // A prefix longer than the 190-byte head still removes its keys.
    let deep = format!("tenant:acme:{}", "d".repeat(200));
    let key = format!("{deep}:{long}");
    cache.set(&key, "z", None).await.unwrap();
    cache.set("tenant:globex:x", "y", None).await.unwrap();
    cache.delete_prefix(&deep).await.unwrap();
    assert_eq!(cache.get(&key).await.unwrap(), None, "long prefix");
    assert_eq!(
        cache.get("tenant:globex:x").await.unwrap().as_deref(),
        Some("y")
    );
    let _ = cache.drop_table().await;
}

/// Keys compare byte for byte: case and accents are not folded, and a
/// prefix delete stays inside its exact namespace (#1757).
async fn keys_compare_exactly(pool: &Pool) {
    let cache = fresh(pool, "rustango_cache_long_exact").await;
    cache.set("User:1", "upper", None).await.unwrap();
    assert_eq!(cache.get("user:1").await.unwrap(), None, "case folded");
    cache.set("user:1", "lower", None).await.unwrap();
    assert_eq!(cache.get("User:1").await.unwrap().as_deref(), Some("upper"));
    assert_eq!(cache.get("user:1").await.unwrap().as_deref(), Some("lower"));

    cache.set("cafe", "plain", None).await.unwrap();
    assert_eq!(cache.get("café").await.unwrap(), None, "accent folded");
    assert!(cache.add("café", "accent", None).await.unwrap(), "add café");
    assert_eq!(cache.get("cafe").await.unwrap().as_deref(), Some("plain"));

    cache.set("tenant:acme:x", "a", None).await.unwrap();
    cache.set("tenant:ACME:x", "b", None).await.unwrap();
    cache.set("tenant:acmé:x", "c", None).await.unwrap();
    cache.delete_prefix("tenant:acme:").await.unwrap();
    assert_eq!(cache.get("tenant:acme:x").await.unwrap(), None);
    assert_eq!(
        cache.get("tenant:ACME:x").await.unwrap().as_deref(),
        Some("b"),
        "prefix delete folded case"
    );
    assert_eq!(
        cache.get("tenant:acmé:x").await.unwrap().as_deref(),
        Some("c"),
        "prefix delete folded accents"
    );
    let _ = cache.drop_table().await;
}

tri_dialect_test! {
    setup: noop,
    scenarios: [long_keys_round_trip, prefix_delete_reaches_long_keys, keys_compare_exactly],
}
