//! `RedisCache::delete_prefix` against a live Redis (#1227).
//!
//! This file exists because of a bug that shipped in review: `Cache`
//! grew a `delete_prefix` whose default clears the WHOLE cache (safe on
//! a backend that cannot enumerate its keys — under-deleting would let
//! another namespace read a stale entry). `RedisCache` did not override
//! it, and `RedisCache::clear()` is **`FLUSHDB`** — so one tenant's
//! `ScopedCache::clear()` would have wiped every other tenant's
//! entries, every rate-limit counter, and every `lock:*` key, letting
//! two replicas both take a "once per cluster" lock.
//!
//! Nothing caught it: the crate had no Redis tests and CI runs no Redis
//! service, because `cache-redis` is off by default.
//!
//! Reads `REDIS_TEST_URL`; skips silently when unset:
//!
//! ```bash
//! docker run -d -p 6399:6379 redis:7-alpine
//! REDIS_TEST_URL=redis://127.0.0.1:6399/ \
//!   cargo test -p rustango --features cache,cache-redis --test cache_redis_prefix_live
//! ```

#![cfg(feature = "cache-redis")]

use std::sync::Arc;

use rustango::cache::redis_backend::RedisCache;
use rustango::cache::{BoxedCache, Cache, ScopedCache};
use tokio::sync::Mutex;

/// Suite-wide lock. Every test here starts from an empty keyspace, and
/// the reset is `FLUSHDB` — which is global. Without serializing, one
/// test's setup wipes another's data mid-run and they fail in exactly
/// the way a real prefix-delete bug would, which is worse than useless.
fn live_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn cache() -> Option<RedisCache> {
    let url = std::env::var("REDIS_TEST_URL").ok()?;
    Some(RedisCache::new(&url).await.expect("connect REDIS_TEST_URL"))
}

/// The regression: a scoped clear must delete only its own namespace.
/// Before the `SCAN`-based override this flushed the entire database.
#[tokio::test]
async fn scoped_clear_does_not_flush_other_namespaces() {
    let _g = live_lock().lock().await;
    let Some(redis) = cache().await else {
        return;
    };
    redis.clear().await.expect("start from an empty db");

    let shared: BoxedCache = Arc::new(redis);
    let acme = ScopedCache::for_tenant(shared.clone(), "acme");
    let globex = ScopedCache::for_tenant(shared.clone(), "globex");

    acme.set("stats", "acme-value", None).await.unwrap();
    globex.set("stats", "globex-value", None).await.unwrap();
    // Unscoped keys standing in for the collateral damage FLUSHDB did:
    // a distributed lock and a rate-limit counter.
    shared
        .set("lock:nightly_prune", "held", None)
        .await
        .unwrap();
    shared.set("ratelimit:1.2.3.4", "7", None).await.unwrap();

    acme.clear().await.expect("scoped clear");

    assert_eq!(
        acme.get("stats").await.unwrap(),
        None,
        "acme's own key goes"
    );
    assert_eq!(
        globex.get("stats").await.unwrap().as_deref(),
        Some("globex-value"),
        "another tenant's entry must survive — this is the FLUSHDB regression"
    );
    assert_eq!(
        shared.get("lock:nightly_prune").await.unwrap().as_deref(),
        Some("held"),
        "a distributed lock must survive a tenant's cache invalidation"
    );
    assert_eq!(
        shared.get("ratelimit:1.2.3.4").await.unwrap().as_deref(),
        Some("7"),
        "rate-limit counters must survive a tenant's cache invalidation"
    );

    shared.clear().await.ok();
}

/// The cursor loop must delete every match, not just the first `COUNT`
/// batch — `SCAN` returns an arbitrary number of keys per call and a
/// non-zero cursor means "keep going".
#[tokio::test]
async fn delete_prefix_sweeps_past_one_scan_batch() {
    let _g = live_lock().lock().await;
    let Some(redis) = cache().await else {
        return;
    };
    redis.clear().await.expect("start from an empty db");

    // Comfortably more than the COUNT hint (512) so the loop must
    // iterate, plus a decoy namespace that must be untouched.
    for i in 0..1500 {
        redis
            .set(&format!("tenant:bulk:{i}"), "x", None)
            .await
            .unwrap();
    }
    redis.set("tenant:keep:0", "y", None).await.unwrap();

    redis.delete_prefix("tenant:bulk:").await.expect("sweep");

    for i in [0, 500, 999, 1499] {
        assert_eq!(
            redis.get(&format!("tenant:bulk:{i}")).await.unwrap(),
            None,
            "key {i} survived the sweep — the cursor loop stopped early"
        );
    }
    assert_eq!(
        redis.get("tenant:keep:0").await.unwrap().as_deref(),
        Some("y")
    );

    redis.clear().await.ok();
}

/// `MATCH` takes a glob. An unescaped `*` or `?` in the prefix would
/// match beyond its own namespace.
#[tokio::test]
async fn delete_prefix_escapes_glob_metacharacters() {
    let _g = live_lock().lock().await;
    let Some(redis) = cache().await else {
        return;
    };
    redis.clear().await.expect("start from an empty db");

    redis.set("a*b:key", "literal-star", None).await.unwrap();
    redis.set("axxb:key", "would-match", None).await.unwrap();
    redis.set("q?:key", "literal-question", None).await.unwrap();
    redis.set("qZ:key", "would-match", None).await.unwrap();

    redis.delete_prefix("a*b:").await.unwrap();
    assert_eq!(redis.get("a*b:key").await.unwrap(), None);
    assert_eq!(
        redis.get("axxb:key").await.unwrap().as_deref(),
        Some("would-match"),
        "`*` must be escaped, not treated as a glob wildcard"
    );

    redis.delete_prefix("q?:").await.unwrap();
    assert_eq!(redis.get("q?:key").await.unwrap(), None);
    assert_eq!(
        redis.get("qZ:key").await.unwrap().as_deref(),
        Some("would-match"),
        "`?` must be escaped, not treated as a single-character wildcard"
    );

    redis.clear().await.ok();
}

/// An empty keyspace must not error, and must not fall back to a flush.
#[tokio::test]
async fn delete_prefix_on_no_matches_is_a_noop() {
    let _g = live_lock().lock().await;
    let Some(redis) = cache().await else {
        return;
    };
    redis.clear().await.expect("start from an empty db");

    redis.set("untouched", "v", None).await.unwrap();
    redis.delete_prefix("nothing-here:").await.expect("noop");
    assert_eq!(redis.get("untouched").await.unwrap().as_deref(), Some("v"));

    redis.clear().await.ok();
}
