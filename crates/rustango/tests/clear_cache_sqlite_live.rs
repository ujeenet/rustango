#![cfg(all(feature = "sqlite", feature = "cache"))]
//! Live SQLite tests for `DatabaseCache::purge_expired` — Django
//! `manage clearsessions` parity. The framework's CLI wrapper
//! (`manage clear-cache`) is a thin pass-through over this method;
//! we test the method directly to avoid the CLI plumbing.

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
async fn purge_expired_returns_count_of_deleted_rows() {
    let pool = make_pool().await;
    let cache = DatabaseCache::new(pool.clone(), "cc_test");
    cache.ensure_table().await.unwrap();

    // Set 3 keys with a 50ms TTL.
    cache
        .set("k1", "v1", Some(Duration::from_millis(50)))
        .await
        .unwrap();
    cache
        .set("k2", "v2", Some(Duration::from_millis(50)))
        .await
        .unwrap();
    cache
        .set("k3", "v3", Some(Duration::from_millis(50)))
        .await
        .unwrap();
    // 1 no-TTL key — should NEVER be purged.
    cache.set("forever", "f", None).await.unwrap();

    // Wait past expiration.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let purged = cache.purge_expired().await.unwrap();
    assert_eq!(purged, 3, "3 TTL'd rows should be deleted");

    // No-TTL row still present.
    let v: Option<String> = cache.get("forever").await.unwrap();
    assert_eq!(v.as_deref(), Some("f"), "no-TTL row must survive purge");
}

#[tokio::test]
async fn purge_expired_no_op_when_nothing_stale() {
    let pool = make_pool().await;
    let cache = DatabaseCache::new(pool.clone(), "cc_test2");
    cache.ensure_table().await.unwrap();

    // Long TTL — nothing should be purged.
    cache
        .set("k1", "v1", Some(Duration::from_secs(3600)))
        .await
        .unwrap();
    cache.set("forever", "f", None).await.unwrap();

    let purged = cache.purge_expired().await.unwrap();
    assert_eq!(purged, 0, "no rows are stale yet");
}
