//! Live integration test for `crate::tenancy::manage::run` on SQLite.
//!
//! v0.38 — covers the multi-feature dispatcher fix (commit a5d39d7)
//! and the generic-over-`DB` lift of the CLI runner. This proves that
//! a binary built with `features = ["postgres", "sqlite", "tenancy"]`
//! can dispatch a sqlite registry through the full tenancy CLI flow:
//! `migrate-registry` (generates the framework's `system/migrations/`
//! from the compiled models on demand and applies them),
//! `create-tenant` (inserts an Org row), `list-tenants` (verifies the
//! row appears).
//!
//! The same code path is what `Cli::tenancy().run()` uses when the
//! user runs `cargo run -- create-tenant acme` against a sqlite
//! database. Lifting `tenancy::manage::run` to `&TenantPools<DB>`
//! was a critical step for non-PG tenancy support; this test pins
//! the contract.

#![cfg(all(feature = "sqlite", feature = "tenancy"))]

use rustango::sql::sqlx;
use rustango::tenancy::TenantPools;

/// Build a sqlite tenant-pools handle backed by a tempfile DB.
/// We use a real on-disk file (not `:memory:`) so multiple connection
/// acquisitions in the same test see the same data — the framework's
/// per-request acquire path doesn't share a single in-memory connection.
async fn sqlite_pools_in_tempfile() -> (TenantPools<sqlx::Sqlite>, String, tempfile::TempDir) {
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let db_path = tmpdir.path().join("registry.db");
    let url = format!("sqlite://{}?mode=rwc", db_path.display());
    let pool = sqlx::SqlitePool::connect(&url)
        .await
        .expect("sqlite connect");
    let pools = TenantPools::<sqlx::Sqlite>::new(pool);
    (pools, url, tmpdir)
}

#[tokio::test]
async fn tenancy_manage_run_dispatches_init_then_create_then_list_on_sqlite() {
    let (pools, url, _tmpdir) = sqlite_pools_in_tempfile().await;
    let migrations_dir = tempfile::tempdir().expect("migrations tempdir");

    // Step 1: migrate-registry generates the framework's registry-scope
    // `system/migrations/` on demand from the compiled models and applies
    // them against the SQLite database — creates `rustango_orgs`,
    // `rustango_operators`, etc. There is no `init-tenancy` file-writing
    // step any more: the framework ships no hardcoded bootstrap JSON; its
    // schema flows through the same makemigrations/migrate engine as user
    // models.
    let mut buf: Vec<u8> = Vec::new();
    rustango::tenancy::manage::run_with_writer(
        &pools,
        &url,
        migrations_dir.path(),
        vec!["migrate-registry".to_owned()],
        &mut buf,
    )
    .await
    .expect("migrate-registry");

    // Verify the registry tables exist by querying via the pool.
    let pg_pool = pools.registry_inner();
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='rustango_orgs'",
    )
    .fetch_one(pg_pool)
    .await
    .expect("count rustango_orgs");
    assert_eq!(
        count, 1,
        "rustango_orgs should exist after migrate-registry"
    );

    // Verify the bootstrap migration emitted SQLite-flavored DDL
    // (slice 30 regression): `INTEGER PRIMARY KEY AUTOINCREMENT` for
    // `Auto<i64>` PKs, `TEXT` for strings — not the PG `BIGSERIAL` /
    // `TIMESTAMPTZ` types which SQLite accepts as columns but then
    // rejects NULL inserts into.
    let ddl: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='rustango_orgs'",
    )
    .fetch_one(pg_pool)
    .await
    .expect("rustango_orgs DDL");
    assert!(
        ddl.contains("INTEGER PRIMARY KEY AUTOINCREMENT"),
        "expected sqlite-flavored Auto<i64> PK syntax, got: {ddl}"
    );
    assert!(
        !ddl.contains("BIGSERIAL"),
        "BIGSERIAL is PG-specific and shouldn't appear in sqlite DDL, got: {ddl}"
    );
    assert!(
        !ddl.contains("TIMESTAMPTZ"),
        "TIMESTAMPTZ is PG-specific and shouldn't appear in sqlite DDL, got: {ddl}"
    );

    // Step 3: create-tenant inserts an Org row + sets up its storage.
    // We use storage_mode=database with a local file so SQLite can host
    // the tenant DB inline.
    let tenant_db_path = _tmpdir.path().join("acme.db");
    let tenant_url = format!("sqlite://{}?mode=rwc", tenant_db_path.display());
    let mut buf: Vec<u8> = Vec::new();
    rustango::tenancy::manage::run_with_writer(
        &pools,
        &url,
        migrations_dir.path(),
        vec![
            "create-tenant".to_owned(),
            "acme".to_owned(),
            "--display-name".to_owned(),
            "Acme Corp".to_owned(),
            "--mode".to_owned(),
            "database".to_owned(),
            "--database-url".to_owned(),
            tenant_url,
        ],
        &mut buf,
    )
    .await
    .expect("create-tenant");

    // Step 4: list-tenants should now show `acme`.
    let mut buf: Vec<u8> = Vec::new();
    rustango::tenancy::manage::run_with_writer(
        &pools,
        &url,
        migrations_dir.path(),
        vec!["list-tenants".to_owned()],
        &mut buf,
    )
    .await
    .expect("list-tenants");
    let out = String::from_utf8_lossy(&buf);
    assert!(
        out.contains("acme"),
        "list-tenants should mention `acme`, got:\n{out}"
    );
}

#[tokio::test]
async fn tenancy_manage_run_rejects_schema_mode_on_sqlite_with_friendly_error() {
    // Schema-mode is PG-only by language (`SET search_path`). On SQLite
    // the framework rejects it at request time. This pins the contract
    // for the validation error message + the fact that the error is
    // *user-friendly* (points at database-mode).
    let (pools, url, _tmpdir) = sqlite_pools_in_tempfile().await;
    let migrations_dir = tempfile::tempdir().expect("migrations tempdir");

    let mut buf: Vec<u8> = Vec::new();
    rustango::tenancy::manage::run_with_writer(
        &pools,
        &url,
        migrations_dir.path(),
        vec!["migrate-registry".to_owned()],
        &mut buf,
    )
    .await
    .expect("migrate-registry");

    // Attempt to create a schema-mode tenant.
    let mut buf: Vec<u8> = Vec::new();
    let err = rustango::tenancy::manage::run_with_writer(
        &pools,
        &url,
        migrations_dir.path(),
        vec![
            "create-tenant".to_owned(),
            "schemamode".to_owned(),
            "--mode".to_owned(),
            "schema".to_owned(),
        ],
        &mut buf,
    )
    .await
    .expect_err("schema-mode on sqlite should error");
    // Some implementations error inside create-tenant; some on first
    // pool acquisition. Either way the message should mention the
    // Postgres-only requirement so the user knows what to switch.
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("schema") || msg.contains("postgres"),
        "expected the error to mention schema-mode / Postgres, got: {err}"
    );
}
