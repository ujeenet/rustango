//! Django-parity #361 — `register_admin_object_permission!` adds
//! per-row enforcement to the admin's `add` / `change` / `delete`
//! / `view` write paths. Each registered hook is consulted at
//! request time; a `false` return yields 403.

#![cfg(all(feature = "sqlite", feature = "admin", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::sql::Pool;
use rustango::Model;
use serde_json::Value;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(table = "op_post", display = "title")]
#[allow(dead_code)]
pub struct OpPost {
    #[rustango(primary_key)]
    pub id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub owner_id: i64,
}

// Per-object hooks. The "owner_id == 42" rule deliberately denies
// the seeded row (which has owner_id = 7) for every action.
fn only_owner_42_view(_parts: &axum::http::request::Parts, row: Option<&Value>) -> bool {
    row.and_then(|r| r.get("owner_id").and_then(Value::as_i64)) == Some(42)
}
fn only_owner_42_change(_parts: &axum::http::request::Parts, row: Option<&Value>) -> bool {
    row.and_then(|r| r.get("owner_id").and_then(Value::as_i64)) == Some(42)
}
fn only_owner_42_delete(_parts: &axum::http::request::Parts, row: Option<&Value>) -> bool {
    row.and_then(|r| r.get("owner_id").and_then(Value::as_i64)) == Some(42)
}
fn deny_add(_parts: &axum::http::request::Parts, _row: Option<&Value>) -> bool {
    false
}

rustango::register_admin_object_permission!("op_post", "view", only_owner_42_view);
rustango::register_admin_object_permission!("op_post", "change", only_owner_42_change);
rustango::register_admin_object_permission!("op_post", "delete", only_owner_42_delete);
rustango::register_admin_object_permission!("op_post", "add", deny_add);

fn build_app(pool: Pool) -> axum::Router {
    rustango::admin::Builder::new(pool).admin_prefix("").build()
}

async fn fresh_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE op_post (
            id       INTEGER PRIMARY KEY AUTOINCREMENT,
            title    TEXT NOT NULL,
            owner_id INTEGER NOT NULL
        )"#,
        Vec::new(),
    )
    .await
    .expect("create");
    rustango::sql::raw_execute_pool(
        &pool,
        "INSERT INTO op_post (id, title, owner_id) VALUES (1, 'Hi', 7)",
        Vec::new(),
    )
    .await
    .expect("seed");
    pool
}

async fn status_of(method: Method, uri: &str, body: Body) -> StatusCode {
    let pool = fresh_pool().await;
    let app = build_app(pool);
    let req = Request::builder().method(method).uri(uri);
    let req = req
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    resp.status()
}

#[tokio::test]
async fn view_hook_blocks_detail_view_with_403() {
    let status = status_of(Method::GET, "/op_post/1", Body::empty()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn change_hook_blocks_edit_form_with_403() {
    let status = status_of(Method::GET, "/op_post/1/edit", Body::empty()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn change_hook_blocks_update_submit_with_403() {
    let status = status_of(
        Method::POST,
        "/op_post/1",
        Body::from("title=Edited&owner_id=7"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn delete_hook_blocks_delete_submit_with_403() {
    let status = status_of(Method::POST, "/op_post/1/delete", Body::empty()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn add_hook_blocks_create_form_with_403() {
    let status = status_of(Method::GET, "/op_post/new", Body::empty()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn add_hook_blocks_create_submit_with_403() {
    let status = status_of(
        Method::POST,
        "/op_post",
        Body::from("title=New&owner_id=42"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
