//! `DatabaseCache` keys longer than MySQL's `VARCHAR(255)` round-trip
//! and stay distinct, on every dialect (#1674).
//!
//! SQLite always runs; Postgres and MySQL skip when their URL is unset.

#![cfg(feature = "cache")]

use std::time::Duration;

use rustango::cache::{Cache, DatabaseCache};
use rustango::sql::Pool;

async fn assert_long_keys(pool: Pool, table: &str) {
    let cache = DatabaseCache::new(pool, table);
    let _ = cache.drop_table().await;
    cache.ensure_table().await.expect("ensure_table");

    let shared = "k".repeat(255);
    let a = format!("{shared}-alpha-{}", "a".repeat(40));
    let b = format!("{shared}-beta-{}", "b".repeat(40));

    // A lock on a long name must be takeable, visible and releasable.
    assert!(
        cache.add(&a, "token-a", None).await.unwrap(),
        "{table}: add"
    );
    assert_eq!(
        cache.get(&a).await.unwrap().as_deref(),
        Some("token-a"),
        "{table}: a long key must round-trip"
    );
    assert!(
        cache.add(&b, "token-b", None).await.unwrap(),
        "{table}: a key sharing the first 255 bytes is a different key"
    );
    assert_eq!(cache.get(&b).await.unwrap().as_deref(), Some("token-b"));
    assert_eq!(cache.get(&a).await.unwrap().as_deref(), Some("token-a"));

    cache.delete(&a).await.unwrap();
    assert_eq!(cache.get(&a).await.unwrap(), None, "{table}: delete");
    assert_eq!(cache.get(&b).await.unwrap().as_deref(), Some("token-b"));

    // set / incr / touch go through the same key.
    cache
        .set(&a, "v", Some(Duration::from_secs(60)))
        .await
        .unwrap();
    assert_eq!(cache.get(&a).await.unwrap().as_deref(), Some("v"));
    assert!(cache.touch(&a, None).await.unwrap(), "{table}: touch");
    let c = format!("{shared}-counter");
    assert_eq!(cache.incr(&c, 2, None).await.unwrap(), 2);
    assert_eq!(cache.incr(&c, 3, None).await.unwrap(), 5);

    // Short keys keep their exact stored form, so existing rows still hit.
    cache.set("short", "s", None).await.unwrap();
    assert_eq!(cache.get("short").await.unwrap().as_deref(), Some("s"));

    // A prefix clear still reaches long keys under that prefix.
    let scoped = format!("tenant:acme:{shared}");
    cache.set(&scoped, "x", None).await.unwrap();
    cache.set("tenant:globex:x", "y", None).await.unwrap();
    cache.delete_prefix("tenant:acme:").await.unwrap();
    assert_eq!(cache.get(&scoped).await.unwrap(), None, "{table}: prefix");
    assert_eq!(
        cache.get("tenant:globex:x").await.unwrap().as_deref(),
        Some("y")
    );

    let _ = cache.drop_table().await;
}

async fn pool_or_skip(var: &str) -> Option<Pool> {
    let url = std::env::var(var).ok()?;
    match Pool::connect(&url).await {
        Ok(pool) => Some(pool),
        Err(e) => {
            eprintln!("skipping: {var} is set but unreachable ({e})");
            None
        }
    }
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn long_keys_round_trip_on_sqlite() {
    let pool = rustango::sql::sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite");
    assert_long_keys(Pool::Sqlite(pool), "rustango_cache_long_sq").await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn long_keys_round_trip_on_postgres() {
    let Some(pool) = pool_or_skip("DATABASE_URL").await else {
        return;
    };
    assert_long_keys(pool, "rustango_cache_long_pg").await;
}

#[cfg(feature = "mysql")]
#[tokio::test]
async fn long_keys_round_trip_on_mysql() {
    let Some(pool) = pool_or_skip("MYSQL_TEST_URL").await else {
        return;
    };
    assert_long_keys(pool, "rustango_cache_long_my").await;
}
