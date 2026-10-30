#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "admin"))]
//! The tenant sign-in page names the tenant by its display name, and its
//! field follows `[auth] login_field`.

use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use rustango::sql::{sqlx, Auto};
use rustango::tenancy::{Org, TenantPools};
use tower::ServiceExt as _;

static UNIQ: AtomicU64 = AtomicU64::new(0);

struct Registry {
    url: String,
    pool: sqlx::SqlitePool,
    pools: TenantPools<sqlx::Sqlite>,
    tmp: tempfile::TempDir,
}

async fn registry() -> Registry {
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
    let pool = sqlx::SqlitePool::connect(&url).await.expect("connect");
    let pools = TenantPools::<sqlx::Sqlite>::new(pool.clone());
    let migrations = tmp.path().join("migrations");
    std::fs::create_dir_all(&migrations).unwrap();
    let mut buf: Vec<u8> = Vec::new();
    rustango::tenancy::manage::run_with_writer(
        &pools,
        &url,
        &migrations,
        vec!["migrate-registry".to_owned()],
        &mut buf,
    )
    .await
    .expect("migrate-registry");
    Registry {
        url,
        pool,
        pools,
        tmp,
    }
}

/// Add an org and return its slug.
async fn org(reg: &Registry, display_name: &str) -> String {
    let slug = format!(
        "shu{}x{}",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::SeqCst)
    );
    let tenant_url = format!(
        "sqlite://{}?mode=rwc",
        reg.tmp.path().join(format!("{slug}.db")).display()
    );
    let mut org = Org {
        id: Auto::default(),
        slug: slug.clone(),
        display_name: display_name.to_owned(),
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
        logo_path: None,
        favicon_path: None,
        primary_color: None,
        theme_mode: None,
    };
    org.insert_pool(&reg.pools.registry_pool())
        .await
        .expect("seed org");
    slug
}

async fn sign_in_page(reg: &Registry, slug: &str) -> String {
    let app = rustango::server::Builder::from_pool(reg.pool.clone(), reg.url.clone(), "localhost")
        .into_router()
        .await
        .expect("assemble");
    let req = Request::builder()
        .uri("/login")
        .header("host", format!("{slug}.localhost"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[tokio::test]
async fn the_page_names_the_display_name_not_the_slug() {
    let reg = registry().await;
    let slug = org(&reg, "Sandhill University Foundation").await;
    let html = sign_in_page(&reg, &slug).await;
    assert!(
        html.contains("Sign in to <strong>Sandhill University Foundation</strong>"),
        "{html}"
    );
    assert!(
        !html.contains(&format!("<strong>{slug}</strong>")),
        "{html}"
    );
}

#[tokio::test]
async fn an_empty_display_name_falls_back_to_the_slug() {
    let reg = registry().await;
    let slug = org(&reg, "  ").await;
    let html = sign_in_page(&reg, &slug).await;
    assert!(
        html.contains(&format!("Sign in to <strong>{slug}</strong>")),
        "{html}"
    );
}

#[cfg(feature = "sso")]
#[tokio::test]
async fn the_field_follows_login_field() {
    use rustango::login_field::LoginField;
    rustango::tenancy::configure_login_field(LoginField::Email);
    let reg = registry().await;
    let slug = org(&reg, "Acme").await;
    let html = sign_in_page(&reg, &slug).await;
    assert!(
        html.contains(r#"<label for="username">Email</label>"#),
        "{html}"
    );
    assert!(html.contains(r#"type="email" id="username""#), "{html}");
}
