//! Django-parity #435 — DRF `ListSerializer(many=True)` shape.
//! `POST <prefix>` with a JSON array body bulk-creates every entry
//! and returns the created rows in submission order. Validation
//! is atomic: a single bad entry rejects the whole bulk before
//! any insert lands.

#![cfg(all(feature = "sqlite", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use rustango::core::Model as _;
use rustango::sql::{Auto, Pool};
use rustango::viewset::ViewSet;
use rustango::Model;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(table = "bulk_widget", display = "label")]
#[allow(dead_code)]
pub struct Widget {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 60)]
    pub label: String,
    pub priority: i32,
}

async fn fresh_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE bulk_widget (
            id       INTEGER PRIMARY KEY AUTOINCREMENT,
            label    TEXT NOT NULL,
            priority INTEGER NOT NULL
        )"#,
        Vec::new(),
    )
    .await
    .expect("create");
    pool
}

fn build_app(pool: Pool) -> axum::Router {
    ViewSet::for_model(Widget::SCHEMA).router_pool("/widgets", pool)
}

async fn post_json(app: axum::Router, body: &str) -> (StatusCode, String) {
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/widgets")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn json_array_body_bulk_creates_rows_in_order() {
    let pool = fresh_pool().await;
    let app = build_app(pool.clone());
    let body = r#"[
        {"label":"first","priority":1},
        {"label":"second","priority":2},
        {"label":"third","priority":3}
    ]"#;
    let (status, body) = post_json(app, body).await;
    assert_eq!(status, StatusCode::CREATED, "got {status} {body}");
    let arr: serde_json::Value = serde_json::from_str(&body).unwrap();
    let rows = arr.as_array().expect("expected JSON array response");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["label"], "first");
    assert_eq!(rows[1]["label"], "second");
    assert_eq!(rows[2]["label"], "third");
    // PKs auto-assigned in order.
    assert_eq!(rows[0]["id"], 1);
    assert_eq!(rows[2]["id"], 3);
}

#[tokio::test]
async fn empty_array_returns_201_with_empty_array() {
    let pool = fresh_pool().await;
    let app = build_app(pool);
    let (status, body) = post_json(app, "[]").await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body, "[]");
}

#[tokio::test]
async fn single_object_body_still_returns_single_row() {
    // Back-compat — the existing single-object shape must keep
    // working (returns a single JSON object, not an array).
    let pool = fresh_pool().await;
    let app = build_app(pool);
    let body = r#"{"label":"solo","priority":7}"#;
    let (status, body) = post_json(app, body).await;
    assert_eq!(status, StatusCode::CREATED);
    let row: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        row.is_object(),
        "single-object body should return an object, got {body}"
    );
    assert_eq!(row["label"], "solo");
    assert_eq!(row["id"], 1);
}

#[tokio::test]
async fn bulk_entry_with_invalid_field_rejects_whole_bulk_atomically() {
    let pool = fresh_pool().await;
    let app = build_app(pool.clone());
    // Second entry is missing the required `priority` field.
    let body = r#"[
        {"label":"valid","priority":5},
        {"label":"missing_priority"}
    ]"#;
    let (status, msg) = post_json(app, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        msg.contains("bulk entry 1"),
        "error message should identify the failing index, got: {msg}"
    );
    // Atomic-validate — the first entry must NOT have been inserted.
    let count =
        rustango::sql::raw_execute_pool(&pool, "SELECT COUNT(*) FROM bulk_widget", Vec::new())
            .await;
    assert!(count.is_ok());
    let Pool::Sqlite(sp) = &pool else {
        panic!("test gated to sqlite");
    };
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bulk_widget")
        .fetch_one(sp)
        .await
        .unwrap();
    assert_eq!(
        total, 0,
        "atomic-validate: a bad row must reject the whole bulk before any insert lands"
    );
}

#[tokio::test]
async fn bulk_entry_that_is_not_an_object_returns_400() {
    let pool = fresh_pool().await;
    let app = build_app(pool);
    let body = r#"[
        {"label":"ok","priority":1},
        42
    ]"#;
    let (status, msg) = post_json(app, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        msg.contains("not a JSON object") || msg.contains("entry 1"),
        "error message should call out the bad shape, got: {msg}"
    );
}
