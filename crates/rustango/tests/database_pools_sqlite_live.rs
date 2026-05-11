//! Live SQLite live tests for the database-mode tenant pool registry.
//!
//! In-memory only (`sqlite::memory:`) so the test runs unconditionally
//! in CI without external setup. Validates the parallel
//! [`DatabasePools<Sqlite>`] structure against a fake `Org` row that
//! never touches the registry DB — `pool_for_org` only reads the org
//! struct, not the registry table.

#![cfg(all(feature = "tenancy", feature = "sqlite"))]

use rustango::sql::sqlx::{self, Row};
use rustango::tenancy::{BackendKind, DatabasePools, Org};

/// Build a synthetic Org row pointing at an in-memory SQLite DB. Each
/// invocation gets its own database (the `:memory:` connection
/// returned by sqlx is keyed per connection-string).
fn fake_sqlite_org(slug: &str) -> Org {
    Org {
        id: rustango::sql::Auto::default(),
        slug: slug.to_owned(),
        display_name: slug.to_owned(),
        storage_mode: "database".to_owned(),
        backend_kind: "sqlite".to_owned(),
        database_url: Some("sqlite::memory:".to_owned()),
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
async fn acquire_returns_working_sqlite_connection() {
    let pools: DatabasePools<sqlx::Sqlite> = DatabasePools::new(BackendKind::Sqlite);
    let org = fake_sqlite_org("acme");

    let mut conn = pools
        .acquire(&org)
        .await
        .expect("acquire sqlite connection");

    // The connection is hot — sqlite returns `1` from `SELECT 1`.
    let row = sqlx::query("SELECT 1 as one")
        .fetch_one(&mut **conn)
        .await
        .expect("query SELECT 1");
    let one: i32 = row.try_get("one").expect("read one");
    assert_eq!(one, 1);
}

#[tokio::test]
async fn pool_cached_on_repeat_acquire() {
    let pools: DatabasePools<sqlx::Sqlite> = DatabasePools::new(BackendKind::Sqlite);
    let org = fake_sqlite_org("acme");

    let first = pools.pool_for_org(&org).await.expect("first acquire");
    let second = pools.pool_for_org(&org).await.expect("second acquire");

    // Same pool — cache hit. We can't compare Arc pointers directly
    // through DatabasePool's public API; instead verify that the
    // underlying `Pool` is the same by checking the inner pointer
    // via pool_arc() equality.
    assert!(std::sync::Arc::ptr_eq(
        &first.pool_arc(),
        &second.pool_arc()
    ));
}

#[tokio::test]
async fn rejects_postgres_org() {
    let pools: DatabasePools<sqlx::Sqlite> = DatabasePools::new(BackendKind::Sqlite);
    let mut org = fake_sqlite_org("acme");
    org.backend_kind = "postgres".to_owned();

    let err = pools.pool_for_org(&org).await.expect_err("should reject");
    let msg = err.to_string();
    // The wording is the meaningful contract; if we change the
    // message in the future, update this assertion too.
    assert!(
        msg.contains("postgres") && msg.contains("sqlite"),
        "error should name both backends, got: {msg}"
    );
}

#[tokio::test]
async fn rejects_schema_mode() {
    let pools: DatabasePools<sqlx::Sqlite> = DatabasePools::new(BackendKind::Sqlite);
    let mut org = fake_sqlite_org("acme");
    org.storage_mode = "schema".to_owned();

    let err = pools.pool_for_org(&org).await.expect_err("should reject");
    let msg = err.to_string();
    assert!(
        msg.contains("database-mode") || msg.contains("Schema-mode"),
        "error should explain database-mode-only, got: {msg}"
    );
}

#[tokio::test]
async fn rejects_missing_database_url() {
    let pools: DatabasePools<sqlx::Sqlite> = DatabasePools::new(BackendKind::Sqlite);
    let mut org = fake_sqlite_org("acme");
    org.database_url = None;

    let err = pools.pool_for_org(&org).await.expect_err("should reject");
    assert!(
        err.to_string().contains("database_url"),
        "error should name database_url, got: {err}"
    );
}

#[tokio::test]
async fn invalidate_drops_cached_pool() {
    let pools: DatabasePools<sqlx::Sqlite> = DatabasePools::new(BackendKind::Sqlite);
    let org = fake_sqlite_org("acme");

    let first = pools.pool_for_org(&org).await.expect("first build");
    pools.invalidate(&org.slug).await;
    let second = pools.pool_for_org(&org).await.expect("rebuild");

    assert!(
        !std::sync::Arc::ptr_eq(&first.pool_arc(), &second.pool_arc()),
        "post-invalidate acquire should rebuild a fresh pool"
    );
}
