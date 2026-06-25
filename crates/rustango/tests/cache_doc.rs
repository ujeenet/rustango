//! Backing test for `docs/caching.md` — the `Cache` trait, `get_or_set`, typed
//! JSON helpers, and TTL expiry, all on the dependency-free `InMemoryCache`.
//! (The Redis and DB backends share this exact trait surface.)
//!
//! Run: `cargo test -p rustango --test cache_doc`

#![cfg(feature = "cache")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rustango::cache::{get_json, get_or_set, set_json, Cache, InMemoryCache};
use serde::{Deserialize, Serialize};

#[tokio::test]
async fn get_set_exists_delete_roundtrip() {
    let cache = InMemoryCache::new();

    assert_eq!(cache.get("greeting").await.unwrap(), None); // miss
    cache.set("greeting", "hello", None).await.unwrap(); // no TTL = no expiry
    assert_eq!(
        cache.get("greeting").await.unwrap().as_deref(),
        Some("hello")
    );
    assert!(cache.exists("greeting").await.unwrap());

    cache.delete("greeting").await.unwrap();
    assert_eq!(cache.get("greeting").await.unwrap(), None);
    assert!(!cache.exists("greeting").await.unwrap());
}

#[tokio::test]
async fn get_or_set_only_computes_on_a_miss() {
    let cache = InMemoryCache::new();
    let calls = Arc::new(AtomicUsize::new(0));

    // First call: cache miss → the factory runs and the value is stored.
    let c1 = calls.clone();
    let a: i64 = get_or_set(
        &cache,
        "home:stats",
        || async move {
            c1.fetch_add(1, Ordering::SeqCst);
            42
        },
        Some(Duration::from_secs(60)),
    )
    .await
    .unwrap();

    // Second call: cache hit → the factory does NOT run.
    let c2 = calls.clone();
    let b: i64 = get_or_set(
        &cache,
        "home:stats",
        || async move {
            c2.fetch_add(1, Ordering::SeqCst);
            999 // never reached
        },
        Some(Duration::from_secs(60)),
    )
    .await
    .unwrap();

    assert_eq!(a, 42);
    assert_eq!(b, 42, "served from cache, not recomputed");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "factory ran exactly once");
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Profile {
    id: i64,
    name: String,
}

#[tokio::test]
async fn typed_json_helpers_roundtrip() {
    let cache = InMemoryCache::new();
    let p = Profile {
        id: 7,
        name: "Ada".into(),
    };

    set_json(&cache, "profile:7", &p, None).await.unwrap();
    let back: Option<Profile> = get_json(&cache, "profile:7").await.unwrap();
    assert_eq!(back, Some(p));

    let missing: Option<Profile> = get_json(&cache, "profile:404").await.unwrap();
    assert_eq!(missing, None);
}

#[tokio::test]
async fn entries_expire_after_their_ttl() {
    let cache = InMemoryCache::new();
    cache
        .set("flash", "x", Some(Duration::from_millis(50)))
        .await
        .unwrap();
    assert_eq!(cache.get("flash").await.unwrap().as_deref(), Some("x"));

    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(cache.get("flash").await.unwrap(), None, "expired after TTL");
}

#[tokio::test]
async fn boxed_cache_is_swappable() {
    // App code holds a trait object, so the backend swaps without touching
    // call sites (InMemory in tests, Redis/DB in prod).
    let cache: rustango::cache::BoxedCache = Arc::new(InMemoryCache::new());
    cache.set("k", "v", None).await.unwrap();
    assert_eq!(cache.get("k").await.unwrap().as_deref(), Some("v"));
}
