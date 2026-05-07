//! End-to-end branding flow tests — exercise the operator console
//! upload + serve routes and confirm the per-tenant CSS variable
//! override + logo `<img>` end up in the rendered admin sidebar.

#![cfg(feature = "tenancy")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::header;
use axum::http::Request;
use rustango::core::Column as _;
use rustango::sql::{sqlx, Auto, Fetcher};
use rustango::tenancy::admin::TenantAdminBuilder;
use rustango::tenancy::operator_console::{router_with_pools, SessionSecret};
use rustango::tenancy::{branding, HeaderResolver, Org, StorageMode, TenantPools};
use tokio::sync::Mutex;
use tower::ServiceExt;

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

/// Suite-wide lock so the brand-storage env var doesn't ricochet
/// between concurrent test bodies. cargo's harness runs tests in
/// parallel by default; the env vars our module reads are global.
fn live_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .ok()
}

#[tokio::test]
async fn build_brand_css_emits_safelisted_assignments() {
    // Pure-unit-scope smoke: no DB needed. The crate's own unit
    // tests cover this, but echoing it here makes the suite
    // self-explanatory when read in isolation.
    let css = branding::build_op_brand_css(Some("#2c5fb0")).expect("hex roundtrips");
    assert!(css.starts_with("--color-accent: #2c5fb0"), "got: {css}");
    assert!(css.contains("--color-accent-hover"));
    assert!(css.contains("--color-accent-bg-soft"));
    assert!(branding::build_op_brand_css(Some("javascript:alert(1)")).is_none());
}

#[tokio::test]
async fn brand_asset_url_is_safe_against_traversal() {
    let storage: rustango::storage::BoxedStorage =
        Arc::new(rustango::storage::InMemoryStorage::new());
    assert!(branding::brand_asset_url("acme", Some("../etc/passwd"), &storage).is_none());
    assert!(branding::brand_asset_url("../bad", Some("logo.png"), &storage).is_none());
    assert_eq!(
        branding::brand_asset_url("acme", Some("logo.png"), &storage).as_deref(),
        Some("/__brand__/acme/logo.png"),
    );
}

/// Storage backends that expose URLs via `Storage::url` — like S3
/// or `LocalStorage::with_base_url` — bypass the path-based handler.
/// This is the "S3 / R2 / B2 / MinIO" path the user wires for
/// production.
#[tokio::test]
async fn brand_asset_url_uses_direct_url_when_storage_has_one() {
    let storage: rustango::storage::BoxedStorage = Arc::new(
        rustango::storage::LocalStorage::new("/tmp/_rustango_brand_url_test".into())
            .with_base_url("https://cdn.example.com/brand"),
    );
    assert_eq!(
        branding::brand_asset_url("acme", Some("logo.png"), &storage).as_deref(),
        Some("https://cdn.example.com/brand/acme/logo.png"),
    );
}

#[tokio::test]
async fn upload_then_serve_round_trip() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    // Each test gets its own brand storage dir (cleaned up at exit
    // via `tempfile::TempDir`).
    let tmp = tempfile::tempdir().expect("tempdir");
    std::env::set_var(branding::BRAND_STORAGE_ROOT_ENV, tmp.path());
    rustango::migrate::drop_all(&pool).await.unwrap();
    rustango::migrate::apply_all(&pool).await.unwrap();

    // Seed an operator + an org. Pre-hashed password = "letmein".
    let username = format!("brand_op_{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let password = "letmein".to_owned();
    let hash = rustango::tenancy::password::hash(&password).unwrap();
    let mut op = rustango::tenancy::Operator {
        id: Auto::default(),
        username: username.clone(),
        password_hash: hash,
        active: true,
        created_at: now(),
    };
    op.insert(&pool).await.unwrap();

    let slug = format!("brand_acme_{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let mut org = Org {
        id: Auto::default(),
        slug: slug.clone(),
        display_name: "Acme Inc".into(),
        storage_mode: StorageMode::Schema.as_str().into(),
        database_url: None,
        schema_name: Some(slug.clone()),
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
    };
    org.insert(&pool).await.unwrap();

    let pools = Arc::new(TenantPools::new(pool.clone()));
    let secret = SessionSecret::from_env_or_random();
    let app = router_with_pools(pool.clone(), pools.clone(), secret);

    // POST /login → cookie. Required to authenticate the upload.
    let login_form = format!(
        "username={}&password={}&next=%2F",
        urlencoding::encode(&username),
        urlencoding::encode(&password),
    );
    let login_req = Request::builder()
        .method("POST")
        .uri("/login")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(login_form))
        .unwrap();
    let login_resp = app.clone().oneshot(login_req).await.unwrap();
    let cookie = login_resp
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| c.split(';').next())
        .map(str::to_owned)
        .expect("login should set a cookie");

    // Build a multipart body with a tiny PNG signature so the
    // content-type check accepts it. The branding module doesn't
    // re-validate magic bytes — it trusts the content-type — but
    // including the PNG header keeps the round-trip realistic.
    let png_bytes = b"\x89PNG\r\n\x1a\n_logo_payload";
    let boundary = "rustango-brand-test";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"logo\"; filename=\"logo.png\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
    body.extend_from_slice(png_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let upload_req = Request::builder()
        .method("POST")
        .uri(format!("/orgs/{slug}/edit/branding"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header(header::COOKIE, &cookie)
        .body(Body::from(body))
        .unwrap();
    let upload_resp = app.clone().oneshot(upload_req).await.unwrap();
    assert_eq!(
        upload_resp.status(),
        axum::http::StatusCode::SEE_OTHER,
        "upload should redirect to the edit page"
    );

    // Org row picked up the new logo_path.
    let updates: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(slug.clone()))
        .fetch(&pool)
        .await
        .unwrap();
    let updated = updates.into_iter().next().expect("org row exists");
    assert_eq!(
        updated.logo_path.as_deref(),
        Some("logo.png"),
        "Org.logo_path should be set after upload"
    );

    // Public serve route returns the bytes — cookie not needed
    // (branding is public-by-design; logos are cdn-style assets).
    let serve_req = Request::builder()
        .uri(format!("/__brand__/{slug}/logo.png"))
        .body(Body::empty())
        .unwrap();
    let serve_resp = app.clone().oneshot(serve_req).await.unwrap();
    assert_eq!(serve_resp.status(), axum::http::StatusCode::OK);
    assert_eq!(
        serve_resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("image/png"),
    );
    let served_bytes = axum::body::to_bytes(serve_resp.into_body(), 1 << 16)
        .await
        .unwrap();
    assert_eq!(&served_bytes[..], &png_bytes[..]);

    // Path traversal → 404.
    let bad_req = Request::builder()
        .uri(format!("/__brand__/{slug}/../etc/passwd"))
        .body(Body::empty())
        .unwrap();
    let bad_resp = app.clone().oneshot(bad_req).await.unwrap();
    assert_eq!(bad_resp.status(), axum::http::StatusCode::NOT_FOUND);

    // Cleanup.
    let _ = sqlx::query(r#"DELETE FROM "rustango_orgs" WHERE "slug" = $1"#)
        .bind(&slug)
        .execute(&pool)
        .await;
    let _ = sqlx::query(r#"DELETE FROM "rustango_operators" WHERE "username" = $1"#)
        .bind(&username)
        .execute(&pool)
        .await;
    std::env::remove_var(branding::BRAND_STORAGE_ROOT_ENV);
}

#[tokio::test]
async fn tenant_admin_renders_brand_overrides() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    let tmp = tempfile::tempdir().expect("tempdir");
    std::env::set_var(branding::BRAND_STORAGE_ROOT_ENV, tmp.path());

    rustango::migrate::drop_all(&pool).await.unwrap();
    rustango::migrate::apply_all(&pool).await.unwrap();

    // Seed an org with all brand fields populated.
    let slug = format!("brand_render_{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let mut org = Org {
        id: Auto::default(),
        slug: slug.clone(),
        display_name: "Acme Inc".into(),
        storage_mode: StorageMode::Database.as_str().into(),
        database_url: Some(url.clone()),
        schema_name: None,
        host_pattern: Some(format!("{slug}.app.test")),
        port: None,
        path_prefix: None,
        active: true,
        created_at: now(),
        brand_name: Some("Acme Branded".into()),
        brand_tagline: Some("Where work happens".into()),
        logo_path: Some("logo.png".into()),
        favicon_path: None,
        primary_color: Some("#2c5fb0".into()),
        theme_mode: Some("dark".into()),
    };
    org.insert(&pool).await.unwrap();

    let pools = Arc::new(TenantPools::new(pool.clone()));
    let resolver = HeaderResolver::default();
    let app = TenantAdminBuilder::new(pools, url.clone(), resolver).build();

    let req = Request::builder()
        .uri("/")
        .header("x-org", &slug)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body = String::from_utf8(
        axum::body::to_bytes(resp.into_body(), 1 << 18)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();

    // Brand name surfaces in the sidebar and breadcrumb.
    assert!(
        body.contains("Acme Branded"),
        "brand_name should render in admin: {body}"
    );
    // Logo URL ends up as an `<img>` source in the sidebar.
    assert!(
        body.contains(r#"src="/__brand__/"#),
        "logo URL should appear in admin: {body}"
    );
    // Tagline renders.
    assert!(
        body.contains("Where work happens"),
        "tagline should render: {body}"
    );
    // Theme mode flips data-theme.
    assert!(
        body.contains(r#"data-theme="dark""#),
        "theme_mode should set data-theme: {body}"
    );
    // Per-tenant CSS override block carries the accent color.
    assert!(
        body.contains("--color-accent: #2c5fb0"),
        "primary_color should drive --color-accent: {body}"
    );

    // Cleanup.
    let _ = sqlx::query(r#"DELETE FROM "rustango_orgs" WHERE "slug" = $1"#)
        .bind(&slug)
        .execute(&pool)
        .await;
    std::env::remove_var(branding::BRAND_STORAGE_ROOT_ENV);
}

