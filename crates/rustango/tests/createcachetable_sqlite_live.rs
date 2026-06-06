#![cfg(all(feature = "sqlite", feature = "cache"))]
//! Live SQLite test for `DatabaseCache::ensure_table` — Django
//! `manage createcachetable` parity. The framework's CLI wrapper
//! (`manage createcachetable` / `create-cache-table`) is a thin
//! pass-through over `ensure_table`; we test the method directly
//! here.

use std::time::Duration;

use rustango::cache::{Cache, DatabaseCache};
use rustango::sql::{sqlx, Pool};

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    Pool::Sqlite(p)
}

#[tokio::test]
async fn ensure_table_creates_then_no_ops() {
    let pool = make_pool().await;
    let cache = DatabaseCache::new(pool.clone(), "cct_first");

    // First call creates the table.
    cache.ensure_table().await.unwrap();

    // Set + get round-trip works → table is wired up correctly.
    cache.set("k", "v", None).await.unwrap();
    let v: Option<String> = cache.get("k").await.unwrap();
    assert_eq!(v.as_deref(), Some("v"));

    // Second call must be idempotent (CREATE TABLE IF NOT EXISTS).
    cache.ensure_table().await.unwrap();

    // Data still here after the no-op.
    let v: Option<String> = cache.get("k").await.unwrap();
    assert_eq!(v.as_deref(), Some("v"));
}

#[tokio::test]
async fn ensure_table_supports_custom_table_name() {
    let pool = make_pool().await;
    let cache_a = DatabaseCache::new(pool.clone(), "cct_a");
    let cache_b = DatabaseCache::new(pool.clone(), "cct_b");
    cache_a.ensure_table().await.unwrap();
    cache_b.ensure_table().await.unwrap();

    // The two tables are isolated.
    cache_a.set("k", "in_a", None).await.unwrap();
    cache_b.set("k", "in_b", None).await.unwrap();

    assert_eq!(
        cache_a.get("k").await.unwrap().as_deref(),
        Some("in_a"),
        "cache A keeps its own value"
    );
    assert_eq!(
        cache_b.get("k").await.unwrap().as_deref(),
        Some("in_b"),
        "cache B keeps its own value"
    );
}

#[tokio::test]
async fn ensure_table_then_set_with_ttl_works() {
    let pool = make_pool().await;
    let cache = DatabaseCache::new(pool.clone(), "cct_ttl");
    cache.ensure_table().await.unwrap();

    cache
        .set("ttl", "v", Some(Duration::from_secs(60)))
        .await
        .unwrap();
    let v: Option<String> = cache.get("ttl").await.unwrap();
    assert_eq!(v.as_deref(), Some("v"));
}
