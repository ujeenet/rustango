#![cfg(all(feature = "tenancy", feature = "postgres"))]
//! Live tests for `TenantPools`. Bootstraps a registry + 2 schema-mode
//! tenants (acme + globex), exercises pool resolution and search_path
//! scoping. Database-mode tenants are tested separately because they
//! need a second Postgres database to point at.
//!
//! Reads `DATABASE_URL`. Skips silently when unset.

use rustango::migrate;
use rustango::sql::{sqlx, Auto};
use rustango::tenancy::{Org, StorageMode, TenantPool, TenantPools};

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

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(
        sqlx::PgPool::connect(&url)
            .await
            .expect("connect to DATABASE_URL"),
    )
}

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

async fn seed_org(
    pool: &sqlx::PgPool,
    slug: &str,
    mode: StorageMode,
    schema_name: Option<&str>,
    database_url: Option<&str>,
) -> Org {
    let mut org = Org {
        id: Auto::default(),
        slug: slug.into(),
        display_name: slug.to_uppercase(),
        storage_mode: mode.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: database_url.map(str::to_owned),
        schema_name: schema_name.map(str::to_owned),
        host_pattern: Some(format!("{slug}.app.test")),
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
    org.insert(pool).await.unwrap();
    org
}

async fn create_schema(pool: &sqlx::PgPool, name: &str) {
    let sql = format!(r#"CREATE SCHEMA IF NOT EXISTS "{name}""#);
    sqlx::query(&sql).execute(pool).await.unwrap();
}

async fn drop_schema(pool: &sqlx::PgPool, name: &str) {
    let sql = format!(r#"DROP SCHEMA IF EXISTS "{name}" CASCADE"#);
    sqlx::query(&sql).execute(pool).await.unwrap();
}

async fn current_schema(conn: &mut sqlx::PgConnection) -> String {
    let row: (String,) = sqlx::query_as("SELECT current_schema()::text")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    row.0
}

#[tokio::test]
async fn schema_mode_pool_for_org_returns_schema_variant() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    drop_schema(&pool, "acme_tp_t1").await;

    let acme = seed_org(
        &pool,
        "acme_tp_t1",
        StorageMode::Schema,
        Some("acme_tp_t1"),
        None,
    )
    .await;

    let pools = TenantPools::new(pool.clone());
    let tp = pools.pool_for_org(&acme).await.unwrap();
    assert!(tp.is_schema(), "schema mode org → TenantPool::Schema");
    match tp {
        TenantPool::Schema { schema, .. } => assert_eq!(schema, "acme_tp_t1"),
        TenantPool::Database { .. } => panic!("expected Schema variant"),
    }

    drop_schema(&pool, "acme_tp_t1").await;
    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn schema_mode_acquire_sets_search_path_to_tenant_schema() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    drop_schema(&pool, "acme_tp_t2").await;
    create_schema(&pool, "acme_tp_t2").await;

    let acme = seed_org(
        &pool,
        "acme_tp_t2",
        StorageMode::Schema,
        Some("acme_tp_t2"),
        None,
    )
    .await;

    let pools = TenantPools::new(pool.clone());
    let mut conn = pools.acquire(&acme).await.unwrap();
    let schema = current_schema(&mut conn).await;
    assert_eq!(
        schema, "acme_tp_t2",
        "current_schema should be the tenant's"
    );
    assert_eq!(conn.schema(), Some("acme_tp_t2"));

    drop_schema(&pool, "acme_tp_t2").await;
    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn schema_mode_pool_uses_slug_when_schema_name_is_none() {
    // `schema_name = None` means "use the slug as the schema name".
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    drop_schema(&pool, "fallback_tp_t3").await;
    create_schema(&pool, "fallback_tp_t3").await;

    let fallback = seed_org(&pool, "fallback_tp_t3", StorageMode::Schema, None, None).await;

    let pools = TenantPools::new(pool.clone());
    let mut conn = pools.acquire(&fallback).await.unwrap();
    assert_eq!(current_schema(&mut conn).await, "fallback_tp_t3");

    drop_schema(&pool, "fallback_tp_t3").await;
    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn database_mode_without_database_url_errors() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let bad = seed_org(&pool, "bad_tp_t4", StorageMode::Database, None, None).await;
    let pools = TenantPools::new(pool.clone());
    let err = pools.pool_for_org(&bad).await.unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("database_url"), "got: {msg}");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn database_mode_pool_caches_per_slug() {
    // Database-mode pool building is cached. We can't easily spin
    // up a second Postgres for the test, so we point the database_url
    // at the registry itself — that's a degenerate but valid
    // configuration for proving the cache shape.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let dbm = seed_org(&pool, "dbm_tp_t5", StorageMode::Database, None, Some(&url)).await;

    let pools = TenantPools::new(pool.clone());
    assert_eq!(pools.cached_database_pool_count().await, 0);

    // First lookup builds the pool and caches it.
    let _tp1 = pools.pool_for_org(&dbm).await.unwrap();
    assert_eq!(pools.cached_database_pool_count().await, 1);

    // Second lookup hits the cache; count stays at 1.
    let _tp2 = pools.pool_for_org(&dbm).await.unwrap();
    assert_eq!(pools.cached_database_pool_count().await, 1);

    // Invalidate drops the cached pool.
    pools.invalidate(&dbm.slug).await;
    assert_eq!(pools.cached_database_pool_count().await, 0);

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn database_mode_resolves_secret_reference_via_resolver() {
    // EnvSecretsResolver: store an env:// reference in the Org row,
    // resolver picks it up, builds the pool from the resolved URL.
    // We still point at the registry DB (only Postgres available
    // in the test harness); the test demonstrates the indirection
    // works.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url_var = "DATABASE_URL";
    if std::env::var(url_var).is_err() {
        return;
    }
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let dbm = seed_org(
        &pool,
        "secret_tp_t6",
        StorageMode::Database,
        None,
        Some(&format!("env://{url_var}")),
    )
    .await;

    let pools = TenantPools::with_secrets(
        pool.clone(),
        rustango::tenancy::ChainSecretsResolver::standard(),
    );
    let tp = pools.pool_for_org(&dbm).await.unwrap();
    assert!(!tp.is_schema(), "database-mode tenant");

    migrate::drop_all(&pool).await.unwrap();
}
