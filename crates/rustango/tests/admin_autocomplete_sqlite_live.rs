//! Django-parity #358 — admin `autocomplete_fields`.
//!
//! Verifies:
//!   * macro attr + schema field
//!   * `__autocomplete` JSON endpoint returns matched rows
//!   * change-form wires the input to a fetch-driven `<datalist>`

#![cfg(all(feature = "sqlite", feature = "admin", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::core::Model as _;
use rustango::sql::{ForeignKey, Pool};
use rustango::Model;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(table = "ac_author", display = "name", admin(search_fields = "name"))]
#[allow(dead_code)]
pub struct AcAuthor {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "ac_post",
    admin(list_display = "title", autocomplete_fields = "author_id")
)]
#[allow(dead_code)]
pub struct AcPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
    author_id: ForeignKey<AcAuthor, i64>,
}

async fn build_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    for ddl in [
        r#"CREATE TABLE IF NOT EXISTS "ac_author" (
            "id"   INTEGER PRIMARY KEY AUTOINCREMENT,
            "name" TEXT NOT NULL
        )"#,
        r#"CREATE TABLE IF NOT EXISTS "ac_post" (
            "id"        INTEGER PRIMARY KEY AUTOINCREMENT,
            "title"     TEXT NOT NULL,
            "author_id" INTEGER NOT NULL REFERENCES "ac_author"("id")
        )"#,
    ] {
        rustango::sql::raw_execute_pool(&pool, ddl, Vec::new())
            .await
            .expect("create");
    }
    for name in ["Asimov", "Le Guin", "Bradbury"] {
        rustango::sql::raw_execute_pool(
            &pool,
            r#"INSERT INTO "ac_author" ("name") VALUES (?)"#,
            vec![rustango::core::SqlValue::String(name.into())],
        )
        .await
        .expect("seed author");
    }
    pool
}

fn build_app(pool: Pool) -> axum::Router {
    rustango::admin::Builder::new(pool).admin_prefix("").build()
}

async fn fetch_text(pool: Pool, uri: &str) -> String {
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
fn schema_records_autocomplete_fields() {
    let cfg = AcPost::SCHEMA.admin.expect("admin attr set");
    assert_eq!(cfg.autocomplete_fields, &["author_id"]);
}

#[tokio::test]
async fn create_form_emits_typeahead_widget() {
    let pool = build_pool().await;
    let body = fetch_text(pool, "/ac_post/new").await;
    assert!(
        body.contains(r#"list="author_id_options""#),
        "datalist linkage missing: {body}"
    );
    assert!(
        body.contains(r#"id="author_id_options""#),
        "datalist node missing: {body}"
    );
    assert!(
        body.contains("/ac_author/__autocomplete"),
        "endpoint URL missing in JS: {body}"
    );
}

#[tokio::test]
async fn autocomplete_endpoint_returns_matches() {
    let pool = build_pool().await;
    let json = fetch_text(pool, "/ac_author/__autocomplete?q=Asi").await;
    let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    let results = v["results"].as_array().expect("results array");
    assert_eq!(results.len(), 1, "expected 1 match, got: {results:?}");
    assert_eq!(results[0]["text"], "Asimov");
}

#[tokio::test]
async fn autocomplete_endpoint_empty_query_returns_all_capped() {
    let pool = build_pool().await;
    let json = fetch_text(pool, "/ac_author/__autocomplete?q=").await;
    let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    let results = v["results"].as_array().expect("results array");
    // All 3 seeded authors come back when q is empty.
    assert_eq!(results.len(), 3);
}
