#![cfg(feature = "postgres")]
// Tests intentionally exercise the deprecated `ensure_tables(&PgPool)`
// path — that's what they're testing.
#![allow(deprecated)]
//! Live test for the v0.28 user-roles+permissions panel rendered on
//! the `rustango_users` admin detail page (Step 5 / item #76).
//!
//! Provisions one user, assigns them a role with two granted codenames
//! plus one direct denial, then GETs `/__admin/rustango_users/{id}` and
//! asserts the panel renders the role and the effective codenames.

#![cfg(feature = "tenancy")]

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use rustango::sql::sqlx;
use rustango::sql::Auto;
use rustango::tenancy::permissions::{
    assign_role, ensure_tables, get_or_create_role, grant_role_perm, set_user_perm,
};
use rustango::tenancy::User;
use tower::ServiceExt;

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
    sqlx::PgPool::connect(&url).await.ok()
}

async fn fresh(pool: &sqlx::PgPool) {
    rustango::migrate::drop_all(pool).await.unwrap();
    rustango::migrate::apply_all(pool).await.unwrap();
    for t in [
        "rustango_user_permissions",
        "rustango_user_roles",
        "rustango_role_permissions",
        "rustango_roles",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}" CASCADE"#))
            .execute(pool)
            .await
            .unwrap();
    }
    ensure_tables(pool).await.unwrap();
}

#[tokio::test]
async fn user_detail_page_renders_roles_and_effective_perms() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    let mut user = User {
        id: Auto::default(),
        username: format!(
            "panel_test_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ),
        password_hash: "x".into(),
        is_superuser: false,
        active: true,
        created_at: chrono::Utc::now(),
        data: serde_json::json!({}),
        password_changed_at: None,
    };
    user.insert(&pool).await.unwrap();
    let user_id = *user.id.get().expect("PK assigned");

    let role_id = get_or_create_role("editor_panel", "Edits content", &pool)
        .await
        .unwrap();

    grant_role_perm(role_id, "post.add", &pool).await.unwrap();
    grant_role_perm(role_id, "post.change", &pool)
        .await
        .unwrap();
    assign_role(user_id, role_id, &pool).await.unwrap();

    // Direct grant + direct denial — the panel should show the grant
    // and *not* the denied codename even though the role would grant it.
    set_user_perm(user_id, "comment.add", true, &pool)
        .await
        .unwrap();
    set_user_perm(user_id, "post.change", false, &pool)
        .await
        .unwrap();

    let app = rustango::admin::router(pool.clone());
    let req = Request::builder()
        .uri(format!("/rustango_users/{user_id}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1_000_000).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();

    // Panel header rendered.
    assert!(
        html.contains("Roles &amp; permissions"),
        "panel header missing: {html}"
    );
    // Role assignment shown.
    assert!(
        html.contains("editor_panel"),
        "role name missing from panel: {html}"
    );
    // Effective grants visible.
    assert!(
        html.contains("post.add"),
        "post.add codename missing from effective perms: {html}"
    );
    assert!(
        html.contains("comment.add"),
        "comment.add direct grant missing from effective perms: {html}"
    );
    // Denied codename suppressed even though role grants it.
    assert!(
        !html.contains(">post.change<"),
        "post.change should be suppressed by direct denial: {html}"
    );
}
