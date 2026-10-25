#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "admin"))]
//! Brand images must load where the page links them: tenant logos under
//! the default route preset, and the console's own `[brand] logo_path`.
//!
//! Env vars are process-global, so every test holds [`SUITE`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use rustango::sql::{sqlx, Auto};
use rustango::tenancy::admin::TenantAdminBuilder;
use rustango::tenancy::operator_console::{router_full, SessionSecret};
use rustango::tenancy::{branding, HeaderResolver, Org, TenantPools};
use tower::ServiceExt as _;

static SUITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static UNIQ: AtomicU64 = AtomicU64::new(0);

/// A real 1x1 PNG.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

const OP_ENV: &[&str] = &[
    "RUSTANGO_OPERATOR_LOGO_URL",
    "RUSTANGO_OPERATOR_LOGO_PATH",
    branding::MAX_BRAND_BYTES_ENV,
];

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}{}x{}",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::SeqCst)
    )
}

fn clear_op_env() {
    for v in OP_ENV {
        std::env::remove_var(v);
    }
}

async fn send(app: &axum::Router, host: &str, uri: &str) -> axum::response::Response {
    let req = Request::builder()
        .uri(uri)
        .header("host", host)
        .header("x-org", host.split('.').next().unwrap_or_default())
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}

async fn text(resp: axum::response::Response) -> String {
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The decoded value of `attr="…"` on the first tag that contains `marker`.
fn attr_after(html: &str, marker: &str, attr: &str) -> Option<String> {
    let tag = &html[html.find(marker)?..];
    let tag = &tag[..tag.find('>')?];
    let start = tag.find(&format!("{attr}=\""))? + attr.len() + 2;
    let raw = &tag[start..];
    let raw = &raw[..raw.find('"')?];
    Some(raw.replace("&#x2F;", "/").replace("&amp;", "&"))
}

struct Tenant {
    url: String,
    slug: String,
    pools: Arc<TenantPools<sqlx::Sqlite>>,
    pool: sqlx::SqlitePool,
    _tmp: tempfile::TempDir,
}

/// A migrated registry with one org whose logo and favicon are stored.
async fn tenant() -> Tenant {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::env::set_var(branding::BRAND_STORAGE_ROOT_ENV, tmp.path().join("brand"));
    let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
    let pool = sqlx::SqlitePool::connect(&url).await.expect("connect");
    let pools = Arc::new(TenantPools::<sqlx::Sqlite>::new(pool.clone()));
    let migrations = tmp.path().join("migrations");
    std::fs::create_dir_all(&migrations).unwrap();
    let mut buf: Vec<u8> = Vec::new();
    rustango::tenancy::manage::run_with_writer(
        pools.as_ref(),
        &url,
        &migrations,
        vec!["migrate-registry".to_owned()],
        &mut buf,
    )
    .await
    .expect("migrate-registry");

    let slug = unique("acme");
    let storage = branding::default_brand_storage();
    let logo = branding::save_brand_asset(
        &slug,
        branding::BrandAssetKind::Logo,
        PNG,
        Some("image/png"),
        &storage,
    )
    .await
    .expect("save logo");
    let favicon = branding::save_brand_asset(
        &slug,
        branding::BrandAssetKind::Favicon,
        PNG,
        Some("image/png"),
        &storage,
    )
    .await
    .expect("save favicon");
    let tenant_url = format!("sqlite://{}?mode=rwc", tmp.path().join("t.db").display());
    let mut org = Org {
        id: Auto::default(),
        slug: slug.clone(),
        display_name: slug.clone(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: Some(tenant_url),
        schema_name: None,
        host_pattern: Some(format!("{slug}.localhost")),
        port: None,
        path_prefix: None,
        active: true,
        created_at: chrono::Utc::now(),
        brand_name: None,
        brand_tagline: None,
        logo_path: Some(logo),
        favicon_path: Some(favicon),
        primary_color: None,
        theme_mode: None,
    };
    org.insert_pool(&pools.registry_pool())
        .await
        .expect("seed org");
    Tenant {
        url,
        slug,
        pools,
        pool,
        _tmp: tmp,
    }
}

/// GET `src` on `host`: it must be the stored PNG.
async fn assert_png(app: &axum::Router, host: &str, src: &str) {
    let resp = send(app, host, src).await;
    assert_eq!(resp.status(), StatusCode::OK, "GET {src} on {host}");
    let ct = resp.headers()[header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(ct, "image/png", "GET {src}");
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    assert_eq!(&bytes[..], PNG, "GET {src} body");
}

/// The tenant login page under the default route preset links a logo
/// and favicon the tenant host actually serves.
#[tokio::test]
async fn tenant_login_brand_images_load_under_default_routes() {
    let _g = SUITE.lock().await;
    let t = tenant().await;
    let app = rustango::server::Builder::from_pool(t.pool.clone(), t.url.clone(), "localhost")
        .into_router()
        .await
        .expect("assemble");
    let host = format!("{}.localhost", t.slug);

    let resp = send(&app, &host, "/login").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = text(resp).await;
    let logo = attr_after(&html, "class=\"brand-logo\"", "src").expect("logo img");
    let favicon = attr_after(&html, "rel=\"icon\"", "href").expect("favicon link");
    assert!(
        favicon.contains("favicon"),
        "uploaded favicon linked: {favicon}"
    );
    assert_png(&app, &host, &logo).await;
    assert_png(&app, &host, &favicon).await;
    std::env::remove_var(branding::BRAND_STORAGE_ROOT_ENV);
}

/// Tenant admin pages use the tenant favicon, not only the sign-in page.
#[tokio::test]
async fn tenant_admin_pages_use_the_tenant_favicon() {
    let _g = SUITE.lock().await;
    let t = tenant().await;
    let app =
        TenantAdminBuilder::new(t.pools.clone(), t.url.clone(), HeaderResolver::default()).build();
    let host = format!("{}.localhost", t.slug);

    let resp = send(&app, &host, "/").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = text(resp).await;
    let favicon = attr_after(&html, "rel=\"icon\"", "href").expect("favicon link");
    assert!(
        favicon.contains("favicon"),
        "tenant favicon linked: {favicon}"
    );
    assert_png(&app, &host, &favicon).await;
    std::env::remove_var(branding::BRAND_STORAGE_ROOT_ENV);
}

/// A `brand_url` with a trailing slash links and serves `/_brand/...`, not `//`.
#[tokio::test]
async fn brand_url_with_a_trailing_slash_still_serves() {
    let _g = SUITE.lock().await;
    let t = tenant().await;
    let routes = rustango::tenancy::RouteConfig {
        brand_url: "/_brand/".to_owned(),
        ..Default::default()
    };
    let app = TenantAdminBuilder::new(t.pools.clone(), t.url.clone(), HeaderResolver::default())
        .routes(routes)
        .build();
    let host = format!("{}.localhost", t.slug);

    let html = text(send(&app, &host, "/").await).await;
    let favicon = attr_after(&html, "rel=\"icon\"", "href").expect("favicon link");
    assert!(
        favicon.starts_with("/_brand/") && !favicon.contains("//"),
        "no `//`: {favicon}"
    );
    assert_png(&app, &host, &favicon).await;
    std::env::remove_var(branding::BRAND_STORAGE_ROOT_ENV);
}

const PREFIX: &str = "/ops";

/// The console, nested under [`PREFIX`], built after the env is set.
async fn console() -> (axum::Router, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
    let pool = sqlx::SqlitePool::connect(&url).await.expect("connect");
    let storage: rustango::storage::BoxedStorage = Arc::new(rustango::storage::LocalStorage::new(
        tmp.path().join("brand"),
    ));
    let console = router_full(
        pool,
        None,
        None,
        SessionSecret::from_env_or_random(),
        storage,
        None,
        "/_impersonation_handoff".into(),
    );
    (axum::Router::new().nest(PREFIX, console), tmp)
}

async fn console_logo(app: &axum::Router) -> String {
    let resp = send(app, "localhost", &format!("{PREFIX}/login")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    attr_after(&text(resp).await, "class=\"brand-logo\"", "src").expect("console logo img")
}

fn write_file(dir: &std::path::Path, name: &str, bytes: &[u8]) -> String {
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    p.display().to_string()
}

/// `logo_path` is read at boot and served by the console with a
/// versioned, long-cached URL under its mount prefix.
#[tokio::test]
async fn console_serves_a_local_logo_file() {
    let _g = SUITE.lock().await;
    clear_op_env();
    let files = tempfile::tempdir().unwrap();
    std::env::set_var(
        "RUSTANGO_OPERATOR_LOGO_PATH",
        write_file(files.path(), "acme.png", PNG),
    );
    let (app, _tmp) = console().await;
    clear_op_env();

    let src = console_logo(&app).await;
    assert!(
        src.starts_with(&format!("{PREFIX}/__static__/brand-logo.png?v=")),
        "local logo src: {src}"
    );
    let resp = send(&app, "localhost", &src).await;
    assert_eq!(resp.status(), StatusCode::OK, "GET {src}");
    assert_eq!(resp.headers()[header::CONTENT_TYPE], "image/png");
    let cache = resp.headers()[header::CACHE_CONTROL].to_str().unwrap();
    assert!(cache.contains("immutable"), "long cache: {cache}");
    let etag = resp.headers()[header::ETAG].clone();
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    assert_eq!(&bytes[..], PNG);

    let req = Request::builder()
        .uri(&src)
        .header(header::HOST, "localhost")
        .header(header::IF_NONE_MATCH, etag.clone())
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED, "revalidate {src}");
    assert_eq!(resp.headers()[header::ETAG], etag);
}

/// A file the tenant upload would refuse falls back to the bundled logo.
#[tokio::test]
async fn console_refuses_a_bad_logo_file() {
    let _g = SUITE.lock().await;
    let files = tempfile::tempdir().unwrap();
    let svg = write_file(files.path(), "acme.svg", b"<svg onload=alert(1)/>");
    let big = write_file(files.path(), "big.png", &[0_u8; 64]);
    let missing = files.path().join("nope.png").display().to_string();
    for (path, max) in [(svg, None), (big, Some("16")), (missing, None)] {
        clear_op_env();
        std::env::set_var("RUSTANGO_OPERATOR_LOGO_PATH", &path);
        if let Some(m) = max {
            std::env::set_var(branding::MAX_BRAND_BYTES_ENV, m);
        }
        let (app, _tmp) = console().await;
        clear_op_env();
        let src = console_logo(&app).await;
        assert_eq!(src, format!("{PREFIX}/__static__/rustango.png"), "{path}");
    }
}

/// Within one source `logo_url` beats `logo_path`.
#[tokio::test]
async fn console_logo_url_beats_logo_path() {
    let _g = SUITE.lock().await;
    clear_op_env();
    let files = tempfile::tempdir().unwrap();
    std::env::set_var(
        "RUSTANGO_OPERATOR_LOGO_PATH",
        write_file(files.path(), "acme.png", PNG),
    );
    std::env::set_var(
        "RUSTANGO_OPERATOR_LOGO_URL",
        "https://cdn.example.com/l.png",
    );
    let (app, _tmp) = console().await;
    clear_op_env();
    assert_eq!(console_logo(&app).await, "https://cdn.example.com/l.png");
}
