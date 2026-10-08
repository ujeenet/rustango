//! Issue #361 — `register_admin_object_permission!` adds
//! per-row enforcement to the admin's `add` / `change` / `delete`
//! / `view` write paths. Each registered hook is consulted at
//! request time; a `false` return yields 403.

#![cfg(all(feature = "sqlite", feature = "admin", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::sql::{FetcherPool as _, Pool};
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

// #1762: the bulk actions run the same per-row hooks.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "op_bulk",
    admin(actions = "delete_selected, restore_selected, touch_selected, stamp_selected")
)]
#[allow(dead_code)]
pub struct OpBulk {
    #[rustango(primary_key)]
    pub id: rustango::Auto<i64>,
    pub owner_id: i64,
    #[rustango(soft_delete)]
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn owner_42(_parts: &axum::http::request::Parts, row: Option<&Value>) -> bool {
    row.and_then(|r| r.get("owner_id").and_then(Value::as_i64)) == Some(42)
}
rustango::register_admin_object_permission!("op_bulk", "delete", owner_42);
rustango::register_admin_object_permission!("op_bulk", "change", owner_42);

fn deny(_parts: &axum::http::request::Parts, _row: Option<&Value>) -> bool {
    false
}
rustango::register_admin_object_permission!("op_bulk", "stamp_selected", deny);

/// Row 1 belongs to owner 42, row 2 to owner 7; both deleted when `deleted`.
async fn bulk_pool(deleted: bool) -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        "CREATE TABLE op_bulk (id INTEGER PRIMARY KEY, owner_id INTEGER NOT NULL, deleted_at TEXT)",
        Vec::new(),
    )
    .await
    .expect("create");
    rustango::audit::ensure_table_pool(&pool)
        .await
        .expect("audit table");
    let at = if deleted {
        "'2026-01-01T00:00:00Z'"
    } else {
        "NULL"
    };
    rustango::sql::raw_execute_pool(
        &pool,
        &format!(
            "INSERT INTO op_bulk (id, owner_id, deleted_at) VALUES (1, 42, {at}), (2, 7, {at})"
        ),
        Vec::new(),
    )
    .await
    .expect("seed");
    pool
}

async fn run_action(pool: &Pool, form: &'static str) -> StatusCode {
    let req = Request::builder()
        .method(Method::POST)
        .uri("/op_bulk/__action")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    build_app(pool.clone()).oneshot(req).await.unwrap().status()
}

async fn live_ids(pool: &Pool) -> Vec<i64> {
    OpBulk::objects()
        .fetch(pool)
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.deleted_at.is_none())
        .map(|r| r.id.get().copied().unwrap())
        .collect()
}

#[tokio::test]
async fn delete_selected_refuses_when_a_row_hook_denies() {
    let pool = bulk_pool(false).await;
    let form = "action=delete_selected&_selected=1&_selected=2";
    assert_eq!(run_action(&pool, form).await, StatusCode::FORBIDDEN);
    assert_eq!(live_ids(&pool).await, vec![1, 2], "nothing deleted");
    let audit = rustango::audit::fetch_for_entity_pool(&pool, "op_bulk", "1").await;
    assert!(
        audit.unwrap().is_empty(),
        "a refused action writes no audit row"
    );

    let form = "action=delete_selected&_selected=1";
    assert_eq!(run_action(&pool, form).await, StatusCode::SEE_OTHER);
    assert_eq!(live_ids(&pool).await, vec![2]);
    let audit = rustango::audit::fetch_for_entity_pool(&pool, "op_bulk", "1").await;
    assert_eq!(audit.unwrap().len(), 1, "the allowed delete is audited");
}

#[tokio::test]
async fn restore_selected_refuses_when_a_row_hook_denies() {
    let pool = bulk_pool(true).await;
    let form = "action=restore_selected&_selected=1&_selected=2";
    assert_eq!(run_action(&pool, form).await, StatusCode::FORBIDDEN);
    assert!(live_ids(&pool).await.is_empty(), "nothing restored");
    let form = "action=restore_selected&_selected=1";
    assert_eq!(run_action(&pool, form).await, StatusCode::SEE_OTHER);
    assert_eq!(live_ids(&pool).await, vec![1]);
}

// #1805: a custom action needs the `change` hook and a hook named after it.
static TOUCHED: std::sync::Mutex<Vec<rustango::core::SqlValue>> = std::sync::Mutex::new(Vec::new());

fn record<'a>(
    _: &'a Pool,
    pks: &'a [rustango::core::SqlValue],
) -> rustango::admin::AdminActionFuture<'a> {
    TOUCHED.lock().unwrap().extend_from_slice(pks);
    Box::pin(async { Ok(()) })
}

async fn run_custom(pool: &Pool, form: &'static str) -> StatusCode {
    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .register_action("op_bulk", "touch_selected", record)
        .register_action("op_bulk", "stamp_selected", record)
        .build();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/op_bulk/__action")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    app.oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn custom_action_refuses_when_a_row_hook_denies() {
    let pool = bulk_pool(false).await;
    let form = "action=touch_selected&_selected=1&_selected=2";
    assert_eq!(run_custom(&pool, form).await, StatusCode::FORBIDDEN);
    assert!(TOUCHED.lock().unwrap().is_empty(), "handler not run");

    let form = "action=stamp_selected&_selected=1";
    assert_eq!(run_custom(&pool, form).await, StatusCode::FORBIDDEN);
    assert!(TOUCHED.lock().unwrap().is_empty(), "named hook refuses");

    let form = "action=touch_selected&_selected=1";
    assert_eq!(run_custom(&pool, form).await, StatusCode::SEE_OTHER);
    assert_eq!(
        *TOUCHED.lock().unwrap(),
        vec![rustango::core::SqlValue::I64(1)]
    );
}

// #2231: the "view" hook hides rows from the list, autocomplete and FK facet names.
#[derive(Model, Debug, Clone)]
#[rustango(table = "op_note", admin(list_display = "id", list_filter = "post_id"))]
#[allow(dead_code)]
pub struct OpNote {
    #[rustango(primary_key)]
    pub id: rustango::Auto<i64>,
    #[rustango(fk = "op_post", on = "id")]
    pub post_id: i64,
}

/// Post 1 belongs to owner 7 (denied), post 2 to owner 42; one note on each.
async fn view_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    for sql in [
        "CREATE TABLE op_post (id INTEGER PRIMARY KEY, title TEXT NOT NULL, owner_id INTEGER NOT NULL)",
        "CREATE TABLE op_note (id INTEGER PRIMARY KEY, post_id INTEGER NOT NULL)",
        "INSERT INTO op_post (id, title, owner_id) VALUES (1, 'theirs-row', 7), (2, 'mine-row', 42)",
        "INSERT INTO op_note (id, post_id) VALUES (1, 1), (2, 2)",
    ] {
        rustango::sql::raw_execute_pool(&pool, sql, Vec::new())
            .await
            .expect(sql);
    }
    pool
}

async fn get_body(uri: &str) -> String {
    use http_body_util::BodyExt as _;
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let resp = build_app(view_pool().await).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn view_hook_hides_rows_from_the_list() {
    let body = get_body("/op_post").await;
    assert!(body.contains("mine-row"), "control: an allowed row shows");
    assert!(!body.contains("theirs-row"), "a denied row is listed");
}

#[tokio::test]
async fn view_hook_hides_rows_from_autocomplete() {
    let body = get_body("/op_post/__autocomplete?q=row").await;
    assert!(body.contains("mine-row"), "control: {body}");
    assert!(
        !body.contains("theirs-row"),
        "a denied row is offered: {body}"
    );
}

#[tokio::test]
async fn view_hook_hides_fk_facet_names() {
    let body = get_body("/op_note").await;
    assert!(body.contains("mine-row"), "control: an allowed name shows");
    assert!(!body.contains("theirs-row"), "a denied row's name shows");
}

// #1818: an action registered with `ActionPerm::Delete` needs `delete`, not `change`.
#[derive(Model, Debug, Clone)]
#[rustango(table = "op_purge", admin(actions = "purge_selected, tidy_selected"))]
#[allow(dead_code)]
pub struct OpPurge {
    #[rustango(primary_key)]
    pub id: rustango::Auto<i64>,
}
rustango::register_admin_object_permission!("op_purge", "delete", deny);

static PURGED: std::sync::Mutex<Vec<rustango::core::SqlValue>> = std::sync::Mutex::new(Vec::new());

fn purge<'a>(
    _: &'a Pool,
    pks: &'a [rustango::core::SqlValue],
) -> rustango::admin::AdminActionFuture<'a> {
    PURGED.lock().unwrap().extend_from_slice(pks);
    Box::pin(async { Ok(()) })
}

async fn run_purge(perms: Option<&[&str]>, form: &'static str) -> StatusCode {
    use rustango::admin::ActionPerm;
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        "CREATE TABLE op_purge (id INTEGER PRIMARY KEY)",
        Vec::new(),
    )
    .await
    .expect("create");
    rustango::sql::raw_execute_pool(&pool, "INSERT INTO op_purge (id) VALUES (1)", Vec::new())
        .await
        .expect("seed");
    rustango::audit::ensure_table_pool(&pool)
        .await
        .expect("audit table");
    let mut b = rustango::admin::Builder::new(pool)
        .admin_prefix("")
        .register_action_with_perm("op_purge", "purge_selected", ActionPerm::Delete, purge)
        .register_action("op_purge", "tidy_selected", purge);
    if let Some(p) = perms {
        b = b.with_user_perms(p.iter().map(|s| (*s).to_owned()));
    }
    let req = Request::builder()
        .method(Method::POST)
        .uri("/op_purge/__action")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    b.build().oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn a_delete_action_runs_the_delete_hook_and_perm() {
    let purge_form = "action=purge_selected&_selected=1";
    assert_eq!(run_purge(None, purge_form).await, StatusCode::FORBIDDEN);
    let change_only: &[&str] = &["op_purge.view", "op_purge.change"];
    assert_eq!(
        run_purge(Some(change_only), purge_form).await,
        StatusCode::FORBIDDEN
    );
    assert!(PURGED.lock().unwrap().is_empty(), "handler not run");
    // The same handler as a `change` action passes: no `change` hook denies.
    let tidy = "action=tidy_selected&_selected=1";
    assert_eq!(
        run_purge(Some(change_only), tidy).await,
        StatusCode::SEE_OTHER
    );
    assert_eq!(PURGED.lock().unwrap().len(), 1);
}
