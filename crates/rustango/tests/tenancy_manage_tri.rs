//! Tenancy manage verbs and `api::create_tenant`, on every registry backend.
//! Tenants are database-mode on the registry's backend.

#![cfg(all(feature = "tenancy", feature = "testkit", feature = "sqlite"))]

use std::path::Path;

use rustango::core::Column as _;
use rustango::sql::{FetcherPool as _, Pool};
use rustango::tenancy::manage::api::{create_tenant, CreateTenantOpts};
use rustango::tenancy::{BackendKind, Org, StorageMode, TenancyError, TenantPools};
use rustango::tri_dialect_test;

async fn setup(pool: &Pool) {
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
}

fn name(tag: &str) -> String {
    format!("mt-{tag}-{}", std::process::id())
}

async fn create(
    pool: &Pool,
    dir: &Path,
    slug: &str,
    opts: CreateTenantOpts,
) -> Result<Org, TenancyError> {
    match pool.clone() {
        #[cfg(feature = "postgres")]
        Pool::Postgres(p) => create_tenant(&TenantPools::new(p), "", dir, slug, opts).await,
        #[cfg(feature = "mysql")]
        Pool::Mysql(p) => create_tenant(&TenantPools::new(p), "", dir, slug, opts).await,
        Pool::Sqlite(p) => create_tenant(&TenantPools::new(p), "", dir, slug, opts).await,
    }
}

/// A database-mode tenant on the registry's own backend: PG and MySQL reuse
/// the test database, SQLite gets a file.
fn tenant_opts(pool: &Pool, tmp: &Path) -> CreateTenantOpts {
    let (backend, url) = match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(_) => (
            BackendKind::Postgres,
            std::env::var("DATABASE_URL").unwrap(),
        ),
        #[cfg(feature = "mysql")]
        Pool::Mysql(_) => (BackendKind::MySql, std::env::var("MYSQL_TEST_URL").unwrap()),
        Pool::Sqlite(_) => (
            BackendKind::Sqlite,
            format!("sqlite://{}?mode=rwc", tmp.join("t.db").display()),
        ),
    };
    CreateTenantOpts {
        mode: StorageMode::Database,
        backend,
        database_url: Some(url),
        ..CreateTenantOpts::default()
    }
}

/// One tenant migration whose SQL fails on every backend, writing nothing.
fn broken_migration(dir: &Path) {
    let mig = rustango::migrate::Migration {
        name: "0001_fail".to_owned(),
        created_at: "2026-10-09T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: rustango::migrate::MigrationScope::Tenant,
        replaces: Vec::new(),
        snapshot: serde_json::from_value(serde_json::json!({ "tables": [] })).unwrap(),
        forward: vec![rustango::migrate::Operation::Data(
            rustango::migrate::DataOp {
                sql: "SELECT * FROM rustango_no_such_table_2392".into(),
                reverse_sql: None,
                reversible: false,
            },
        )],
    };
    rustango::migrate::file::write(&dir.join("0001_fail.json"), &mig).unwrap();
}

async fn org_by_slug(pool: &Pool, slug: &str) -> Option<Org> {
    let rows: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(slug.to_owned()))
        .fetch(pool)
        .await
        .expect("fetch org");
    rows.into_iter().next()
}

/// #2392 — a failed migration is an error, and the tenant stays inactive.
async fn create_tenant_returns_a_failed_migration(pool: &Pool) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    broken_migration(&dir);
    let slug = name("broken");
    let r = create(pool, &dir, &slug, tenant_opts(pool, tmp.path())).await;
    let err = r.expect_err("a failed migration must fail create_tenant");
    assert!(
        err.to_string().contains("rustango_no_such_table_2392"),
        "{err}"
    );
    let org = org_by_slug(pool, &slug)
        .await
        .expect("row kept for the operator");
    assert!(!org.active, "a half-migrated tenant must not resolve");
}

/// #2392 — a clean run activates the tenant.
async fn create_tenant_activates_after_migrating(pool: &Pool) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    let slug = name("clean");
    let org = create(pool, &dir, &slug, tenant_opts(pool, tmp.path()))
        .await
        .expect("create");
    assert!(org.active);
}

tri_dialect_test! {
    setup: setup,
    sqlite: file,
    scenarios: [
        create_tenant_returns_a_failed_migration,
        create_tenant_activates_after_migrating,
    ],
}
