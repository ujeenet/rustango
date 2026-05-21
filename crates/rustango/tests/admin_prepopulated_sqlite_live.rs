//! Django-parity #356 — admin `prepopulated_fields`.
//!
//! Verifies the macro-emitted attr, schema shape, and the
//! client-side JS hookup on the change-form.

#![cfg(all(feature = "sqlite", feature = "admin", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::core::Model as _;
use rustango::sql::Pool;
use rustango::Model;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "pp_post",
    admin(list_display = "title,slug", prepopulated_fields = "slug:title")
)]
#[allow(dead_code)]
pub struct PpPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
    #[rustango(max_length = 200)]
    slug: String,
}

async fn build_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    let ddl = r#"CREATE TABLE IF NOT EXISTS "pp_post" (
        "id"    INTEGER PRIMARY KEY AUTOINCREMENT,
        "title" TEXT NOT NULL,
        "slug"  TEXT NOT NULL
    )"#;
    rustango::sql::raw_execute_pool(&pool, ddl, Vec::new())
        .await
        .expect("create");
    pool
}

fn build_app(pool: Pool) -> axum::Router {
    rustango::admin::Builder::new(pool).admin_prefix("").build()
}

async fn fetch_body(pool: Pool, uri: &str) -> String {
    let app = build_app(pool);
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "GET {uri} returned non-200");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[test]
fn schema_records_prepopulated_fields() {
    let cfg = PpPost::SCHEMA.admin.expect("admin attr set");
    assert_eq!(cfg.prepopulated_fields.len(), 1);
    let pp = &cfg.prepopulated_fields[0];
    assert_eq!(pp.target, "slug");
    assert_eq!(pp.sources, &["title"]);
}

#[tokio::test]
async fn create_form_emits_prepopulated_script() {
    let pool = build_pool().await;
    let body = fetch_body(pool, "/pp_post/new").await;
    assert!(
        body.contains("prepopulated-config"),
        "script id missing: {body}"
    );
    // The config JSON should name target=slug + sources=[title].
    assert!(
        body.contains("\"target\":\"slug\""),
        "target missing in config: {body}"
    );
    assert!(
        body.contains("\"sources\":[\"title\"]"),
        "sources missing in config: {body}"
    );
}

#[tokio::test]
async fn edit_form_suppresses_prepopulated_script() {
    let pool = build_pool().await;
    // Seed one row so the edit form has something to render.
    rustango::sql::raw_execute_pool(
        &pool,
        r#"INSERT INTO "pp_post" ("title", "slug") VALUES (?, ?)"#,
        vec![
            rustango::core::SqlValue::String("Hello World".into()),
            rustango::core::SqlValue::String("hello-world".into()),
        ],
    )
    .await
    .expect("seed insert");
    let body = fetch_body(pool, "/pp_post/1").await;
    assert!(
        !body.contains("prepopulated-config"),
        "edit form should not emit prepopulated script: {body}"
    );
}
