//! `DatabaseCache::incr` is one atomic upsert on every dialect (#1871):
//! parallel increments are never lost, and the TTL is set only on insert.
//! Also `purge_expired` batching (#1906).

#![cfg(all(
    feature = "cache",
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use rustango::cache::db_backend::PURGE_BATCH;
use rustango::cache::{Cache, DatabaseCache};
use rustango::core::SqlValue;
use rustango::sql::{raw_query_pool, Pool};
use rustango::test_assertions::QueryCounter;
use rustango::tri_dialect_test;

async fn noop(_: &Pool) {}

/// One table per scenario: nextest runs them in parallel.
async fn fresh(pool: &Pool, table: &str) -> DatabaseCache {
    let cache = DatabaseCache::new(pool.clone(), table);
    let _ = cache.drop_table().await;
    cache.ensure_table().await.expect("ensure_table");
    cache
}

/// Parallel failed logins must each count: every caller sees its own value.
async fn parallel_incr_loses_nothing(pool: &Pool) {
    let cache = Arc::new(fresh(pool, "rustango_cache_incr_par").await);
    const N: i64 = 32;
    let mut handles = Vec::new();
    for _ in 0..N {
        let c = Arc::clone(&cache);
        handles.push(tokio::spawn(async move {
            c.incr("lockout:uid:1", 1, Some(Duration::from_secs(60)))
                .await
                .expect("incr")
        }));
    }
    let mut seen = BTreeSet::new();
    for h in handles {
        seen.insert(h.await.unwrap());
    }
    assert_eq!(
        cache.get("lockout:uid:1").await.unwrap().as_deref(),
        Some(N.to_string().as_str()),
        "lost updates"
    );
    assert_eq!(seen, (1..=N).collect::<BTreeSet<_>>(), "duplicate returns");
    let _ = cache.drop_table().await;
}

/// The TTL is set when the counter is created, not moved by later calls;
/// an expired counter starts again, and a non-integer value counts as 0.
async fn incr_ttl_and_reset(pool: &Pool) {
    let cache = fresh(pool, "rustango_cache_incr_ttl").await;
    let short = Some(Duration::from_millis(300));
    assert_eq!(cache.incr("w", 2, short).await.unwrap(), 2);
    assert_eq!(
        cache
            .incr("w", 3, Some(Duration::from_secs(60)))
            .await
            .unwrap(),
        5
    );
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(cache.get("w").await.unwrap(), None, "TTL was extended");
    assert_eq!(cache.incr("w", 4, short).await.unwrap(), 4, "expired reset");

    assert_eq!(cache.incr("forever", -3, None).await.unwrap(), -3);
    assert_eq!(cache.incr("forever", 1, None).await.unwrap(), -2);

    cache.set("junk", "abc", None).await.unwrap();
    assert_eq!(cache.incr("junk", 2, None).await.unwrap(), 2);
    let _ = cache.drop_table().await;
}

/// An i64 overflow is an error on every dialect and leaves the counter alone.
async fn incr_overflow_is_an_error(pool: &Pool) {
    let cache = fresh(pool, "rustango_cache_incr_ovf").await;
    let big = "999999999999999999";
    cache.set("n", big, None).await.unwrap();
    let err = cache.incr("n", i64::MAX, None).await.unwrap_err();
    assert!(err.to_string().contains("out of range"), "{err}");
    assert_eq!(cache.get("n").await.unwrap().as_deref(), Some(big));
    let _ = cache.drop_table().await;
}

/// `purge_expired` deletes in bounded batches, keeps live rows, and
/// `ensure_table` adds the `expires` index it scans by (#1906).
async fn purge_is_batched_and_indexed(pool: &Pool) {
    let table = "rustango_cache_purge";
    let cache = fresh(pool, table).await;
    cache
        .ensure_table()
        .await
        .expect("ensure_table is idempotent");
    let batch = usize::try_from(PURGE_BATCH).unwrap();
    for i in 0..2 * batch + 5 {
        let ttl = Some(Duration::from_millis(1));
        cache.set(&format!("old{i}"), "v", ttl).await.unwrap();
    }
    cache.set("forever", "v", None).await.unwrap();
    let live = Some(Duration::from_secs(60));
    cache.set("live", "v", live).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let (purged, statements) = QueryCounter::scope(async {
        let n = cache.purge_expired().await.unwrap();
        (n, QueryCounter::current())
    })
    .await;
    assert_eq!(purged, 2 * PURGE_BATCH + 5);
    assert_eq!(statements, 3, "one DELETE per batch");
    assert!(cache.exists("forever").await.unwrap());
    assert!(cache.exists("live").await.unwrap());

    let index = format!("{table}_expires_idx");
    let sql = match pool.dialect().name() {
        "postgres" => "SELECT COUNT(*) FROM pg_indexes WHERE indexname = $1",
        "mysql" => {
            "SELECT COUNT(*) FROM information_schema.statistics \
             WHERE table_schema = DATABASE() AND index_name = ?"
        }
        _ => "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?",
    };
    let rows: Vec<(i64,)> = raw_query_pool(sql, vec![SqlValue::String(index)], pool)
        .await
        .unwrap();
    assert_eq!(rows[0].0, 1, "expires index");
    let _ = cache.drop_table().await;
}

tri_dialect_test! {
    setup: noop,
    scenarios: [
        parallel_incr_loses_nothing,
        incr_ttl_and_reset,
        incr_overflow_is_an_error,
        purge_is_batched_and_indexed,
    ],
}
