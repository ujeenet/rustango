//! End-to-end live test for the in-admin model reference (`/__docs`,
//! Django `admindocs` parity, #1011). Builds an admin (no session auth →
//! open routes), hits `/__docs`, and asserts the registered models +
//! their fields / types / key-flags / relations render.

#![cfg(all(feature = "sqlite", feature = "admin", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use rustango::sql::Pool;
use rustango::Model;
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "adoc_category",
    app = "adoc_app",
    admin(list_display = "name")
)]
pub struct AdocCategory {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 100)]
    name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "adoc_post", app = "adoc_app", admin(list_display = "title"))]
pub struct AdocPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
    #[rustango(fk = "adoc_category", on = "id")]
    category_id: i64,
}

async fn docs_html() -> String {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    let app = rustango::admin::Builder::new(pool).admin_prefix("").build();
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/__docs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "/__docs should render");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).expect("utf8")
}

#[tokio::test]
async fn docs_lists_models_fields_and_relations() {
    let html = docs_html().await;

    // App grouping + both models present.
    assert!(html.contains("adoc_app"), "app label missing: {html}");
    assert!(html.contains("AdocPost"), "model name missing");
    assert!(html.contains("adoc_post"), "table name missing");
    assert!(html.contains("AdocCategory"), "second model missing");

    // Fields + their rendered types.
    assert!(html.contains("title"), "field name missing");
    assert!(html.contains("String"), "field type missing");

    // PK flag on the id column.
    assert!(html.contains("PK"), "primary-key flag missing");

    // FK relation rendered (category_id → adoc_category.id).
    assert!(
        html.contains("FK → adoc_category.id"),
        "FK relation missing: {html}"
    );
}
