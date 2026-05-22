//! Django-parity #348 — `list_display` supports `data.<key>` dotted
//! paths into JSON columns.

#![cfg(all(feature = "sqlite", feature = "admin", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::sql::Pool;
use rustango::Model;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "jp_post",
    admin(list_display = "title,data.headline,data.featured")
)]
#[allow(dead_code)]
pub struct JpPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
    data: serde_json::Value,
}

async fn build_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE IF NOT EXISTS "jp_post" (
            "id"    INTEGER PRIMARY KEY AUTOINCREMENT,
            "title" TEXT NOT NULL,
            "data"  TEXT NOT NULL
        )"#,
        Vec::new(),
    )
    .await
    .expect("create");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"INSERT INTO "jp_post" ("title", "data") VALUES (?, ?)"#,
        vec![
            rustango::core::SqlValue::String("First".into()),
            rustango::core::SqlValue::Json(serde_json::json!({
                "headline": "Breaking news",
                "featured": true,
            })),
        ],
    )
    .await
    .expect("seed");
    pool
}

fn build_app(pool: Pool) -> axum::Router {
    rustango::admin::Builder::new(pool).admin_prefix("").build()
}

async fn body_of(pool: Pool, uri: &str) -> String {
    let app = build_app(pool);
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "GET {uri} returned non-200");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn list_renders_json_subkey_value() {
    let pool = build_pool().await;
    let body = body_of(pool, "/jp_post").await;
    // Dotted-path string value renders inline.
    assert!(
        body.contains("Breaking news"),
        "dotted-path JSON string missing: {body}"
    );
    // Dotted-path bool value renders as the checkbox glyph.
    assert!(
        body.contains("rcms-bool yes"),
        "bool checkbox glyph missing: {body}"
    );
    // Column header shows the dotted path.
    assert!(
        body.contains("data.headline"),
        "dotted-path header missing: {body}"
    );
}

#[tokio::test]
async fn list_renders_null_for_missing_key() {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE IF NOT EXISTS "jp_post" (
            "id"    INTEGER PRIMARY KEY AUTOINCREMENT,
            "title" TEXT NOT NULL,
            "data"  TEXT NOT NULL
        )"#,
        Vec::new(),
    )
    .await
    .expect("create");
    // Row with empty data — no headline key.
    rustango::sql::raw_execute_pool(
        &pool,
        r#"INSERT INTO "jp_post" ("title", "data") VALUES (?, ?)"#,
        vec![
            rustango::core::SqlValue::String("Empty".into()),
            rustango::core::SqlValue::Json(serde_json::json!({})),
        ],
    )
    .await
    .expect("seed");
    let body = body_of(pool, "/jp_post").await;
    // Missing key drilldown emits `<em>NULL</em>`.
    assert!(
        body.contains("<em>NULL</em>"),
        "expected NULL fallback for missing key: {body}"
    );
}
