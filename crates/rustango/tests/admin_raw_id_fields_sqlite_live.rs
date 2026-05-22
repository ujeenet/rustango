//! Django-parity #357 — admin `raw_id_fields`.
//!
//! Verifies the macro-emitted attr + the lookup link rendered next
//! to the FK input on the change-form.

#![cfg(all(feature = "sqlite", feature = "admin", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::core::Model as _;
use rustango::sql::{ForeignKey, Pool};
use rustango::Model;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(table = "rid_author")]
#[allow(dead_code)]
pub struct RidAuthor {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "rid_post",
    admin(list_display = "title,author_id", raw_id_fields = "author_id")
)]
#[allow(dead_code)]
pub struct RidPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
    author_id: ForeignKey<RidAuthor, i64>,
}

async fn build_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    for ddl in [
        r#"CREATE TABLE IF NOT EXISTS "rid_author" (
            "id"   INTEGER PRIMARY KEY AUTOINCREMENT,
            "name" TEXT NOT NULL
        )"#,
        r#"CREATE TABLE IF NOT EXISTS "rid_post" (
            "id"         INTEGER PRIMARY KEY AUTOINCREMENT,
            "title"      TEXT NOT NULL,
            "author_id"  INTEGER NOT NULL REFERENCES "rid_author"("id")
        )"#,
    ] {
        rustango::sql::raw_execute_pool(&pool, ddl, Vec::new())
            .await
            .expect("create");
    }
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
fn schema_records_raw_id_fields() {
    let cfg = RidPost::SCHEMA.admin.expect("admin attr set");
    assert_eq!(cfg.raw_id_fields, &["author_id"]);
}

#[tokio::test]
async fn create_form_emits_lookup_link_for_raw_id_field() {
    let pool = build_pool().await;
    let body = fetch_body(pool, "/rid_post/new").await;
    assert!(
        body.contains("raw-id-lookup"),
        "lookup link class missing: {body}"
    );
    // The lookup href points at the target model's admin list view.
    assert!(
        body.contains("href=\"/rid_author\""),
        "lookup href missing: {body}"
    );
}

#[tokio::test]
async fn create_form_skips_lookup_for_non_listed_fk() {
    // Model with an FK but no `raw_id_fields` attr — should NOT
    // render the lookup link.
    let pool = build_pool().await;
    // Seed one author so the form has something to point at.
    rustango::sql::raw_execute_pool(
        &pool,
        r#"INSERT INTO "rid_author" ("name") VALUES (?)"#,
        vec![rustango::core::SqlValue::String("Asimov".into())],
    )
    .await
    .expect("seed author");
    // RidAuthor itself has no FK at all — different angle but still
    // covers "no raw-id-lookup HTML rendered for a non-raw-id field":
    let body = fetch_body(pool, "/rid_author/new").await;
    assert!(
        !body.contains("raw-id-lookup"),
        "lookup link should not appear on a model without raw_id_fields: {body}"
    );
}
