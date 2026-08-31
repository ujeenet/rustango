#![cfg(feature = "cache")]
//! Unit tests for cache backends — no external services required.

use std::sync::Arc;
use std::time::Duration;

use rustango::cache::{
    get_json, get_or_set, set_json, BoxedCache, Cache, InMemoryCache, NullCache,
};

// ------------------------------------------------------------------ NullCache

#[tokio::test]
async fn null_cache_get_always_none() {
    let c = NullCache;
    assert!(c.get("key").await.unwrap().is_none());
}

#[tokio::test]
async fn null_cache_set_is_noop() {
    let c = NullCache;
    c.set("k", "v", None).await.unwrap();
    assert!(c.get("k").await.unwrap().is_none());
}

#[tokio::test]
async fn null_cache_exists_always_false() {
    let c = NullCache;
    c.set("k", "v", None).await.unwrap();
    assert!(!c.exists("k").await.unwrap());
}

// ------------------------------------------------------------------ InMemoryCache: basic

#[tokio::test]
async fn memory_cache_set_and_get() {
    let c = InMemoryCache::new();
    c.set("name", "alice", None).await.unwrap();
    assert_eq!(c.get("name").await.unwrap().as_deref(), Some("alice"));
}

#[tokio::test]
async fn memory_cache_get_missing_is_none() {
    let c = InMemoryCache::new();
    assert!(c.get("ghost").await.unwrap().is_none());
}

#[tokio::test]
async fn memory_cache_delete_removes_entry() {
    let c = InMemoryCache::new();
    c.set("k", "v", None).await.unwrap();
    c.delete("k").await.unwrap();
    assert!(c.get("k").await.unwrap().is_none());
}

#[tokio::test]
async fn memory_cache_delete_missing_is_noop() {
    let c = InMemoryCache::new();
    c.delete("nope").await.unwrap(); // must not panic
}

#[tokio::test]
async fn memory_cache_exists_true_when_set() {
    let c = InMemoryCache::new();
    c.set("present", "1", None).await.unwrap();
    assert!(c.exists("present").await.unwrap());
    assert!(!c.exists("absent").await.unwrap());
}

#[tokio::test]
async fn memory_cache_clear_removes_all() {
    let c = InMemoryCache::new();
    c.set("a", "1", None).await.unwrap();
    c.set("b", "2", None).await.unwrap();
    c.clear().await.unwrap();
    assert!(c.get("a").await.unwrap().is_none());
    assert!(c.get("b").await.unwrap().is_none());
}

#[tokio::test]
async fn memory_cache_overwrite_replaces_value() {
    let c = InMemoryCache::new();
    c.set("k", "v1", None).await.unwrap();
    c.set("k", "v2", None).await.unwrap();
    assert_eq!(c.get("k").await.unwrap().as_deref(), Some("v2"));
}

// ------------------------------------------------------------------ InMemoryCache: TTL

#[tokio::test]
async fn memory_cache_entry_expires_after_ttl() {
    let c = InMemoryCache::new();
    c.set("tmp", "value", Some(Duration::from_millis(10)))
        .await
        .unwrap();
    // Before expiry
    assert!(c.get("tmp").await.unwrap().is_some());
    tokio::time::sleep(Duration::from_millis(20)).await;
    // After expiry
    assert!(c.get("tmp").await.unwrap().is_none());
    assert!(!c.exists("tmp").await.unwrap());
}

#[tokio::test]
async fn memory_cache_with_default_ttl_applies_to_no_ttl_sets() {
    let c = InMemoryCache::with_default_ttl(Duration::from_millis(15));
    c.set("k", "v", None).await.unwrap(); // uses default TTL
    assert!(c.get("k").await.unwrap().is_some());
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(c.get("k").await.unwrap().is_none());
}

#[tokio::test]
async fn memory_cache_explicit_ttl_overrides_default() {
    let c = InMemoryCache::with_default_ttl(Duration::from_millis(50));
    // Explicit None-TTL-override: pass a longer explicit TTL
    c.set("k", "v", Some(Duration::from_secs(60)))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    // Still alive because explicit TTL is 60s
    assert!(c.get("k").await.unwrap().is_some());
}

// ------------------------------------------------------------------ InMemoryCache: thread safety

#[tokio::test]
async fn memory_cache_arc_shared_across_tasks() {
    let cache: Arc<InMemoryCache> = Arc::new(InMemoryCache::new());
    let c1 = cache.clone();
    let c2 = cache.clone();

    let t1 = tokio::spawn(async move {
        c1.set("shared", "hello", None).await.unwrap();
    });
    let t2 = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(5)).await;
        c2.get("shared").await.unwrap()
    });

    t1.await.unwrap();
    let val = t2.await.unwrap();
    assert_eq!(val.as_deref(), Some("hello"));
}

// ------------------------------------------------------------------ Typed helpers

#[tokio::test]
async fn set_json_and_get_json_roundtrip() {
    let c = InMemoryCache::new();
    let data = vec![1_i64, 2, 3];
    set_json(&c, "nums", &data, None).await.unwrap();
    let out: Option<Vec<i64>> = get_json(&c, "nums").await.unwrap();
    assert_eq!(out, Some(vec![1, 2, 3]));
}

#[tokio::test]
async fn get_json_missing_key_returns_none() {
    let c = InMemoryCache::new();
    let out: Option<String> = get_json(&c, "nope").await.unwrap();
    assert!(out.is_none());
}

#[tokio::test]
async fn get_or_set_computes_on_miss() {
    let c = InMemoryCache::new();
    let val: i64 = get_or_set(&c, "answer", || async { 42_i64 }, None)
        .await
        .unwrap();
    assert_eq!(val, 42);
    // Second call hits cache, factory not called again
    let val2: i64 = get_or_set(&c, "answer", || async { 99_i64 }, None)
        .await
        .unwrap();
    assert_eq!(val2, 42, "cache miss should not re-compute");
}

#[tokio::test]
async fn get_or_set_uses_cached_value_on_hit() {
    let c = InMemoryCache::new();
    c.set("x", "100", None).await.unwrap();
    let val: i64 = get_or_set(&c, "x", || async { 999_i64 }, None)
        .await
        .unwrap();
    assert_eq!(val, 100);
}

// ------------------------------------------------------------------ BoxedCache dyn dispatch

#[tokio::test]
async fn boxed_cache_in_memory_via_dyn() {
    let cache: BoxedCache = Arc::new(InMemoryCache::new());
    cache.set("dyn", "works", None).await.unwrap();
    assert_eq!(cache.get("dyn").await.unwrap().as_deref(), Some("works"));
}

#[tokio::test]
async fn boxed_cache_null_via_dyn() {
    let cache: BoxedCache = Arc::new(NullCache);
    cache.set("k", "v", None).await.unwrap();
    assert!(cache.get("k").await.unwrap().is_none());
}

// ------------------------------------------------------------------ Cache::incr default impl

#[tokio::test]
async fn incr_starts_at_by_when_missing() {
    let c = InMemoryCache::new();
    assert_eq!(c.incr("counter", 1, None).await.unwrap(), 1);
}

#[tokio::test]
async fn incr_increments_each_call() {
    let c = InMemoryCache::new();
    assert_eq!(c.incr("counter", 1, None).await.unwrap(), 1);
    assert_eq!(c.incr("counter", 1, None).await.unwrap(), 2);
    assert_eq!(c.incr("counter", 1, None).await.unwrap(), 3);
}

#[tokio::test]
async fn incr_supports_arbitrary_delta() {
    let c = InMemoryCache::new();
    assert_eq!(c.incr("counter", 5, None).await.unwrap(), 5);
    assert_eq!(c.incr("counter", 3, None).await.unwrap(), 8);
    assert_eq!(c.incr("counter", -2, None).await.unwrap(), 6);
}

#[tokio::test]
async fn incr_resets_on_non_integer_value() {
    let c = InMemoryCache::new();
    c.set("counter", "not-a-number", None).await.unwrap();
    // Default impl parses the existing value; non-int parses to 0, so the
    // next incr returns `by`.
    assert_eq!(c.incr("counter", 7, None).await.unwrap(), 7);
}

// ------------------------------------------------------------------ add (Django parity)

#[tokio::test]
async fn add_inserts_when_key_is_absent() {
    let c = InMemoryCache::new();
    let inserted = c.add("lock", "1", None).await.unwrap();
    assert!(inserted, "first add into empty cache should win the race");
    assert_eq!(c.get("lock").await.unwrap().as_deref(), Some("1"));
}

#[tokio::test]
async fn add_is_no_op_when_key_already_set() {
    let c = InMemoryCache::new();
    c.set("lock", "winner", None).await.unwrap();
    let inserted = c.add("lock", "loser", None).await.unwrap();
    assert!(!inserted, "second add must lose to the existing entry");
    assert_eq!(
        c.get("lock").await.unwrap().as_deref(),
        Some("winner"),
        "add must not overwrite the existing value"
    );
}

#[tokio::test]
async fn add_can_re_insert_after_expiry() {
    let c = InMemoryCache::new();
    c.set("lock", "v", Some(Duration::from_millis(50)))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    let inserted = c.add("lock", "fresh", None).await.unwrap();
    assert!(inserted, "expired entries don't block a subsequent add");
    assert_eq!(c.get("lock").await.unwrap().as_deref(), Some("fresh"));
}

// ------------------------------------------------------------------ touch (Django parity)

#[tokio::test]
async fn touch_extends_ttl_on_existing_key() {
    let c = InMemoryCache::new();
    c.set("k", "v", Some(Duration::from_millis(100)))
        .await
        .unwrap();
    // Touch with a longer TTL before the original expires.
    let touched = c.touch("k", Some(Duration::from_secs(3600))).await.unwrap();
    assert!(touched, "touch on a live key must return true");
    // After the original TTL would've fired the key still lives.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(c.get("k").await.unwrap().as_deref(), Some("v"));
}

#[tokio::test]
async fn touch_returns_false_when_key_absent() {
    let c = InMemoryCache::new();
    let touched = c
        .touch("ghost", Some(Duration::from_secs(60)))
        .await
        .unwrap();
    assert!(!touched);
}

#[tokio::test]
async fn touch_with_none_ttl_persists_indefinitely() {
    let c = InMemoryCache::new();
    c.set("k", "v", Some(Duration::from_millis(50)))
        .await
        .unwrap();
    let touched = c.touch("k", None).await.unwrap();
    assert!(touched);
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(
        c.get("k").await.unwrap().as_deref(),
        Some("v"),
        "touch(None) removes the TTL so the entry survives past the original expiry"
    );
}

#[tokio::test]
async fn touch_does_not_alter_value() {
    let c = InMemoryCache::new();
    c.set("k", "original", None).await.unwrap();
    c.touch("k", Some(Duration::from_secs(60))).await.unwrap();
    assert_eq!(c.get("k").await.unwrap().as_deref(), Some("original"));
}

// ------------------------------------------------------------------ get_many / set_many / delete_many (Django parity)

#[tokio::test]
async fn set_many_writes_every_entry() {
    let c = InMemoryCache::new();
    c.set_many(&[("a", "1"), ("b", "2"), ("c", "3")], None)
        .await
        .unwrap();
    assert_eq!(c.get("a").await.unwrap().as_deref(), Some("1"));
    assert_eq!(c.get("b").await.unwrap().as_deref(), Some("2"));
    assert_eq!(c.get("c").await.unwrap().as_deref(), Some("3"));
}

#[tokio::test]
async fn get_many_returns_only_present_keys() {
    let c = InMemoryCache::new();
    c.set("a", "1", None).await.unwrap();
    c.set("c", "3", None).await.unwrap();
    let map = c.get_many(&["a", "b", "c"]).await.unwrap();
    assert_eq!(map.len(), 2);
    assert_eq!(map.get("a").map(String::as_str), Some("1"));
    assert_eq!(map.get("c").map(String::as_str), Some("3"));
    assert!(!map.contains_key("b"));
}

#[tokio::test]
async fn get_many_with_no_keys_returns_empty() {
    let c = InMemoryCache::new();
    c.set("a", "1", None).await.unwrap();
    let map = c.get_many(&[]).await.unwrap();
    assert!(map.is_empty());
}

#[tokio::test]
async fn set_many_with_ttl_expires_every_entry() {
    let c = InMemoryCache::new();
    c.set_many(
        &[("k1", "v1"), ("k2", "v2")],
        Some(Duration::from_millis(50)),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(c.get("k1").await.unwrap().is_none());
    assert!(c.get("k2").await.unwrap().is_none());
}

#[tokio::test]
async fn delete_many_removes_every_listed_key() {
    let c = InMemoryCache::new();
    c.set_many(&[("a", "1"), ("b", "2"), ("c", "3")], None)
        .await
        .unwrap();
    c.delete_many(&["a", "c"]).await.unwrap();
    assert!(c.get("a").await.unwrap().is_none());
    assert_eq!(c.get("b").await.unwrap().as_deref(), Some("2"));
    assert!(c.get("c").await.unwrap().is_none());
}

#[tokio::test]
async fn delete_many_ignores_missing_keys() {
    let c = InMemoryCache::new();
    c.set("kept", "v", None).await.unwrap();
    // Mix of missing + present — neither should error.
    c.delete_many(&["ghost1", "kept", "ghost2"]).await.unwrap();
    assert!(c.get("kept").await.unwrap().is_none());
}

// ------------------------------------------------------------------ has_key + decr (Django parity)

#[tokio::test]
async fn has_key_returns_true_when_present() {
    // Django parity: `cache.has_key("k")` is the alias most users
    // translate from `if "k" in cache:` — should match `exists`.
    let c = InMemoryCache::new();
    c.set("present", "v", None).await.unwrap();
    assert!(c.has_key("present").await.unwrap());
}

#[tokio::test]
async fn has_key_returns_false_when_absent() {
    let c = InMemoryCache::new();
    assert!(!c.has_key("ghost").await.unwrap());
}

#[tokio::test]
async fn has_key_returns_false_after_expiry() {
    let c = InMemoryCache::new();
    c.set("ttl", "v", Some(Duration::from_millis(20)))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(!c.has_key("ttl").await.unwrap());
}

#[tokio::test]
async fn has_key_matches_exists_for_same_keys() {
    // Belt-and-suspenders: has_key should never disagree with exists.
    let c = InMemoryCache::new();
    c.set("yes", "v", None).await.unwrap();
    assert_eq!(
        c.has_key("yes").await.unwrap(),
        c.exists("yes").await.unwrap(),
    );
    assert_eq!(
        c.has_key("no").await.unwrap(),
        c.exists("no").await.unwrap(),
    );
}

#[tokio::test]
async fn decr_decrements_existing_counter() {
    let c = InMemoryCache::new();
    c.set("counter", "10", None).await.unwrap();
    let new = c.decr("counter", 3, None).await.unwrap();
    assert_eq!(new, 7);
    assert_eq!(c.get("counter").await.unwrap().as_deref(), Some("7"));
}

#[tokio::test]
async fn decr_underflows_to_negative_when_unbounded() {
    // Django semantics: decr doesn't clamp at zero — returns whatever
    // arithmetic yields. Apps that want a clamp do it client-side.
    let c = InMemoryCache::new();
    c.set("counter", "2", None).await.unwrap();
    let new = c.decr("counter", 5, None).await.unwrap();
    assert_eq!(new, -3);
}

#[tokio::test]
async fn decr_treats_missing_key_as_zero() {
    // Matches incr's default-impl behavior: missing → start from 0.
    let c = InMemoryCache::new();
    let new = c.decr("ghost", 4, None).await.unwrap();
    assert_eq!(new, -4);
}

#[tokio::test]
async fn decr_is_inverse_of_incr() {
    let c = InMemoryCache::new();
    c.incr("k", 7, None).await.unwrap();
    let after = c.decr("k", 7, None).await.unwrap();
    assert_eq!(after, 0);
}

// ------------------------------------------------------------------ get_or (Django parity)

#[tokio::test]
async fn get_or_returns_stored_value_when_present() {
    // Django parity: `cache.get('k', default='x')` returns the value
    // when the key exists, not the default.
    let c = InMemoryCache::new();
    c.set("greeting", "hello", None).await.unwrap();
    assert_eq!(c.get_or("greeting", "ANON").await.unwrap(), "hello");
}

#[tokio::test]
async fn get_or_returns_default_when_absent() {
    let c = InMemoryCache::new();
    assert_eq!(c.get_or("missing", "fallback").await.unwrap(), "fallback");
}

#[tokio::test]
async fn get_or_returns_default_after_expiry() {
    let c = InMemoryCache::new();
    c.set("ttl", "fresh", Some(Duration::from_millis(20)))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(c.get_or("ttl", "default").await.unwrap(), "default");
}

#[tokio::test]
async fn get_or_does_not_write_default_back() {
    // Subtle: Django's two-arg get just returns the default — it does
    // NOT store it. (get_or_set is the "fetch-or-compute-and-store"
    // shape; get_or is purely a read fallback.)
    let c = InMemoryCache::new();
    let _ = c.get_or("ghost", "would-not-be-stored").await.unwrap();
    assert!(!c.has_key("ghost").await.unwrap());
}

/// #1253 — `InMemoryCache::incr` must be atomic under concurrency.
/// The trait default is get-parse-set, which loses updates; the counters
/// built on it (lockout, distributed lock, rate limit) rely on this.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inmemory_incr_is_atomic_under_contention() {
    use std::sync::Arc;
    let cache: Arc<rustango::cache::InMemoryCache> =
        Arc::new(rustango::cache::InMemoryCache::new());
    let mut handles = Vec::new();
    for _ in 0..100 {
        let c = cache.clone();
        handles.push(tokio::spawn(async move {
            rustango::cache::Cache::incr(&*c, "counter", 1, None)
                .await
                .unwrap()
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let final_val = rustango::cache::Cache::get(&*cache, "counter")
        .await
        .unwrap()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap();
    assert_eq!(final_val, 100, "incr lost updates under contention");
}
