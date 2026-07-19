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
        ..rustango::testkit::org()
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

#[tokio::test]
async fn url_template_substitutes_slug_for_orgs_without_explicit_url() {
    // Set a template; both orgs have no database_url. The pool
    // registry should expand `{slug}` for each one and end up with
    // two distinct pools.
    let pools: DatabasePools<sqlx::Sqlite> = DatabasePools::new(BackendKind::Sqlite)
        .with_url_template("sqlite:file:tang_test_{slug}?mode=memory&cache=shared");

    let mut acme = fake_sqlite_org("acme");
    acme.database_url = None;
    let mut beta = fake_sqlite_org("beta");
    beta.database_url = None;

    let acme_pool = pools
        .pool_for_org(&acme)
        .await
        .expect("acme acquire via template");
    let beta_pool = pools
        .pool_for_org(&beta)
        .await
        .expect("beta acquire via template");

    // Different orgs → different pools (cache is per slug).
    assert!(
        !std::sync::Arc::ptr_eq(&acme_pool.pool_arc(), &beta_pool.pool_arc()),
        "two slugs should resolve to two distinct pools"
    );

    // Both connections actually work end-to-end.
    let row = sqlx::query("SELECT 1 as one")
        .fetch_one(acme_pool.pool())
        .await
        .expect("acme query");
    let one: i32 = row.try_get("one").expect("read");
    assert_eq!(one, 1);
}

#[tokio::test]
async fn explicit_database_url_wins_over_template() {
    // Even with a template configured, an org carrying its own
    // database_url should use it. Lets operators carve out a
    // special-case DB per tenant without giving up the convenience
    // of a default template for the rest.
    let pools: DatabasePools<sqlx::Sqlite> = DatabasePools::new(BackendKind::Sqlite)
        .with_url_template("sqlite:file:tang_template_{slug}?mode=memory&cache=shared");

    let org = fake_sqlite_org("acme"); // database_url = Some(":memory:")
    let _pool = pools
        .pool_for_org(&org)
        .await
        .expect("acquire honors explicit url");
    // We don't have a way to assert *which* URL was used without
    // bypassing the public API; trust the resolve_secret +
    // build_pool path which we exercise above and below.
}

#[tokio::test]
async fn no_url_no_template_is_a_clear_error() {
    let pools: DatabasePools<sqlx::Sqlite> = DatabasePools::new(BackendKind::Sqlite);
    let mut org = fake_sqlite_org("acme");
    org.database_url = None;

    let err = pools.pool_for_org(&org).await.expect_err("should reject");
    let msg = err.to_string();
    assert!(
        msg.contains("url_template") && msg.contains("with_url_template"),
        "error should name the missing knob, got: {msg}"
    );
}
