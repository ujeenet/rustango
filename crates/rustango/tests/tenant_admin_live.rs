#![cfg(all(feature = "tenancy", feature = "postgres"))]
//! Live tests for the tenant-aware admin router.
//!
//! Two tenants in different storage modes; same admin URL serves
//! their respective data (or 404s if no tenant resolved).
//!
//! Reads `DATABASE_URL`. Skips silently when unset.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use rustango::sql::{sqlx, Auto};
use rustango::tenancy::{
    admin::TenantAdminBuilder, ChainResolver, HeaderResolver, Org, StorageMode, SubdomainResolver,
    TenantPools,
};
use rustango::{migrate as rmig, Model};
use tower::ServiceExt;

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn unique(prefix: &str) -> String {
    let n = UNIQ.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    format!("{prefix}_{pid}_{n}")
}

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
    Some(sqlx::PgPool::connect(&url).await.unwrap())
}

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

async fn drop_schema(pool: &sqlx::PgPool, name: &str) {
    let sql = format!(r#"DROP SCHEMA IF EXISTS "{name}" CASCADE"#);
    sqlx::query(&sql).execute(pool).await.unwrap();
}

/// Define a tiny model for the per-tenant data so the admin actually
/// has something to render.
#[derive(Model, Debug, Clone)]
#[rustango(table = "ten_admin_widget", display = "label")]
#[allow(dead_code)]
pub struct Widget {
    #[rustango(primary_key)]
    pub id: rustango::Auto<i64>,
    #[rustango(max_length = 64)]
    pub label: String,
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[tokio::test]
async fn database_mode_admin_serves_tenant_data() {
    // Single tenant in database mode pointing at the registry DB
    // (degenerate but sufficient — proves the dispatch flow).
    // Insert a Widget through the registry pool, then GET / via the
    // admin: the row must show up.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();

    // Apply registry schema (Org table + Widget table since we live
    // in the same DB).
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    // Seed an org and a widget.
    let org_slug = unique("acme");
    let mut org = Org {
        id: Auto::default(),
        slug: org_slug.clone(),
        display_name: "ACME".into(),
        storage_mode: StorageMode::Database.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: Some(url.clone()),
        schema_name: None,
        host_pattern: Some(format!("{org_slug}.app.test")),
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

    let mut widget = Widget {
        id: Auto::default(),
        label: "marker_widget".into(),
    };
    widget.insert(&pool).await.unwrap();

    // Build the tenant admin — header-based resolver so the test
    // doesn't need DNS.
    let pools = Arc::new(TenantPools::new(pool.clone()));
    let resolver = HeaderResolver::default();
    let app = TenantAdminBuilder::new(pools, url.clone(), resolver)
        .show_only(["ten_admin_widget"])
        .build();

    let req = Request::builder()
        .uri("/ten_admin_widget")
        .header("x-org", &org_slug)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "list view should 200");
    let body = body_text(resp).await;
    assert!(
        body.contains("marker_widget"),
        "tenant data should render in admin: {body}"
    );

    rmig::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn no_tenant_match_returns_404() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let pools = Arc::new(TenantPools::new(pool.clone()));
    let app = TenantAdminBuilder::new(pools, url, HeaderResolver::default()).build();

    // No X-Org header → resolver returns None → 404.
    let req = Request::builder().uri("/").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    rmig::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn unknown_slug_in_header_returns_404() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let pools = Arc::new(TenantPools::new(pool.clone()));
    let app = TenantAdminBuilder::new(pools, url, HeaderResolver::default()).build();

    let req = Request::builder()
        .uri("/")
        .header("x-org", "ghost-tenant")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    rmig::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn schema_mode_admin_dispatches_with_search_path_set() {
    // Two schema-mode tenants. Each gets its own `ten_admin_widget`
    // table in its own schema. The admin URL with X-Org=acme returns
    // acme's data; X-Org=globex returns globex's. Browser-style
    // isolation by tenant.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();

    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let acme_schema = unique("admin_acme");
    let globex_schema = unique("admin_globex");
    drop_schema(&pool, &acme_schema).await;
    drop_schema(&pool, &globex_schema).await;
    sqlx::query(&format!(r#"CREATE SCHEMA "{acme_schema}""#))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!(r#"CREATE SCHEMA "{globex_schema}""#))
        .execute(&pool)
        .await
        .unwrap();

    // Per-tenant widget tables (mirroring what `migrate_tenants`
    // would do with a real Widget migration). Widget snapshot would
    // produce: id BIGSERIAL PK, label VARCHAR(64) NOT NULL.
    for schema in [&acme_schema, &globex_schema] {
        let sql = format!(
            r#"CREATE TABLE "{schema}"."ten_admin_widget" (
                "id" BIGSERIAL NOT NULL PRIMARY KEY,
                "label" VARCHAR(64) NOT NULL
            )"#
        );
        sqlx::query(&sql).execute(&pool).await.unwrap();
    }
    sqlx::query(&format!(
        r#"INSERT INTO "{acme_schema}"."ten_admin_widget" (label) VALUES ('acme_only_widget')"#
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        r#"INSERT INTO "{globex_schema}"."ten_admin_widget" (label) VALUES ('globex_only_widget')"#
    ))
    .execute(&pool)
    .await
    .unwrap();

    let acme_slug = unique("acme");
    let globex_slug = unique("globex");
    let mut acme_org = Org {
        id: Auto::default(),
        slug: acme_slug.clone(),
        display_name: "ACME".into(),
        storage_mode: StorageMode::Schema.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: None,
        schema_name: Some(acme_schema.clone()),
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
    acme_org.insert(&pool).await.unwrap();

    let mut globex_org = Org {
        id: Auto::default(),
        slug: globex_slug.clone(),
        display_name: "Globex".into(),
        storage_mode: StorageMode::Schema.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: None,
        schema_name: Some(globex_schema.clone()),
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
    globex_org.insert(&pool).await.unwrap();

    let pools = Arc::new(TenantPools::new(pool.clone()));
    let app = TenantAdminBuilder::new(pools, url.clone(), HeaderResolver::default())
        .show_only(["ten_admin_widget"])
        .build();

    // Acme view: must contain acme_only_widget, NOT globex_only_widget.
    let req = Request::builder()
        .uri("/ten_admin_widget")
        .header("x-org", &acme_slug)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_text(resp).await;
    assert!(
        body.contains("acme_only_widget"),
        "acme widget missing: {body}"
    );
    assert!(
        !body.contains("globex_only_widget"),
        "globex data leaked into acme view: {body}"
    );

    // Globex view: opposite.
    let req = Request::builder()
        .uri("/ten_admin_widget")
        .header("x-org", &globex_slug)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_text(resp).await;
    assert!(
        body.contains("globex_only_widget"),
        "globex widget missing: {body}"
    );
    assert!(
        !body.contains("acme_only_widget"),
        "acme data leaked into globex view: {body}"
    );

    drop_schema(&pool, &acme_schema).await;
    drop_schema(&pool, &globex_schema).await;
    rmig::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn subdomain_chain_resolves_via_host_header() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug = unique("subdomain_acme");
    let host = format!("{slug}.app.test");
    let mut org = Org {
        id: Auto::default(),
        slug: slug.clone(),
        display_name: "ACME".into(),
        storage_mode: StorageMode::Database.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: Some(url.clone()),
        schema_name: None,
        host_pattern: Some(host.clone()),
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

    let pools = Arc::new(TenantPools::new(pool.clone()));
    let resolver = ChainResolver::new()
        .push(SubdomainResolver::new("app.test"))
        .push(HeaderResolver::default());
    let app = TenantAdminBuilder::new(pools, url, resolver)
        .show_only(["rustango_orgs"])
        .build();

    let req = Request::builder()
        .uri("/")
        .header("host", &host)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "subdomain resolver should land on the tenant"
    );

    rmig::drop_all(&pool).await.unwrap();
}
