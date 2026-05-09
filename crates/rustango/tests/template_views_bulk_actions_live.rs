#![cfg(all(feature = "template_views", feature = "tenancy"))]
//! Live end-to-end test for `ListView::bulk_actions` (#80, v0.30.4).
//!
//! Mounts a `ListView` with bulk_actions enabled + a custom action,
//! seeds a few rows, then POSTs `_selected_action=...` form data
//! and asserts:
//! 1. Built-in `delete_selected` actually deletes the right rows.
//! 2. A user-registered action runs against the per-request pool.
//! 3. `303 See Other` redirect → fresh GET shows the post-action
//!    state.
//! 4. Empty selection / unknown action → 400.
//!
//! Reads `DATABASE_URL`. Skips silently when unset.
//!
//! Run: `DATABASE_URL=... cargo test --test template_views_bulk_actions_live`

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use rustango::core::Model as _;
use rustango::sql::{sqlx, Auto};
use rustango::template_views::{BulkActionFn, ListView};
use rustango::Model;
use tera::Tera;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(table = "tv_bulk_widget", display = "label")]
#[allow(dead_code)]
pub struct Widget {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub label: String,
    pub published: bool,
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(sqlx::PgPool::connect(&url).await.unwrap())
}

async fn fresh_table(pool: &sqlx::PgPool) {
    sqlx::query("DROP TABLE IF EXISTS tv_bulk_widget CASCADE")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE tv_bulk_widget (
            id BIGSERIAL PRIMARY KEY,
            label VARCHAR(64) NOT NULL,
            published BOOLEAN NOT NULL DEFAULT FALSE
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

fn tera() -> Arc<Tera> {
    let mut t = Tera::default();
    // Minimal template — stamp out a marker the smoke tests don't
    // actually rely on. The list endpoint runs SQL regardless of
    // template body, so a one-line template is fine.
    t.add_raw_template(
        "tv_bulk_widget_list.html",
        "rows={{ object_list | length }} actions={{ bulk_actions | length }}",
    )
    .unwrap();
    Arc::new(t)
}

async fn seed_three(pool: &sqlx::PgPool) -> Vec<i64> {
    let mut ids = Vec::new();
    for label in ["alpha", "beta", "gamma"] {
        let id: i64 =
            sqlx::query_scalar("INSERT INTO tv_bulk_widget (label) VALUES ($1) RETURNING id")
                .bind(label)
                .fetch_one(pool)
                .await
                .unwrap();
        ids.push(id);
    }
    ids
}

fn build_app(pool: sqlx::PgPool) -> axum::Router {
    let publish_handler: BulkActionFn = Arc::new(|pool, pks| {
        let pool = pool.clone();
        let pks = pks.to_vec();
        Box::pin(async move {
            // Translate SqlValue::I64 back to bind values; the
            // framework guarantees they're typed correctly per the
            // schema's PK type.
            let ids: Vec<i64> = pks
                .iter()
                .filter_map(|v| match v {
                    rustango::core::SqlValue::I64(n) => Some(*n),
                    _ => None,
                })
                .collect();
            sqlx::query("UPDATE tv_bulk_widget SET published = TRUE WHERE id = ANY($1)")
                .bind(&ids)
                .execute(&pool)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    });

    let lv = ListView::for_model(Widget::SCHEMA)
        .page_size(20)
        .bulk_actions(true)
        .action("publish_selected", "Publish selected", publish_handler);

    lv.router("/widgets", tera(), pool)
}

fn post_form(uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn delete_selected_built_in_runs_against_picked_rows() {
    let Some(pool) = pool().await else { return };
    fresh_table(&pool).await;
    let ids = seed_three(&pool).await;

    let app = build_app(pool.clone());
    // Select first two; leave the third.
    let body = format!(
        "action=delete_selected&_selected_action={}&_selected_action={}",
        ids[0], ids[1]
    );
    let resp = app.oneshot(post_form("/widgets", &body)).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::SEE_OTHER,
        "POST after success → 303"
    );
    let location = resp
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap();
    assert_eq!(location, "/widgets");

    // Confirm via SQL: only `gamma` left.
    let labels: Vec<String> = sqlx::query_scalar("SELECT label FROM tv_bulk_widget ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(labels, vec!["gamma".to_string()]);
}

#[tokio::test]
async fn user_action_runs_against_pool_and_updates_rows() {
    let Some(pool) = pool().await else { return };
    fresh_table(&pool).await;
    let ids = seed_three(&pool).await;

    let app = build_app(pool.clone());
    let body = format!(
        "action=publish_selected&_selected_action={}&_selected_action={}",
        ids[0], ids[2]
    );
    let resp = app.oneshot(post_form("/widgets", &body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    // First + third published; second still false.
    let published: Vec<(i64, bool)> =
        sqlx::query_as("SELECT id, published FROM tv_bulk_widget ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(published[0].1, "first row should be published");
    assert!(!published[1].1, "second row should remain unpublished");
    assert!(published[2].1, "third row should be published");
}

#[tokio::test]
async fn empty_selection_yields_400() {
    let Some(pool) = pool().await else { return };
    fresh_table(&pool).await;
    let _ = seed_three(&pool).await;
    let app = build_app(pool);

    // No `_selected_action` at all.
    let body = "action=delete_selected";
    let resp = app.oneshot(post_form("/widgets", body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let msg = body_string(resp).await;
    assert!(
        msg.contains("no rows selected"),
        "expected friendly error, got: {msg}"
    );
}

#[tokio::test]
async fn unknown_action_name_yields_400() {
    let Some(pool) = pool().await else { return };
    fresh_table(&pool).await;
    let ids = seed_three(&pool).await;
    let app = build_app(pool);

    let body = format!("action=teleport&_selected_action={}", ids[0]);
    let resp = app.oneshot(post_form("/widgets", &body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let msg = body_string(resp).await;
    assert!(msg.contains("teleport"), "got: {msg}");
}

/// `with_delete_confirmation(true)` makes the first POST render
/// the confirmation template (status 200, body shows the rows)
/// instead of running the DELETE. The second POST with
/// `confirmed=true` actually deletes.
#[tokio::test]
async fn confirmation_renders_first_then_deletes_on_confirmed() {
    let Some(pool) = pool().await else { return };
    fresh_table(&pool).await;
    let ids = seed_three(&pool).await;

    // Custom Tera with both list and confirm templates.
    let mut t = Tera::default();
    t.add_raw_template(
        "tv_bulk_widget_list.html",
        "rows={{ object_list | length }}",
    )
    .unwrap();
    t.add_raw_template(
        "tv_bulk_widget_confirm_bulk_delete.html",
        "CONFIRM action={{ action }} pks={{ pks | length }} \
         objects={{ objects | length }} csrf={{ csrf_token }}",
    )
    .unwrap();
    let tera = Arc::new(t);

    let lv = ListView::for_model(Widget::SCHEMA)
        .bulk_actions(true)
        .with_delete_confirmation(true);
    let app = lv.router("/widgets", tera, pool.clone());

    // First POST — no `confirmed` field. Should render confirm
    // page (200), NOT redirect.
    let body = format!(
        "action=delete_selected&_selected_action={}&_selected_action={}",
        ids[0], ids[1]
    );
    let resp = app
        .clone()
        .oneshot(post_form("/widgets", &body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "confirm page renders 200");
    let body_str = body_string(resp).await;
    assert!(
        body_str.contains("CONFIRM action=delete_selected"),
        "confirm template body, got: {body_str}"
    );
    assert!(
        body_str.contains("pks=2"),
        "two PKs selected, got: {body_str}"
    );
    assert!(
        body_str.contains("objects=2"),
        "rows fetched for display, got: {body_str}"
    );

    // Confirm rows are still there (no DELETE happened).
    let count_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tv_bulk_widget")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count_before, 3,
        "confirmation render must not touch the table"
    );

    // Second POST — same payload + confirmed=true. Should DELETE
    // and 303 to /widgets.
    let body = format!(
        "action=delete_selected&_selected_action={}&_selected_action={}&confirmed=true",
        ids[0], ids[1]
    );
    let resp = app.oneshot(post_form("/widgets", &body)).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::SEE_OTHER,
        "confirmed=true → redirect"
    );
    let labels: Vec<String> = sqlx::query_scalar("SELECT label FROM tv_bulk_widget ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(labels, vec!["gamma".to_string()]);
}

/// Confirmation flag is gated on `delete_selected` only — custom
/// actions still run on the first POST without confirmation.
#[tokio::test]
async fn confirmation_does_not_gate_custom_actions() {
    let Some(pool) = pool().await else { return };
    fresh_table(&pool).await;
    let ids = seed_three(&pool).await;

    let publish: BulkActionFn = Arc::new(|pool, pks| {
        let pool = pool.clone();
        let pks = pks.to_vec();
        Box::pin(async move {
            let ids: Vec<i64> = pks
                .iter()
                .filter_map(|v| match v {
                    rustango::core::SqlValue::I64(n) => Some(*n),
                    _ => None,
                })
                .collect();
            sqlx::query("UPDATE tv_bulk_widget SET published = TRUE WHERE id = ANY($1)")
                .bind(&ids)
                .execute(&pool)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    });

    let lv = ListView::for_model(Widget::SCHEMA)
        .bulk_actions(true)
        .with_delete_confirmation(true) // ON, but only gates delete_selected
        .action("publish_selected", "Publish", publish);
    let app = lv.router("/widgets", tera(), pool.clone());

    // POST with publish_selected — runs immediately, no confirm.
    let body = format!("action=publish_selected&_selected_action={}", ids[0]);
    let resp = app.oneshot(post_form("/widgets", &body)).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::SEE_OTHER,
        "custom action → 303 immediately"
    );
    let pub0: bool = sqlx::query_scalar("SELECT published FROM tv_bulk_widget WHERE id = $1")
        .bind(ids[0])
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(pub0, "publish_selected ran without confirmation prompt");
}

#[tokio::test]
async fn list_get_stamps_bulk_actions_into_template_context() {
    let Some(pool) = pool().await else { return };
    fresh_table(&pool).await;
    let _ = seed_three(&pool).await;
    let app = build_app(pool);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/widgets")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    // Built-in delete_selected + one user action = 2 entries.
    assert!(body.contains("rows=3"), "got: {body}");
    assert!(body.contains("actions=2"), "got: {body}");
}
