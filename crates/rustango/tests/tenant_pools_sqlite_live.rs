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
        schema_name: None,
        host_pattern: None,
        port: None,
        path_prefix: None,
        active: true,
        created_at: chrono::Utc::now(),
        brand_name: None,
        brand_tagline: None,
        logo_path: None,
        favicon_path: None,
        primary_color: None,
        theme_mode: None,
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
