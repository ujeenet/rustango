//! Live regression for v0.34 slice 3 — `TenantPools<sqlx::Sqlite>`
//! works end-to-end. Proves the generalized struct can hold a SQLite
//! registry, build per-tenant SQLite pools, and acquire connections —
//! all without any Postgres dependency at the type level.
//!
//! Companion to `pure_sqlite_stack_live.rs` (which tests the
//! `DatabasePools<DB>` path) — this file covers the unified
//! `TenantPools<DB>` post-slice-3.

#![cfg(all(feature = "sqlite", feature = "tenancy"))]

use rustango::sql::{sqlx, Auto};
use rustango::tenancy::{Org, TenantPools};

fn fake_db_org(slug: &str, url: &str) -> Org {
    Org {
        id: Auto::default(),
        slug: slug.to_owned(),
        display_name: slug.to_owned(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: Some(url.to_owned()),
        ..rustango::testkit::org()
    }
}

#[tokio::test]
async fn tenant_pools_sqlite_constructs_and_acquires() {
    let registry: sqlx::SqlitePool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("registry pool");
    let pools: TenantPools<sqlx::Sqlite> = TenantPools::new(registry);
    // Generic registry_pool returns Pool::Sqlite.
    assert_eq!(pools.registry_pool().dialect().name(), "sqlite");

    let org = fake_db_org(
        "acme",
        "sqlite:file:tenant_pools_test_acme?mode=memory&cache=shared",
    );

    // database_pool_for_org should yield a Database variant + the conn
    // can run a real query.
    let mut conn = pools.database_acquire(&org).await.expect("acquire");
    let row: (i64,) = sqlx::query_as("SELECT 42")
        .fetch_one(&mut **conn)
        .await
        .expect("query");
    assert_eq!(row.0, 42);
}

#[tokio::test]
async fn tenant_pools_sqlite_rejects_schema_mode() {
    let registry: sqlx::SqlitePool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("registry pool");
    let pools: TenantPools<sqlx::Sqlite> = TenantPools::new(registry);

    let mut org = fake_db_org("schemy", "sqlite::memory:");
    org.storage_mode = "schema".into();
    org.schema_name = Some("schemy".into());

    let err = pools.database_pool_for_org(&org).await;
    assert!(err.is_err(), "schema-mode on sqlite must be rejected");
    let msg = format!("{:?}", err.unwrap_err());
    assert!(
        msg.contains("schema-mode") || msg.contains("schema"),
        "error message should explain schema-mode is PG-only, got: {msg}",
    );
}

#[tokio::test]
async fn tenant_pools_sqlite_cache_persists() {
    let registry: sqlx::SqlitePool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("registry pool");
    let pools: TenantPools<sqlx::Sqlite> = TenantPools::new(registry);

    let org = fake_db_org(
        "beta",
        "sqlite:file:tenant_pools_test_beta?mode=memory&cache=shared",
    );

    // First acquire creates the cached pool.
    assert_eq!(pools.cached_database_pool_count().await, 0);
    {
        let mut conn = pools.database_acquire(&org).await.expect("acquire 1");
        sqlx::query("CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY)")
            .execute(&mut **conn)
            .await
            .expect("create");
        sqlx::query("INSERT INTO t DEFAULT VALUES")
            .execute(&mut **conn)
            .await
            .expect("insert");
    }
    assert_eq!(pools.cached_database_pool_count().await, 1);

    // Second acquire reuses the cached pool — sees the prior row.
    let mut conn = pools.database_acquire(&org).await.expect("acquire 2");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM t")
        .fetch_one(&mut **conn)
        .await
        .expect("count");
    assert_eq!(count, 1, "cache should persist between acquires");

    // Invalidate drops the cached pool.
    pools.invalidate(&org.slug).await;
    assert_eq!(pools.cached_database_pool_count().await, 0);
}

/// Past the cap the cache evicts its most idle tenant instead of
/// refusing (#1527). Asserts both halves: the over-cap tenant is
/// served, and the cache stays bounded.
#[tokio::test]
async fn database_cache_evicts_the_idle_tenant_instead_of_refusing() {
    let registry: sqlx::SqlitePool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("registry pool");
    let cfg = rustango::tenancy::TenantPoolsConfig {
        max_cached_database_pools: 2,
        ..Default::default()
    };
    let pools: TenantPools<sqlx::Sqlite> = TenantPools::new(registry).config(cfg);

    let org = |n: usize| {
        fake_db_org(
            &format!("evict{n}"),
            &format!("sqlite:file:tenant_pools_evict_{n}?mode=memory&cache=shared"),
        )
    };

    // Fill to the cap, then touch tenant 0 so tenant 1 is the idle one.
    for n in 0..2 {
        pools.database_pool_for_org(&org(n)).await.expect("fill");
    }
    assert_eq!(pools.cached_database_pool_count().await, 2);
    pools
        .database_pool_for_org(&org(0))
        .await
        .expect("touch 0 so it is not the eviction victim");

    // The over-cap tenant must get a usable pool, not an error.
    let mut conn = pools
        .database_acquire(&org(2))
        .await
        .expect("tenant past the cap must still be served");
    sqlx::query("CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY)")
        .execute(&mut **conn)
        .await
        .expect("the evicting path must hand back a live pool");
    drop(conn);

    // And the cache must still be bounded.
    assert_eq!(
        pools.cached_database_pool_count().await,
        2,
        "eviction must hold the cache at the cap, not grow past it",
    );

    // The old bug returned the same error forever, so one success is
    // not enough.
    for _ in 0..3 {
        pools
            .database_pool_for_org(&org(2))
            .await
            .expect("over-cap tenant stays served on later requests");
    }
    assert_eq!(pools.cached_database_pool_count().await, 2);
}

async fn marker(pools: &TenantPools<sqlx::Sqlite>, org: &Org) -> String {
    let mut conn = pools.database_acquire(org).await.expect("acquire");
    sqlx::query_scalar("SELECT name FROM marker")
        .fetch_one(&mut **conn)
        .await
        .expect("marker")
}

async fn seed_marker(url: &str, name: &str) -> sqlx::SqlitePool {
    let db = sqlx::SqlitePool::connect(url).await.expect("tenant db");
    sqlx::query("CREATE TABLE marker (name TEXT)")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO marker VALUES (?)")
        .bind(name)
        .execute(&db)
        .await
        .unwrap();
    db
}

/// Another process moved the tenant: the fresh Org names a new URL, so
/// the cached pool must not be served (#1882). Covers a stale pool
/// re-inserted by a build that raced an invalidate, too.
#[tokio::test]
async fn cached_pool_follows_a_database_url_change_without_invalidate() {
    let registry = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
    let pools: TenantPools<sqlx::Sqlite> = TenantPools::new(registry);
    let old_url = "sqlite:file:tenant_pools_moved_old?mode=memory&cache=shared";
    let new_url = "sqlite:file:tenant_pools_moved_new?mode=memory&cache=shared";
    let _old = seed_marker(old_url, "old").await;
    let _new = seed_marker(new_url, "new").await;

    let mut org = fake_db_org("moved", old_url);
    assert_eq!(marker(&pools, &org).await, "old");
    org.database_url = Some(new_url.to_owned());
    assert_eq!(marker(&pools, &org).await, "new", "served the old database");
    assert_eq!(pools.cached_database_pool_count().await, 1);
}
