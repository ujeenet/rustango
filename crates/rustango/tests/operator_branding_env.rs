#![cfg(feature = "postgres")]
//! Operator-console branding driven by env vars (no DB required).
//!
//! The operator console reads `RUSTANGO_OPERATOR_BRAND_NAME` /
//! `_TAGLINE` / `_LOGO_URL` / `_PRIMARY_COLOR` / `_THEME_MODE` at boot
//! and stamps them into every render context. These tests poke at the
//! login form (the only surface that doesn't require a DB lookup) so
//! the branding wiring is verified without standing up a registry.

#![cfg(feature = "tenancy")]

use std::sync::Mutex;

use axum::body::{to_bytes, Body};
use axum::http::Request;
use rustango::sql::sqlx;
use rustango::tenancy::operator_console::{router_with_pools, SessionSecret};
use std::sync::Arc;
use tower::ServiceExt;

/// Suite-wide lock — the operator console reads env vars at boot, so
/// each test must mutate them in isolation. Tokio's parallel test
/// runner would otherwise interleave `set_var` calls across these
/// bodies and the sibling branding_live tests.
fn env_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

const ENV_VARS: &[&str] = &[
    "RUSTANGO_OPERATOR_BRAND_NAME",
    "RUSTANGO_OPERATOR_TAGLINE",
    "RUSTANGO_OPERATOR_LOGO_URL",
    "RUSTANGO_OPERATOR_PRIMARY_COLOR",
    "RUSTANGO_OPERATOR_THEME_MODE",
];

fn clear_env() {
    for v in ENV_VARS {
        std::env::remove_var(v);
    }
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = to_bytes(resp.into_body(), 1 << 18).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[tokio::test]
async fn login_form_renders_default_brand_when_env_unset() {
    let _g = env_lock().lock().unwrap();
    clear_env();
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let secret = SessionSecret::from_env_or_random();
    let pools = Arc::new(rustango::tenancy::TenantPools::new(pool.clone()));
    let app = router_with_pools(pool, pools, secret);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_text(resp).await;

    assert!(
        body.contains("rustango"),
        "default brand name should render: {body}"
    );
    assert!(
        body.contains(r#"data-theme="auto""#),
        "default theme_mode is auto: {body}"
    );
    assert!(
        body.contains("/__static__/rustango.png"),
        "default logo URL should be the bundled PNG: {body}"
    );
}

#[tokio::test]
async fn login_form_picks_up_env_brand_name_and_tagline() {
    let _g = env_lock().lock().unwrap();
    clear_env();
    std::env::set_var("RUSTANGO_OPERATOR_BRAND_NAME", "Acme Operations");
    std::env::set_var("RUSTANGO_OPERATOR_TAGLINE", "Internal admin");
    let Some(pool) = pool().await else {
        clear_env();
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let secret = SessionSecret::from_env_or_random();
    let pools = Arc::new(rustango::tenancy::TenantPools::new(pool.clone()));
    let app = router_with_pools(pool, pools, secret);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_text(resp).await;
    clear_env();

    assert!(
        body.contains("Acme Operations"),
        "env brand_name should render: {body}"
    );
    assert!(
        body.contains("Internal admin"),
        "env brand tagline should render: {body}"
    );
}

#[tokio::test]
async fn login_form_picks_up_env_primary_color() {
    let _g = env_lock().lock().unwrap();
    clear_env();
    std::env::set_var("RUSTANGO_OPERATOR_PRIMARY_COLOR", "#2c5fb0");
    let Some(pool) = pool().await else {
        clear_env();
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let secret = SessionSecret::from_env_or_random();
    let pools = Arc::new(rustango::tenancy::TenantPools::new(pool.clone()));
    let app = router_with_pools(pool, pools, secret);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_text(resp).await;
    clear_env();

    assert!(
        body.contains("--color-accent: #2c5fb0"),
        "primary_color should drive --color-accent override: {body}"
    );
}

#[tokio::test]
async fn login_form_picks_up_env_theme_mode_dark() {
    let _g = env_lock().lock().unwrap();
    clear_env();
    std::env::set_var("RUSTANGO_OPERATOR_THEME_MODE", "dark");
    let Some(pool) = pool().await else {
        clear_env();
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let secret = SessionSecret::from_env_or_random();
    let pools = Arc::new(rustango::tenancy::TenantPools::new(pool.clone()));
    let app = router_with_pools(pool, pools, secret);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_text(resp).await;
    clear_env();

    assert!(
        body.contains(r#"data-theme="dark""#),
        "env theme_mode=dark should set data-theme: {body}"
    );
}

#[tokio::test]
async fn login_form_rejects_invalid_env_primary_color() {
    let _g = env_lock().lock().unwrap();
    clear_env();
    // `javascript:alert(1)` doesn't match the hex shape — must be
    // dropped, not interpolated into the output.
    std::env::set_var("RUSTANGO_OPERATOR_PRIMARY_COLOR", "javascript:alert(1)");
    let Some(pool) = pool().await else {
        clear_env();
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let secret = SessionSecret::from_env_or_random();
    let pools = Arc::new(rustango::tenancy::TenantPools::new(pool.clone()));
    let app = router_with_pools(pool, pools, secret);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_text(resp).await;
    clear_env();

    assert!(
        !body.contains("javascript:"),
        "malformed primary_color must NOT reach the rendered HTML: {body}"
    );
}

#[tokio::test]
async fn login_form_rejects_invalid_env_theme_mode() {
    let _g = env_lock().lock().unwrap();
    clear_env();
    std::env::set_var("RUSTANGO_OPERATOR_THEME_MODE", "solarized-pony");
    let Some(pool) = pool().await else {
        clear_env();
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let secret = SessionSecret::from_env_or_random();
    let pools = Arc::new(rustango::tenancy::TenantPools::new(pool.clone()));
    let app = router_with_pools(pool, pools, secret);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_text(resp).await;
    clear_env();

    assert!(
        !body.contains("solarized-pony"),
        "unknown theme_mode value must be rejected, not echoed: {body}"
    );
    assert!(
        body.contains(r#"data-theme="auto""#),
        "unknown theme_mode falls back to auto: {body}"
    );
}
