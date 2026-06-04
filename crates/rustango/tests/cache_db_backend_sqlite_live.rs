//! Django-parity #409 — `DatabaseCache` against a live sqlite pool.
//! Round-trip, lazy GC on TTL expiry, delete, clear, ensure_table
//! idempotency. The DDL emit + upsert SQL is dialect-rendered, so
//! sqlite hits the `ON CONFLICT` arm; the MySQL `ON DUPLICATE KEY
//! UPDATE` arm is covered by inspection (see `db_backend.rs::set`).

#![cfg(feature = "sqlite")]

use std::time::Duration;

use rustango::cache::{Cache, DatabaseCache};
use rustango::sql::{sqlx, Pool};

async fn fresh_pool() -> Pool {
    let pool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    Pool::Sqlite(pool)
}

#[tokio::test]
async fn ensure_table_is_idempotent_and_round_trip_works() {
    let pool = fresh_pool().await;
    let cache = DatabaseCache::new(pool, "rustango_cache");

    // First call creates; second call is a no-op (IF NOT EXISTS).
    cache.ensure_table().await.expect("create #1");
    cache.ensure_table().await.expect("create #2");

    cache.set("greeting", "hello", None).await.expect("set");
    assert_eq!(
        cache.get("greeting").await.unwrap().as_deref(),
        Some("hello"),
    );
    assert!(cache.exists("greeting").await.unwrap());
}

#[tokio::test]
async fn set_overwrites_existing_value() {
    let pool = fresh_pool().await;
    let cache = DatabaseCache::new(pool, "rustango_cache_ow");
    cache.ensure_table().await.unwrap();

    cache.set("k", "v1", None).await.unwrap();
    cache.set("k", "v2", None).await.unwrap();
    assert_eq!(cache.get("k").await.unwrap().as_deref(), Some("v2"));
}

#[tokio::test]
async fn ttl_purges_lazily_on_read() {
    let pool = fresh_pool().await;
    let cache = DatabaseCache::new(pool, "rustango_cache_ttl");
    cache.ensure_table().await.unwrap();

    // Use a 3-second TTL with a 3.2-second sleep below. Earlier
    // 1-second TTL was racing under CI load: the SQLite TIMESTAMP
    // column stores integer-second precision, so a set at
    // T=HH:MM:SS.999 followed by a slow `get()` could land at T+1
    // and read None instead of Some("v"). 3-second budget gives
    // ~3000ms of slack before the second-boundary tick matters.
    cache
        .set("k", "v", Some(Duration::from_secs(3)))
        .await
        .unwrap();
    // Still present immediately.
    assert_eq!(cache.get("k").await.unwrap().as_deref(), Some("v"));
    // Wait past TTL — sqlite has 1-second granularity in the schema's BIGINT.
    tokio::time::sleep(Duration::from_millis(3_200)).await;
    assert!(cache.get("k").await.unwrap().is_none());
    assert!(!cache.exists("k").await.unwrap());
}

#[tokio::test]
async fn delete_removes_the_row() {
    let pool = fresh_pool().await;
    let cache = DatabaseCache::new(pool, "rustango_cache_del");
    cache.ensure_table().await.unwrap();

    cache.set("k", "v", None).await.unwrap();
    cache.delete("k").await.unwrap();
    assert!(!cache.exists("k").await.unwrap());
    assert_eq!(cache.get("k").await.unwrap(), None);
}

#[tokio::test]
async fn clear_drops_every_entry() {
    let pool = fresh_pool().await;
    let cache = DatabaseCache::new(pool, "rustango_cache_clr");
    cache.ensure_table().await.unwrap();

    for i in 0..5 {
        cache.set(&format!("k{i}"), "v", None).await.unwrap();
    }
    cache.clear().await.unwrap();
    for i in 0..5 {
        assert!(cache.get(&format!("k{i}")).await.unwrap().is_none());
    }
}

#[tokio::test]
async fn incr_uses_default_get_set_path() {
    // Cache trait provides a default non-atomic `incr` over get/set —
    // DatabaseCache inherits it. Verify it round-trips via the table.
    let pool = fresh_pool().await;
    let cache = DatabaseCache::new(pool, "rustango_cache_inc");
    cache.ensure_table().await.unwrap();

    assert_eq!(cache.incr("counter", 1, None).await.unwrap(), 1);
    assert_eq!(cache.incr("counter", 4, None).await.unwrap(), 5);
    assert_eq!(cache.incr("counter", -2, None).await.unwrap(), 3);
}

#[tokio::test]
async fn drop_table_after_use() {
    let pool = fresh_pool().await;
    let cache = DatabaseCache::new(pool, "rustango_cache_drop");
    cache.ensure_table().await.unwrap();
    cache.set("k", "v", None).await.unwrap();

    cache.drop_table().await.unwrap();
    // After drop the next read errors at the SQL layer (table is gone),
    // but `get` maps that to `CacheError::Connection` — verify it returns
    // an Err rather than silently producing None.
    let res = cache.get("k").await;
    assert!(res.is_err(), "expected error after drop_table");
}
