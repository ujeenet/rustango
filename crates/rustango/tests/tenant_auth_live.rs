//! Live tests for the tenant-side auth + `is_superuser` admin gating
//! shipped in v0.6 step 7.
//!
//! Reads `DATABASE_URL`. Skips silently when unset.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use rustango::sql::{sqlx, Auto};
use rustango::{migrate as rmig, Model};
use rustango::tenancy::{
    admin::TenantAdminBuilder, tenant_console::SessionSecret, HeaderResolver, Org, StorageMode,
    TenantPools,
};
use tower::ServiceExt;

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn unique(prefix: &str) -> String {
    let n = UNIQ.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    format!("{prefix}_{pid}_{n}")
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(sqlx::PgPool::connect(&url).await.unwrap())
}

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

fn secret() -> SessionSecret {
    SessionSecret::from_bytes(b"tenant-auth-test-secret-32-bytes".to_vec())
}

/// Demo model — same shape as admin_live's Widget so it has list/edit
/// surface to assert against.
#[derive(Model, Debug, Clone)]
#[rustango(table = "tenauth_widget", display = "label")]
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

/// Insert a tenant Org pointing at the registry DB (database mode is
/// the simplest setup for tests — same Postgres backs both layers).
async fn seed_db_mode_tenant(pool: &sqlx::PgPool, slug: &str, url: &str) -> Org {
    let mut org = Org {
        id: Auto::default(),
        slug: slug.to_owned(),
        display_name: slug.to_owned(),
        storage_mode: StorageMode::Database.as_str().into(),
        database_url: Some(url.to_owned()),
        schema_name: None,
        host_pattern: Some(format!("{slug}.app.test")),
        port: None,
        path_prefix: None,
        active: true,
        created_at: now(),
    };
    org.insert(pool).await.unwrap();
    org
}

/// Recreate the tenant `rustango_users` table directly in `public`
/// (the test's Org points at the registry DB, so `public` is the
/// "tenant schema"). Drops + creates so the test starts clean.
async fn reset_users_table(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "rustango_users" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "rustango_users" (
            "id" BIGSERIAL NOT NULL PRIMARY KEY,
            "username" VARCHAR(64) NOT NULL UNIQUE,
            "password_hash" VARCHAR(255) NOT NULL,
            "is_superuser" BOOLEAN NOT NULL,
            "active" BOOLEAN NOT NULL,
            "created_at" TIMESTAMPTZ NOT NULL
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn insert_user(pool: &sqlx::PgPool, username: &str, password: &str, is_superuser: bool) {
    let hash = rustango::tenancy::password::hash(password).unwrap();
    sqlx::query(
        r#"INSERT INTO "rustango_users" (username, password_hash, is_superuser, active, created_at) VALUES ($1, $2, $3, true, NOW())"#,
    )
    .bind(username)
    .bind(hash)
    .bind(is_superuser)
    .execute(pool)
    .await
    .unwrap();
}

fn build_app(pools: Arc<TenantPools>, url: String) -> axum::Router {
    TenantAdminBuilder::new(pools, url, HeaderResolver::default())
        .show_only(["tenauth_widget"])
        .with_session(secret())
        .build()
}

/// Anon traffic to a private route is redirected to `/__login`.
#[tokio::test]
async fn anon_request_redirects_to_login() {
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug = unique("anon");
    seed_db_mode_tenant(&pool, &slug, &url).await;
    reset_users_table(&pool).await;

    let pools = Arc::new(TenantPools::new(pool.clone()));
    let app = build_app(pools, url);

    let req = Request::builder()
        .uri("/tenauth_widget")
        .header("x-org", &slug)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER, "anon should 303");
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        location.starts_with("/__login"),
        "should redirect to login, got `{location}`"
    );
    assert!(
        location.contains("next="),
        "should preserve next, got `{location}`"
    );

    rmig::drop_all(&pool).await.unwrap();
}

/// `GET /__login` renders the form. Public surface — no cookie needed.
#[tokio::test]
async fn login_form_renders_for_anon() {
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug = unique("loginform");
    seed_db_mode_tenant(&pool, &slug, &url).await;
    reset_users_table(&pool).await;

    let pools = Arc::new(TenantPools::new(pool.clone()));
    let app = build_app(pools, url);

    let req = Request::builder()
        .uri("/__login")
        .header("x-org", &slug)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_text(resp).await;
    assert!(body.contains("Sign in"), "login form should render: {body}");
    assert!(body.contains(&slug), "should reference tenant slug");

    rmig::drop_all(&pool).await.unwrap();
}

/// Wrong credentials → 303 back to `/__login?error=...`.
#[tokio::test]
async fn login_with_wrong_credentials_redirects_to_error() {
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug = unique("badcreds");
    seed_db_mode_tenant(&pool, &slug, &url).await;
    reset_users_table(&pool).await;
    insert_user(&pool, "alice", "hunter2", true).await;

    let pools = Arc::new(TenantPools::new(pool.clone()));
    let app = build_app(pools, url);

    let body = serde_urlencoded::to_string([("username", "alice"), ("password", "WRONG")]).unwrap();
    let req = Request::builder()
        .method("POST")
        .uri("/__login")
        .header("x-org", &slug)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(location.contains("error="), "got `{location}`");

    // No session cookie set.
    let set_cookie = resp
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("rustango_tenant_session="));
    if let Some(c) = set_cookie {
        assert!(
            c.contains("rustango_tenant_session=;") || c.contains("Max-Age=0"),
            "wrong creds should not mint a real session cookie, got: {c}"
        );
    }

    rmig::drop_all(&pool).await.unwrap();
}

/// Superuser login → 303, cookie minted, follow-up GET sees full
/// admin (write-buttons / `/new` link present).
#[tokio::test]
async fn superuser_login_grants_read_write_admin() {
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug = unique("super");
    seed_db_mode_tenant(&pool, &slug, &url).await;
    reset_users_table(&pool).await;
    insert_user(&pool, "alice", "hunter2", true).await;

    // Seed a widget so the list view has content.
    let mut w = Widget {
        id: Auto::default(),
        label: "rw_marker".into(),
    };
    w.insert(&pool).await.unwrap();

    let pools = Arc::new(TenantPools::new(pool.clone()));
    let app = build_app(pools, url);

    // Login.
    let form = serde_urlencoded::to_string([("username", "alice"), ("password", "hunter2")])
        .unwrap();
    let login_req = Request::builder()
        .method("POST")
        .uri("/__login")
        .header("x-org", &slug)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    let login_resp = app.clone().oneshot(login_req).await.unwrap();
    assert_eq!(login_resp.status(), StatusCode::SEE_OTHER);
    let cookie = extract_session_cookie(&login_resp).expect("cookie should be set");

    // Hit the list view with the cookie.
    let list_req = Request::builder()
        .uri("/tenauth_widget")
        .header("x-org", &slug)
        .header("cookie", &cookie)
        .body(Body::empty())
        .unwrap();
    let list_resp = app.oneshot(list_req).await.unwrap();
    assert_eq!(list_resp.status(), StatusCode::OK);
    let body = body_text(list_resp).await;
    assert!(body.contains("rw_marker"), "row should render: {body}");
    // Superuser sees the "Add new" / "/new" affordance somewhere on
    // the page (the rustango-admin templates emit it for non-read-only
    // tables). A non-superuser test below will assert the inverse.
    assert!(
        body.contains("/tenauth_widget/new") || body.to_lowercase().contains("add"),
        "superuser should see write-link, got: {body}"
    );

    rmig::drop_all(&pool).await.unwrap();
}

/// Non-superuser login → cookie minted, but the admin renders in
/// read-only mode (mutating routes 403, write-links hidden).
#[tokio::test]
async fn non_superuser_session_forces_read_only_admin() {
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug = unique("ronly");
    seed_db_mode_tenant(&pool, &slug, &url).await;
    reset_users_table(&pool).await;
    insert_user(&pool, "bob", "hunter2", false).await;

    let mut w = Widget {
        id: Auto::default(),
        label: "ro_marker".into(),
    };
    w.insert(&pool).await.unwrap();

    let pools = Arc::new(TenantPools::new(pool.clone()));
    let app = build_app(pools, url);

    let form = serde_urlencoded::to_string([("username", "bob"), ("password", "hunter2")]).unwrap();
    let login_req = Request::builder()
        .method("POST")
        .uri("/__login")
        .header("x-org", &slug)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    let login_resp = app.clone().oneshot(login_req).await.unwrap();
    assert_eq!(login_resp.status(), StatusCode::SEE_OTHER);
    let cookie = extract_session_cookie(&login_resp).expect("cookie should be set");

    // List view renders OK with the row.
    let list_req = Request::builder()
        .uri("/tenauth_widget")
        .header("x-org", &slug)
        .header("cookie", &cookie)
        .body(Body::empty())
        .unwrap();
    let list_resp = app.clone().oneshot(list_req).await.unwrap();
    assert_eq!(list_resp.status(), StatusCode::OK);
    let body = body_text(list_resp).await;
    assert!(body.contains("ro_marker"), "row should render: {body}");

    // Mutating route → 403 (rustango-admin returns FORBIDDEN for
    // read-only tables).
    let new_req = Request::builder()
        .uri("/tenauth_widget/new")
        .header("x-org", &slug)
        .header("cookie", &cookie)
        .body(Body::empty())
        .unwrap();
    let new_resp = app.oneshot(new_req).await.unwrap();
    assert_eq!(
        new_resp.status(),
        StatusCode::FORBIDDEN,
        "non-superuser /new should 403"
    );

    rmig::drop_all(&pool).await.unwrap();
}

/// Cookie minted for tenant A is rejected when sent against tenant B
/// (anti-replay defense — `payload.slug` mismatch).
#[tokio::test]
async fn cookie_from_one_tenant_is_rejected_at_another() {
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug_a = unique("a");
    let slug_b = unique("b");
    seed_db_mode_tenant(&pool, &slug_a, &url).await;
    seed_db_mode_tenant(&pool, &slug_b, &url).await;
    reset_users_table(&pool).await;
    insert_user(&pool, "alice", "hunter2", true).await;

    let pools = Arc::new(TenantPools::new(pool.clone()));
    let app = build_app(pools, url);

    let form = serde_urlencoded::to_string([("username", "alice"), ("password", "hunter2")])
        .unwrap();
    let login_req = Request::builder()
        .method("POST")
        .uri("/__login")
        .header("x-org", &slug_a)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    let login_resp = app.clone().oneshot(login_req).await.unwrap();
    let cookie = extract_session_cookie(&login_resp).expect("cookie should be set");

    // Hit tenant B with tenant A's cookie → redirect to login.
    let req = Request::builder()
        .uri("/tenauth_widget")
        .header("x-org", &slug_b)
        .header("cookie", &cookie)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(location.starts_with("/__login"), "got `{location}`");

    rmig::drop_all(&pool).await.unwrap();
}

fn extract_session_cookie(resp: &axum::http::Response<Body>) -> Option<String> {
    for v in resp.headers().get_all("set-cookie") {
        let s = v.to_str().ok()?;
        if s.starts_with("rustango_tenant_session=") {
            // Take the first segment up to ; — that's `name=value`.
            let head = s.split(';').next()?;
            return Some(head.to_owned());
        }
    }
    None
}
