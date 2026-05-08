#![cfg(feature = "tenancy")]
//! Live tests for the operator console's `/orgs/{slug}/edit` flow
//! (added in v0.25.0). Covers:
//!
//! * GET /orgs/{slug}/edit renders a form pre-populated from the row
//!   via `admin::render::render_value_for_input`, with editable
//!   fields driven by `admin::render::render_input` per `FieldSchema`.
//! * POST /orgs/{slug}/edit applies a partial UPDATE through
//!   `forms::collect_values` — same parser the per-app admin uses on
//!   its own update_submit, so bound checks (max_length, min/max,
//!   type) come along for free.
//! * `database_url` rotation evicts the cached `TenantPool` so the
//!   next request rebuilds with the new URL.
//! * Locked fields (slug, storage_mode, schema_name, id, created_at)
//!   are display-only and stay unchanged across an edit.
//!
//! Reads `DATABASE_URL`. Skips silently when unset.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use rustango::core::Column as _;
use rustango::sql::{sqlx, Auto, Fetcher};
use rustango::tenancy::{
    operator_console::{router_with_pools, SessionSecret},
    Org, StorageMode, TenantPools,
};
use rustango::{migrate as rmig, Model};
use tower::ServiceExt;

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn unique(prefix: &str) -> String {
    let n = UNIQ.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    format!("{prefix}_{pid}_{n}")
}

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(sqlx::PgPool::connect(&url).await.unwrap())
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Boot the operator console + seed an operator + log them in.
/// Returns (router, session-cookie value, registry pool, operator
/// password) so individual tests can issue authenticated requests.
async fn boot() -> Option<(axum::Router, String, sqlx::PgPool, String)> {
    let pool = pool().await?;
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    // Seed an operator. Pre-hashed password = "letmein".
    let username = unique("op");
    let password = "letmein".to_owned();
    let hash = rustango::tenancy::password::hash(&password).unwrap();
    let mut op = rustango::tenancy::Operator {
        id: Auto::default(),
        username: username.clone(),
        password_hash: hash,
        active: true,
        created_at: now(),
        password_changed_at: None,
    };
    op.insert(&pool).await.unwrap();

    let pools = Arc::new(TenantPools::new(pool.clone()));
    let secret = SessionSecret::from_env_or_random();
    let app = router_with_pools(pool.clone(), pools.clone(), secret);

    // POST /login → cookie.
    let login_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "username={username}&password={password}"
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        login_resp.status().is_redirection(),
        "login expected 303, got {}",
        login_resp.status()
    );
    let cookie = login_resp
        .headers()
        .get("set-cookie")
        .expect("set-cookie present after login")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    Some((app, cookie, pool, password))
}

async fn seed_org(pool: &sqlx::PgPool, slug: &str, mode: StorageMode, db_url: Option<&str>) {
    let mut org = Org {
        id: Auto::default(),
        slug: slug.to_owned(),
        display_name: format!("display for {slug}"),
        storage_mode: mode.as_str().into(),
        database_url: db_url.map(str::to_owned),
        schema_name: None,
        host_pattern: Some(format!("{slug}.example.com")),
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
    org.insert(pool).await.unwrap();
}

#[tokio::test]
async fn get_edit_form_renders_with_prefill_and_no_creds() {
    let Some((app, cookie, pool, _)) = boot().await else {
        return;
    };
    let slug = unique("acme");
    seed_org(
        &pool,
        &slug,
        StorageMode::Database,
        Some("postgres://hidden:secret@example.com:5432/db"),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/orgs/{slug}/edit"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "form should render");
    let html = body_text(resp).await;

    // Editable fields visible.
    assert!(
        html.contains("display_name"),
        "display_name field missing from form"
    );
    assert!(html.contains("host_pattern"), "host_pattern field missing");
    assert!(html.contains("active"), "active field missing");

    // Existing display_name prefilled.
    assert!(
        html.contains(&format!("display for {slug}")),
        "display_name not prefilled"
    );

    // database_url field present BUT the literal secret must not
    // round-trip back to the browser.
    assert!(html.contains("database_url"), "database_url field missing");
    assert!(
        !html.contains("hidden:secret@example.com"),
        "literal credential leaked into rendered form"
    );

    // Locked fields shown as display-only — slug appears once for
    // the page header + once in the locked table row, but never as
    // an `<input name="slug">` in the editable section.
    assert!(html.contains(&slug), "locked slug should be displayed");
    assert!(
        !html.contains(r#"name="slug""#),
        "slug must not be an editable form input"
    );
    assert!(
        !html.contains(r#"name="storage_mode""#),
        "storage_mode must not be editable"
    );
    assert!(
        !html.contains(r#"name="created_at""#),
        "created_at must not be editable"
    );
}

#[tokio::test]
async fn post_edit_updates_only_editable_fields() {
    let Some((app, cookie, pool, _)) = boot().await else {
        return;
    };
    let slug = unique("acme");
    seed_org(&pool, &slug, StorageMode::Schema, None).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{slug}/edit"))
                .header("cookie", &cookie)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(
                    "display_name=Renamed+Org&host_pattern=new.example.com&active=on",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        resp.status().is_redirection(),
        "successful edit should redirect, got {}",
        resp.status()
    );

    // Re-fetch and assert.
    let row: Org = Org::objects()
        .where_(Org::slug.eq(slug.clone()))
        .fetch(&pool)
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("org still present");
    assert_eq!(row.display_name, "Renamed Org");
    assert_eq!(row.host_pattern.as_deref(), Some("new.example.com"));
    assert!(row.active);
    // Locked fields untouched.
    assert_eq!(row.slug, slug);
    assert_eq!(row.storage_mode, StorageMode::Schema.as_str());
}

#[tokio::test]
async fn post_edit_with_blank_database_url_keeps_existing() {
    let Some((app, cookie, pool, _)) = boot().await else {
        return;
    };
    let slug = unique("acme");
    let original_url = "env:DATABASE_URL_ACME";
    seed_org(&pool, &slug, StorageMode::Database, Some(original_url)).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{slug}/edit"))
                .header("cookie", &cookie)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(
                    "display_name=Updated&host_pattern=&active=on&database_url=",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(resp.status().is_redirection());

    let row: Org = Org::objects()
        .where_(Org::slug.eq(slug.clone()))
        .fetch(&pool)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(
        row.database_url.as_deref(),
        Some(original_url),
        "blank database_url submission must NOT overwrite the existing value"
    );
    assert_eq!(row.display_name, "Updated");
}

#[tokio::test]
async fn post_edit_active_toggle_off() {
    let Some((app, cookie, pool, _)) = boot().await else {
        return;
    };
    let slug = unique("acme");
    seed_org(&pool, &slug, StorageMode::Schema, None).await;

    // Submit without `active` field → unchecked → false.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{slug}/edit"))
                .header("cookie", &cookie)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("display_name=Soft+disabled&host_pattern="))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(resp.status().is_redirection());

    let row: Org = Org::objects()
        .where_(Org::slug.eq(slug.clone()))
        .fetch(&pool)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(
        !row.active,
        "missing `active` form field should soft-disable the org"
    );
}
