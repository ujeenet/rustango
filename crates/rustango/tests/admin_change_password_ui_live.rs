#![cfg(feature = "postgres")]
//! Live test for the v0.28.2 self-serve change-password page on the
//! tenant admin (`#77`). Covers anonymous-redirect-to-login plus the
//! happy-path POST that flips the stored hash.
//!
//! Reads `DATABASE_URL`. Skips silently when unset.

#![cfg(feature = "tenancy")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use rustango::migrate as rmig;
use rustango::sql::sqlx;
use rustango::sql::Auto;
use rustango::tenancy::tenant_console::{
    encode as encode_session, TenantSessionPayload, COOKIE_NAME,
};
use rustango::tenancy::{
    admin::TenantAdminBuilder, routes::RouteConfig, ChainResolver, Org, StorageMode,
    SubdomainResolver, TenantPools,
};
use tokio::sync::Mutex;
use tower::ServiceExt;

static UNIQ: AtomicU64 = AtomicU64::new(0);
fn unique(prefix: &str) -> String {
    let n = UNIQ.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    format!("{prefix}_{pid}_{n}")
}

/// Suite-wide lock. Every test in this file `drop_all + apply_all` on
/// the same database, which races under `cargo test`'s default parallel
/// harness ("relation rustango_users already exists" / pg_class
/// duplicate-key). Acquire this lock before touching the schema.
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

fn test_secret() -> rustango::tenancy::tenant_console::SessionSecret {
    rustango::tenancy::tenant_console::SessionSecret::from_bytes(b"a".repeat(32))
}

#[tokio::test]
async fn change_password_anonymous_redirects_to_login() {
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let _g = live_lock().lock().await;
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    // Provision a tenant in database mode pointing at the registry DB
    // (degenerate but sufficient for routing — same trick the existing
    // `database_mode_admin_serves_tenant_data` test uses).
    let slug = unique("chpw_anon");
    let host_pattern = format!("{slug}.app.test");
    let mut org = Org {
        id: Auto::default(),
        slug: slug.clone(),
        display_name: "Test".into(),
        storage_mode: StorageMode::Database.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: Some(url.clone()),
        schema_name: None,
        host_pattern: Some(host_pattern.clone()),
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
    let resolver = ChainResolver::new().push(SubdomainResolver::new("app.test"));
    let app = TenantAdminBuilder::new(pools, url.clone(), resolver)
        .routes(RouteConfig::legacy())
        .with_session(test_secret())
        .build();

    let res = app
        .oneshot(
            Request::builder()
                .uri("/__change-password")
                .header("Host", &host_pattern)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let loc = res
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        loc.starts_with("/__login"),
        "expected redirect to /__login, got `{loc}`"
    );

    rmig::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn change_password_authenticated_get_renders_form() {
    let Some(pool) = pool().await else {
        return;
    };
    let _g = live_lock().lock().await;
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug = unique("chpw_auth");
    let host_pattern = format!("{slug}.app.test");
    let mut org = Org {
        id: Auto::default(),
        slug: slug.clone(),
        display_name: "Test".into(),
        storage_mode: StorageMode::Database.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: Some(url.clone()),
        schema_name: None,
        host_pattern: Some(host_pattern.clone()),
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
    // Seed a user so validate_session can look it up.
    let hash = rustango::tenancy::password::hash("old-pw-secret").unwrap();
    let user_id: i64 = sqlx::query_scalar(
        "INSERT INTO rustango_users (username, password_hash, is_superuser, active, created_at) \
         VALUES ('alice', $1, TRUE, TRUE, NOW()) RETURNING id",
    )
    .bind(&hash)
    .fetch_one(&pool)
    .await
    .unwrap();

    let secret = test_secret();
    let pools = Arc::new(TenantPools::new(pool.clone()));
    let resolver = ChainResolver::new().push(SubdomainResolver::new("app.test"));
    let app = TenantAdminBuilder::new(pools, url.clone(), resolver)
        .routes(RouteConfig::legacy())
        .with_session(secret.clone())
        .build();

    let payload = TenantSessionPayload::new(user_id, &slug, 3600);
    let cookie = format!("{COOKIE_NAME}={}", encode_session(&secret, &payload));

    let res = app
        .oneshot(
            Request::builder()
                .uri("/__change-password")
                .header("Host", &host_pattern)
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1_000_000).await.unwrap();
    let html = String::from_utf8_lossy(&body);
    assert!(
        html.contains("Change password for"),
        "page header missing: {html}"
    );
    assert!(
        html.contains("name=\"current_password\""),
        "current_password input missing"
    );
    assert!(
        html.contains("name=\"new_password\""),
        "new_password input missing"
    );

    rmig::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn change_password_post_updates_stored_hash_when_current_matches() {
    let Some(pool) = pool().await else {
        return;
    };
    let _g = live_lock().lock().await;
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug = unique("chpw_post");
    let host_pattern = format!("{slug}.app.test");
    let mut org = Org {
        id: Auto::default(),
        slug: slug.clone(),
        display_name: "Test".into(),
        storage_mode: StorageMode::Database.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: Some(url.clone()),
        schema_name: None,
        host_pattern: Some(host_pattern.clone()),
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
    let hash = rustango::tenancy::password::hash("old-pw-secret").unwrap();
    let user_id: i64 = sqlx::query_scalar(
        "INSERT INTO rustango_users (username, password_hash, is_superuser, active, created_at) \
         VALUES ('alice', $1, TRUE, TRUE, NOW()) RETURNING id",
    )
    .bind(&hash)
    .fetch_one(&pool)
    .await
    .unwrap();

    let secret = test_secret();
    let pools = Arc::new(TenantPools::new(pool.clone()));
    let resolver = ChainResolver::new().push(SubdomainResolver::new("app.test"));
    let app = TenantAdminBuilder::new(pools, url.clone(), resolver)
        .routes(RouteConfig::legacy())
        .with_session(secret.clone())
        .build();

    let payload = TenantSessionPayload::new(user_id, &slug, 3600);
    let cookie = format!("{COOKIE_NAME}={}", encode_session(&secret, &payload));

    let body =
        "current_password=old-pw-secret&new_password=new-pw-secret&confirm_password=new-pw-secret";
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/__change-password")
                .header("Host", &host_pattern)
                .header(header::COOKIE, cookie.clone())
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let loc = res
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(loc.contains("ok=Password"), "got loc: {loc}");

    // The stored hash must verify against the new password and not
    // the old one.
    let stored: String =
        sqlx::query_scalar("SELECT password_hash FROM rustango_users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(rustango::tenancy::password::verify("new-pw-secret", &stored).unwrap());
    assert!(!rustango::tenancy::password::verify("old-pw-secret", &stored).unwrap());

    rmig::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn change_password_post_rejects_wrong_current() {
    let Some(pool) = pool().await else {
        return;
    };
    let _g = live_lock().lock().await;
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug = unique("chpw_rej");
    let host_pattern = format!("{slug}.app.test");
    let mut org = Org {
        id: Auto::default(),
        slug: slug.clone(),
        display_name: "Test".into(),
        storage_mode: StorageMode::Database.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: Some(url.clone()),
        schema_name: None,
        host_pattern: Some(host_pattern.clone()),
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
    let hash = rustango::tenancy::password::hash("real-current").unwrap();
    let user_id: i64 = sqlx::query_scalar(
        "INSERT INTO rustango_users (username, password_hash, is_superuser, active, created_at) \
         VALUES ('alice', $1, TRUE, TRUE, NOW()) RETURNING id",
    )
    .bind(&hash)
    .fetch_one(&pool)
    .await
    .unwrap();

    let secret = test_secret();
    let pools = Arc::new(TenantPools::new(pool.clone()));
    let resolver = ChainResolver::new().push(SubdomainResolver::new("app.test"));
    let app = TenantAdminBuilder::new(pools, url.clone(), resolver)
        .routes(RouteConfig::legacy())
        .with_session(secret.clone())
        .build();

    let payload = TenantSessionPayload::new(user_id, &slug, 3600);
    let cookie = format!("{COOKIE_NAME}={}", encode_session(&secret, &payload));

    let body =
        "current_password=wrong-pw&new_password=new-pw-secret&confirm_password=new-pw-secret";
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/__change-password")
                .header("Host", &host_pattern)
                .header(header::COOKIE, cookie)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let loc = res
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        loc.contains("error="),
        "expected error redirect, got: {loc}"
    );
    assert!(
        loc.contains("Current") && loc.contains("did") && loc.contains("match"),
        "expected mismatch error, got: {loc}"
    );

    // Stored hash must still be the original.
    let stored: String =
        sqlx::query_scalar("SELECT password_hash FROM rustango_users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(rustango::tenancy::password::verify("real-current", &stored).unwrap());

    rmig::drop_all(&pool).await.unwrap();
}

/// v0.28.4 (#77 follow-up) — sessions issued before the latest
/// password rotation must be rejected. Provision a user, mint a
/// cookie with a fixed (past) `iat`, stamp `password_changed_at`
/// to "now", then try to use the cookie. validate_session should
/// see `payload.iat < password_changed_at.timestamp()` and bounce
/// to login.
#[tokio::test]
async fn session_minted_before_password_rotation_is_rejected() {
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let _g = live_lock().lock().await;
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug = unique("chpw_pwd_at");
    let host_pattern = format!("{slug}.app.test");
    let mut org = Org {
        id: Auto::default(),
        slug: slug.clone(),
        display_name: "Test".into(),
        storage_mode: StorageMode::Database.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: Some(url.clone()),
        schema_name: None,
        host_pattern: Some(host_pattern.clone()),
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
    let hash = rustango::tenancy::password::hash("starting-pw").unwrap();
    let user_id: i64 = sqlx::query_scalar(
        "INSERT INTO rustango_users (username, password_hash, is_superuser, active, created_at) \
         VALUES ('alice', $1, TRUE, TRUE, NOW()) RETURNING id",
    )
    .bind(&hash)
    .fetch_one(&pool)
    .await
    .unwrap();

    let secret = test_secret();
    let pools = Arc::new(TenantPools::new(pool.clone()));
    let resolver = ChainResolver::new().push(SubdomainResolver::new("app.test"));
    let app = TenantAdminBuilder::new(pools, url.clone(), resolver)
        .routes(RouteConfig::legacy())
        .with_session(secret.clone())
        .build();

    // Mint a cookie *before* the password rotation. The fixed `iat`
    // (= now - 60s) lets us deterministically stamp
    // `password_changed_at` later in this test to a value strictly
    // greater than `iat`.
    let now_ts = chrono::Utc::now().timestamp();
    let mut payload = TenantSessionPayload::new(user_id, &slug, 3600);
    payload.iat = now_ts - 60;
    payload.exp = now_ts + 3600;
    let cookie = format!("{COOKIE_NAME}={}", encode_session(&secret, &payload));

    // Sanity: cookie works while password_changed_at is NULL.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/__change-password")
                .header("Host", &host_pattern)
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "pre-rotation cookie must work while password_changed_at IS NULL",
    );

    // Now simulate a rotation: stamp password_changed_at = NOW().
    sqlx::query("UPDATE rustango_users SET password_changed_at = NOW() WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();

    // Same cookie should now bounce to login.
    let res = app
        .oneshot(
            Request::builder()
                .uri("/__change-password")
                .header("Host", &host_pattern)
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::SEE_OTHER,
        "post-rotation cookie with stale iat must redirect",
    );
    let loc = res
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        loc.contains("/__login") || loc.contains("/login"),
        "expected redirect to login, got: {loc}",
    );

    rmig::drop_all(&pool).await.unwrap();
}
