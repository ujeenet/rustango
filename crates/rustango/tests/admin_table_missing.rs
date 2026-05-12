#![cfg(feature = "postgres")]
//! Verify the admin returns a friendly HTML page (HTTP 503) when a
//! registered model's table doesn't exist in the connected DB —
//! instead of the raw `relation X does not exist` 500 JSON.
//!
//! Skips silently if `DATABASE_URL` is unset.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use rustango::sql::{sqlx, Auto};
use rustango::Model;
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(table = "admin_table_missing_widget")]
#[allow(dead_code)]
pub struct Widget {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
}

#[tokio::test]
async fn missing_table_returns_503_with_migrate_hint_html() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pool = sqlx::PgPool::connect(&url).await.expect("connect");

    // Make sure the table really is absent.
    sqlx::query("DROP TABLE IF EXISTS admin_table_missing_widget CASCADE")
        .execute(&pool)
        .await
        .unwrap();

    // Touch the SCHEMA so inventory registers the model in this binary.
    use rustango::core::Model as _;
    let _ = Widget::SCHEMA.table;

    let router = rustango::admin::Builder::new(pool).build();
    let req = Request::builder()
        .method(Method::GET)
        .uri("/admin_table_missing_widget")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.expect("handler runs");

    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&bytes);

    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "missing-table should be 503; got {status}; body head: {}",
        &html[..html.len().min(300)]
    );
    assert!(
        html.contains("admin_table_missing_widget"),
        "HTML should name the missing table; head: {}",
        &html[..html.len().min(300)]
    );
    assert!(
        html.to_lowercase().contains("migrate"),
        "HTML should hint at running migrations; head: {}",
        &html[..html.len().min(300)]
    );
    assert!(
        !html.contains("\"error\":"),
        "old JSON 500 shape must NOT appear; head: {}",
        &html[..html.len().min(300)]
    );
}
