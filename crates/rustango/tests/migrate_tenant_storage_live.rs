#![cfg(feature = "postgres")]
//! Integration test for the `migrate-tenant-storage` verb (item #58
//! in the future-feature backlog). The `--dry-run` flag short-circuits
//! before any actual pg_dump → psql work, so this test exercises the
//! verb's dispatch + Org-lookup + storage-mode validation path
//! without requiring `pg_dump` / `psql` on PATH or doing any real
//! data movement.
//!
//! A full live test (with real pg_dump) is left as follow-on work —
//! it would need both binaries on PATH plus a second writable
//! database to be the target.

#![cfg(all(feature = "tenancy", feature = "postgres"))]

use std::sync::Arc;

use rustango::sql::sqlx::PgPool;
use rustango::sql::Auto;
use rustango::tenancy::{manage::run_with_writer, Org, StorageMode, TenantPools};

use tokio::sync::Mutex;

/// Suite-wide lock. Every test in this file resets the shared PG
/// schema; under cargo's default parallel harness two tests would race
/// on PG's `pg_type_typname_nsp_index` / `pg_class_relname_nsp_index`
/// system-catalog uniques when both try to CREATE/DROP the same table
/// at once.
fn live_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    PgPool::connect(&url).await.ok()
}

async fn fresh(pool: &PgPool) {
    rustango::migrate::drop_all(pool).await.unwrap();
    rustango::migrate::apply_all(pool).await.unwrap();
}

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| (*v).to_owned()).collect()
}

#[tokio::test]
async fn migrate_tenant_storage_dry_run_prints_plan() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    // Seed a schema-mode tenant.
    let mut org = Org {
        id: Auto::default(),
        slug: "acme_dryrun".into(),
        display_name: "ACME Dry Run".into(),
        storage_mode: StorageMode::Schema.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: None,
        schema_name: Some("acme_dryrun".into()),
        host_pattern: None,
        port: None,
        path_prefix: None,
        active: true,
        created_at: now(),
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
        sso_secret_ref: None,    };
    org.insert(&pool).await.unwrap();

    let pools = TenantPools::new(pool.clone());
    let registry_url = std::env::var("DATABASE_URL").unwrap();
    let dir = std::env::temp_dir(); // not used by this verb

    let mut buf = Vec::<u8>::new();
    let res = run_with_writer(
        &pools,
        &registry_url,
        &dir,
        args(&[
            "migrate-tenant-storage",
            "acme_dryrun",
            "--to",
            "database",
            "--database-url",
            "postgres://example:password@db.example.com/acme_dryrun",
            "--dry-run",
        ]),
        &mut buf,
    )
    .await;
    res.unwrap();

    let output = String::from_utf8(buf).unwrap();
    assert!(
        output.contains("schema → database"),
        "expected mode-flip line: {output}"
    );
    assert!(
        output.contains("[dry-run] no changes"),
        "expected dry-run terminator: {output}"
    );
    assert!(
        output.contains("***@db.example.com"),
        "password should be redacted in printed plan: {output}"
    );

    rustango::migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn migrate_tenant_storage_rejects_same_mode_no_op() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    let mut org = Org {
        id: Auto::default(),
        slug: "noop_tenant".into(),
        display_name: "No-op".into(),
        storage_mode: StorageMode::Schema.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: None,
        schema_name: Some("noop_tenant".into()),
        host_pattern: None,
        port: None,
        path_prefix: None,
        active: true,
        created_at: now(),
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
        sso_secret_ref: None,    };
    org.insert(&pool).await.unwrap();

    let pools = TenantPools::new(pool.clone());
    let registry_url = std::env::var("DATABASE_URL").unwrap();

    let mut buf = Vec::<u8>::new();
    let err = run_with_writer(
        &pools,
        &registry_url,
        &std::env::temp_dir(),
        args(&[
            "migrate-tenant-storage",
            "noop_tenant",
            "--to",
            "schema",
            "--dry-run",
        ]),
        &mut buf,
    )
    .await
    .unwrap_err();

    assert!(
        format!("{err}").contains("already in `schema`"),
        "expected same-mode error: {err}"
    );
    rustango::migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn migrate_tenant_storage_rejects_database_target_without_url() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    let mut org = Org {
        id: Auto::default(),
        slug: "needs_url".into(),
        display_name: "Needs URL".into(),
        storage_mode: StorageMode::Schema.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: None,
        schema_name: Some("needs_url".into()),
        host_pattern: None,
        port: None,
        path_prefix: None,
        active: true,
        created_at: now(),
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
        sso_secret_ref: None,    };
    org.insert(&pool).await.unwrap();

    let pools = TenantPools::new(pool.clone());
    let registry_url = std::env::var("DATABASE_URL").unwrap();

    let mut buf = Vec::<u8>::new();
    let err = run_with_writer(
        &pools,
        &registry_url,
        &std::env::temp_dir(),
        args(&[
            "migrate-tenant-storage",
            "needs_url",
            "--to",
            "database",
            "--dry-run",
        ]),
        &mut buf,
    )
    .await
    .unwrap_err();

    assert!(
        format!("{err}").contains("--database-url"),
        "expected validation error mentioning --database-url: {err}"
    );
    rustango::migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn migrate_tenant_storage_rejects_unknown_slug() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    let pools = TenantPools::new(pool.clone());
    let registry_url = std::env::var("DATABASE_URL").unwrap();

    let mut buf = Vec::<u8>::new();
    let err = run_with_writer(
        &pools,
        &registry_url,
        &std::env::temp_dir(),
        args(&[
            "migrate-tenant-storage",
            "ghost_tenant",
            "--to",
            "database",
            "--database-url",
            "postgres://x:y@h/d",
            "--dry-run",
        ]),
        &mut buf,
    )
    .await
    .unwrap_err();

    assert!(
        format!("{err}").contains("not found"),
        "expected `not found` for unknown slug: {err}"
    );
    rustango::migrate::drop_all(&pool).await.unwrap();
}

// Suppress unused-import warning when the file's only consumer
// is the test runner.
#[allow(dead_code)]
fn _force_use_arc() {
    let _: Option<Arc<TenantPools>> = None;
}
