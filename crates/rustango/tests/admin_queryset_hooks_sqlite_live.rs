//! Django-parity #360 — `register_admin_queryset!` adds a
//! request-aware Filter contribution to the admin's list view
//! WHERE clause. End-to-end: register a hook that hides
//! `archived = true` rows, seed both kinds of rows, fetch the list
//! page, assert the archived row's title isn't present.

#![cfg(all(feature = "sqlite", feature = "admin", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use rustango::core::{Filter, Op, SqlValue};
use rustango::sql::Pool;
use rustango::Model;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "qh_post",
    display = "title",
    admin(list_display = "title, archived")
)]
#[allow(dead_code)]
pub struct QhPost {
    #[rustango(primary_key)]
    pub id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub archived: bool,
}

// Inventory-collected hook — hide archived rows from the admin
// list view. The hook receives request `Parts`; we ignore them
// here (the rule is unconditional). A real app might consult
// `parts.extensions` for a request-user struct + return per-user
// scoping predicates.
fn hide_archived(_parts: &axum::http::request::Parts) -> Vec<Filter> {
    vec![Filter {
        column: "archived",
        op: Op::Eq,
        value: SqlValue::Bool(false),
    }]
}
rustango::register_admin_queryset!("qh_post", hide_archived);

fn build_app(pool: Pool) -> axum::Router {
    rustango::admin::Builder::new(pool).admin_prefix("").build()
}

async fn fresh_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE qh_post (
            id       INTEGER PRIMARY KEY AUTOINCREMENT,
            title    TEXT NOT NULL,
            archived INTEGER NOT NULL
        )"#,
        Vec::new(),
    )
    .await
    .expect("create");
    rustango::sql::raw_execute_pool(
        &pool,
        "INSERT INTO qh_post (id, title, archived) VALUES \
         (1, 'Visible Post', 0), \
         (2, 'Archived Post', 1)",
        Vec::new(),
    )
    .await
    .expect("seed");
    pool
}

async fn fetch_body(pool: Pool, uri: &str) -> String {
    let app = build_app(pool);
    let resp = app
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(header::ACCEPT, "text/html")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn registered_hook_hides_filtered_rows_from_list_view() {
    let pool = fresh_pool().await;
    let body = fetch_body(pool, "/qh_post").await;
    // The visible row's title should be on the page.
    assert!(
        body.contains("Visible Post"),
        "visible row should appear in the list, got: {body}"
    );
    // The archived row's title must NOT appear — the hook's filter
    // is appended to the WHERE clause.
    assert!(
        !body.contains("Archived Post"),
        "archived row should be hidden by the queryset hook, got: {body}"
    );
}

#[tokio::test]
async fn hook_composes_with_per_field_filter_url_params() {
    // The URL already filters by `archived=true`, but the hook
    // ALSO filters `archived = false`. Two contradictory predicates
    // on the same column produce zero results — verifies the hook
    // ANDs with URL filters rather than overriding them.
    let pool = fresh_pool().await;
    let body = fetch_body(pool, "/qh_post?archived=true").await;
    // Neither row should pass both `archived=false` (hook) AND
    // `archived=true` (URL).
    assert!(
        !body.contains("Visible Post"),
        "visible row should be filtered out by URL param"
    );
    assert!(
        !body.contains("Archived Post"),
        "archived row should be filtered out by hook"
    );
}
