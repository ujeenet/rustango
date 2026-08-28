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

    cache
        .set("k", "v", Some(Duration::from_secs(1)))
        .await
        .unwrap();
    // Still present immediately.
    assert_eq!(cache.get("k").await.unwrap().as_deref(), Some("v"));
    // Wait past TTL — sqlite has 1-second granularity in the schema's BIGINT.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
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

#[tokio::test]
async fn purge_expired_drops_expired_rows_keeps_others() {
    // Sister method to the implicit lazy GC on `get` — `purge_expired`
    // is the eager flow for ops cron jobs / `manage clearcache`.
    // Set three rows: one expired, one with TTL still alive, one
    // with no TTL. Wait past the short TTL, purge, verify the
    // expired one is gone and the others survive.
    let pool = fresh_pool().await;
    let cache = DatabaseCache::new(pool, "rustango_cache_purge");
    cache.ensure_table().await.unwrap();

    cache
        .set("short", "doomed", Some(Duration::from_secs(1)))
        .await
        .unwrap();
    cache
        .set("long", "alive", Some(Duration::from_secs(3600)))
        .await
        .unwrap();
    cache.set("forever", "alive", None).await.unwrap();

    tokio::time::sleep(Duration::from_millis(1_100)).await;
    cache.purge_expired().await.expect("purge_expired");

    // The expired row is gone — `get` returns None (and doesn't even
    // hit the lazy GC since the row is already removed).
    assert_eq!(cache.get("short").await.unwrap(), None);
    // Both other rows survive.
    assert_eq!(cache.get("long").await.unwrap().as_deref(), Some("alive"));
    assert_eq!(
        cache.get("forever").await.unwrap().as_deref(),
        Some("alive")
    );
}

#[tokio::test]
async fn purge_expired_is_no_op_when_table_has_only_live_rows() {
    let pool = fresh_pool().await;
    let cache = DatabaseCache::new(pool, "rustango_cache_purge_noop");
    cache.ensure_table().await.unwrap();
    cache.set("k", "v", None).await.unwrap();
    cache
        .set("ttl", "v", Some(Duration::from_secs(3600)))
        .await
        .unwrap();

    cache.purge_expired().await.expect("purge_expired");

    assert_eq!(cache.get("k").await.unwrap().as_deref(), Some("v"));
    assert_eq!(cache.get("ttl").await.unwrap().as_deref(), Some("v"));
}

/// `delete_prefix` (#1227) drops exactly the matching keys. This is the
/// path `ScopedCache::clear` uses, so getting it wrong means either a
/// tenant's invalidation silently doing nothing or wiping a neighbour's
/// entries.
#[tokio::test]
async fn delete_prefix_drops_only_the_matching_keys() {
    let pool = fresh_pool().await;
    let cache = DatabaseCache::new(pool, "rustango_cache_prefix");
    cache.ensure_table().await.unwrap();

    cache.set("tenant:acme:a", "1", None).await.unwrap();
    cache.set("tenant:acme:b", "2", None).await.unwrap();
    cache.set("tenant:globex:a", "9", None).await.unwrap();
    cache.set("unrelated", "keep", None).await.unwrap();

    cache
        .delete_prefix("tenant:acme:")
        .await
        .expect("prefix delete");

    assert_eq!(cache.get("tenant:acme:a").await.unwrap(), None);
    assert_eq!(cache.get("tenant:acme:b").await.unwrap(), None);
    assert_eq!(
        cache.get("tenant:globex:a").await.unwrap().as_deref(),
        Some("9"),
        "another tenant's entry must survive"
    );
    assert_eq!(
        cache.get("unrelated").await.unwrap().as_deref(),
        Some("keep")
    );
}

/// A slug that is a prefix of a longer one must not be swept by it —
/// `tenant:acme:` vs `tenant:acme-corp:`.
#[tokio::test]
async fn delete_prefix_respects_the_separator() {
    let pool = fresh_pool().await;
    let cache = DatabaseCache::new(pool, "rustango_cache_prefix_sep");
    cache.ensure_table().await.unwrap();

    cache.set("tenant:acme:k", "short", None).await.unwrap();
    cache.set("tenant:acme-corp:k", "long", None).await.unwrap();

    cache.delete_prefix("tenant:acme:").await.unwrap();

    assert_eq!(cache.get("tenant:acme:k").await.unwrap(), None);
    assert_eq!(
        cache.get("tenant:acme-corp:k").await.unwrap().as_deref(),
        Some("long"),
    );
}

/// `%` and `_` are LIKE wildcards. A prefix containing them must match
/// literally, or one namespace's clear could sweep another's rows —
/// `_` alone would match *any* single character.
#[tokio::test]
async fn delete_prefix_escapes_like_wildcards() {
    let pool = fresh_pool().await;
    let cache = DatabaseCache::new(pool, "rustango_cache_prefix_wild");
    cache.ensure_table().await.unwrap();

    cache.set("a_b:key", "underscore", None).await.unwrap();
    cache
        .set("axb:key", "wildcard-would-match", None)
        .await
        .unwrap();
    cache.set("100%:key", "percent", None).await.unwrap();
    cache
        .set("100zz:key", "percent-would-match", None)
        .await
        .unwrap();

    cache.delete_prefix("a_b:").await.unwrap();
    assert_eq!(cache.get("a_b:key").await.unwrap(), None);
    assert_eq!(
        cache.get("axb:key").await.unwrap().as_deref(),
        Some("wildcard-would-match"),
        "`_` must be escaped, not treated as a single-character wildcard"
    );

    cache.delete_prefix("100%:").await.unwrap();
    assert_eq!(cache.get("100%:key").await.unwrap(), None);
    assert_eq!(
        cache.get("100zz:key").await.unwrap().as_deref(),
        Some("percent-would-match"),
        "`%` must be escaped, not treated as a multi-character wildcard"
    );
}
