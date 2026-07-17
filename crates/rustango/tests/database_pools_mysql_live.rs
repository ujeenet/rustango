//! Live MySQL tests for `DatabasePools<MySql>`.
//!
//! Env-gated on `MYSQL_URL`. Mirrors the in-memory SQLite live tests
//! in `database_pools_sqlite_live.rs` so the two backends stay
//! behaviorally aligned. Skips silently when the env var is unset
//! — `MYSQL_URL=mysql://user:pass@localhost:3306/rustango_test
//! cargo test --features mysql,...` enables them.
//!
//! The tests run against ONE shared database (not per-test) for
//! simplicity; they only do `SELECT 1`-style queries that don't
//! mutate schema, so re-running is safe.

#![cfg(all(feature = "tenancy", feature = "mysql"))]

use rustango::sql::sqlx::{self, Row};
use rustango::tenancy::{BackendKind, DatabasePools, Org};

fn fake_mysql_org(slug: &str, url: &str) -> Org {
    Org {
        id: rustango::sql::Auto::default(),
        slug: slug.to_owned(),
        display_name: slug.to_owned(),
        storage_mode: "database".into(),
        backend_kind: "mysql".into(),
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
        sso_enabled: false,
        sso_provider: None,
        sso_issuer_url: None,
        sso_client_id: None,
        sso_secret_ref: None,
    }
}

#[tokio::test]
async fn acquire_returns_working_mysql_connection() {
    let Ok(url) = std::env::var("MYSQL_URL") else {
        eprintln!("MYSQL_URL not set — skipping");
        return;
    };
    let pools: DatabasePools<sqlx::MySql> = DatabasePools::new(BackendKind::MySql);
    let org = fake_mysql_org("acme", &url);

    let mut conn = pools.acquire(&org).await.expect("acquire mysql connection");
    // DatabaseConn → PoolConnection<MySql> → MySqlConnection; query
    // wants `&mut MySqlConnection`.
    let row = sqlx::query("SELECT 1 as one")
        .fetch_one(&mut **conn)
        .await
        .expect("query SELECT 1");
    let one: i32 = row.try_get("one").expect("read one");
    assert_eq!(one, 1);
}

#[tokio::test]
async fn pool_cached_on_repeat_acquire() {
    let Ok(url) = std::env::var("MYSQL_URL") else {
        eprintln!("MYSQL_URL not set — skipping");
        return;
    };
    let pools: DatabasePools<sqlx::MySql> = DatabasePools::new(BackendKind::MySql);
    let org = fake_mysql_org("acme", &url);

    let first = pools.pool_for_org(&org).await.expect("first acquire");
    let second = pools.pool_for_org(&org).await.expect("second acquire");
    assert!(std::sync::Arc::ptr_eq(
        &first.pool_arc(),
        &second.pool_arc()
    ));
}

#[tokio::test]
async fn rejects_postgres_org() {
    // No DB connection required for this validation path — runs even
    // without MYSQL_URL.
    let pools: DatabasePools<sqlx::MySql> = DatabasePools::new(BackendKind::MySql);
    let mut org = fake_mysql_org("acme", "mysql://nobody:nothing@127.0.0.1:0/none");
    org.backend_kind = "postgres".into();

    let err = pools.pool_for_org(&org).await.expect_err("should reject");
    assert!(
        err.to_string().contains("postgres") && err.to_string().contains("mysql"),
        "error should name both backends, got: {err}"
    );
}

#[tokio::test]
async fn rejects_schema_mode() {
    // Same — no DB needed.
    let pools: DatabasePools<sqlx::MySql> = DatabasePools::new(BackendKind::MySql);
    let mut org = fake_mysql_org("acme", "mysql://nobody:nothing@127.0.0.1:0/none");
    org.storage_mode = "schema".into();

    let err = pools.pool_for_org(&org).await.expect_err("should reject");
    let msg = err.to_string();
    assert!(
        msg.contains("database-mode") || msg.contains("Schema-mode"),
        "error should explain database-mode-only, got: {msg}"
    );
}
