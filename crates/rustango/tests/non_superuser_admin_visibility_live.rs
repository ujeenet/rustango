//! Live integration test for #67 — the "scaffold an app, see it in
//! admin as a non-superuser" loop that #61–#66 collectively broke.
//!
//! What this catches end-to-end:
//!   - #61: `auto_create_permissions` actually runs (we invoke it
//!     here and assert the seeded codenames exist in
//!     `rustango_permissions`).
//!   - #62: framework models with the default `permissions = true`
//!     produce CRUD codenames so a non-superuser can be granted
//!     `<table>.view` and the admin renders them.
//!   - The full visibility chain: `admin::Builder::with_user_perms`
//!     filters the sidebar exactly to the codenames that were
//!     granted. Models the user can't view stay hidden.
//!
//! Activated when `DATABASE_URL` is set; skips silently otherwise.

#![cfg(feature = "tenancy")]

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use rustango::sql::sqlx;
use rustango::sql::Auto;
use rustango::tenancy::permissions::{
    assign_role, auto_create_permissions, ensure_tables, get_or_create_role, grant_role_perm,
    user_permissions,
};
use rustango::tenancy::User;
use tower::ServiceExt;

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
        "rustango_permissions",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}" CASCADE"#))
            .execute(pool)
            .await
            .unwrap();
    }
    ensure_tables(pool).await.unwrap();
}

/// `auto_create_permissions` walks the inventory of registered models
/// and seeds the four CRUD codenames (`{table}.add/change/delete/view`)
/// for every model with the default `permissions = true`. After
/// running it, a known-permissions-enabled framework model
/// (`rustango_users`) must have its codenames in
/// `rustango_permissions`.
#[tokio::test]
async fn auto_create_permissions_seeds_codenames_for_default_models() {
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    auto_create_permissions(&pool).await.unwrap();

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM rustango_permissions WHERE codename = $1")
            .bind("rustango_users.view")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        count, 1,
        "expected `rustango_users.view` to exist after auto_create_permissions"
    );
    // Sanity: all four CRUD verbs land for the same model.
    for action in ["add", "change", "delete", "view"] {
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM rustango_permissions WHERE codename = $1")
                .bind(format!("rustango_users.{action}"))
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            n, 1,
            "expected `rustango_users.{action}` codename after auto_create_permissions"
        );
    }
}

/// End-to-end: a non-superuser whose only granted codename is
/// `rustango_users.view` sees the User model in the sidebar / index
/// AND can hit its list page; models they have no view perm for
/// (e.g. `rustango_roles`) are hidden + 404 on direct access.
///
/// Catches #62 (default `permissions = true`): if a framework model
/// somehow lacked the flag, no codename would exist for it, and a
/// non-superuser could never be granted view — they'd see an empty
/// sidebar regardless of how many roles they were assigned. This
/// test would fail loudly in that case.
#[tokio::test]
async fn non_superuser_with_view_perm_sees_model_in_admin() {
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    auto_create_permissions(&pool).await.unwrap();

    // Provision a non-superuser.
    let mut user = User {
        id: Auto::default(),
        username: format!(
            "vis_test_{}",
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

    // Role grants `rustango_users.view` only — explicitly NOT
    // `rustango_roles.view`, so we can assert the sidebar
    // filtering really does exclude unviewable models.
    let role_id = get_or_create_role("user_viewer", "Can read users", &pool)
        .await
        .unwrap();
    grant_role_perm(role_id, "rustango_users.view", &pool)
        .await
        .unwrap();
    assign_role(user_id, role_id, &pool).await.unwrap();

    let perms = user_permissions(user_id, &pool).await.unwrap();
    assert!(
        perms.iter().any(|p| p == "rustango_users.view"),
        "user should have rustango_users.view in effective set; got {perms:?}"
    );
    assert!(
        !perms.iter().any(|p| p == "rustango_roles.view"),
        "user should NOT have rustango_roles.view; got {perms:?}"
    );

    // Build the admin router scoped to this user's perms.
    let app = rustango::admin::Builder::new(pool.clone())
        .with_user_perms(perms.iter().cloned())
        .build();

    // GET admin index → must contain User table link, must NOT
    // contain Role table link.
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1_000_000).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();

    assert!(
        html.contains("rustango_users"),
        "non-superuser with rustango_users.view should see the User model in the index: {html}"
    );
    // The Role model link should NOT appear because we didn't grant
    // `rustango_roles.view` and the user isn't a superuser.
    assert!(
        !html.contains("href=\"/rustango_roles\""),
        "non-superuser without rustango_roles.view should NOT see Role link in the index: {html}"
    );

    // Direct hit on the allowed table → 200.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/rustango_users")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "GET /rustango_users should succeed for a user holding rustango_users.view"
    );

    // Direct hit on a not-viewable table → 404 (NOT 500). The
    // user shouldn't even know `rustango_roles` exists from this
    // perspective.
    let res = app
        .oneshot(
            Request::builder()
                .uri("/rustango_roles")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::NOT_FOUND,
        "GET /rustango_roles should 404 for a user without rustango_roles.view"
    );
}
