//! Django-parity #366 — recent-actions widget on the admin home.
//!
//! Verifies the admin's `/` index page surfaces the newest audit-log
//! entries written by create / update / delete flows.

#![cfg(all(feature = "sqlite", feature = "admin", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use rustango::sql::Pool;
use rustango::Model;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(table = "ra_post")]
#[allow(dead_code)]
pub struct RaPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
}

async fn build_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE IF NOT EXISTS "ra_post" (
            "id"    INTEGER PRIMARY KEY AUTOINCREMENT,
            "title" TEXT NOT NULL
        )"#,
        Vec::new(),
    )
    .await
    .expect("create");
    rustango::audit::ensure_table_pool(&pool)
        .await
        .expect("audit table");
    pool
}

fn build_app(pool: Pool) -> axum::Router {
    rustango::admin::Builder::new(pool).admin_prefix("").build()
}

async fn body_of(app: axum::Router, uri: &str) -> String {
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "GET {uri} returned non-200");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn index_widget_shows_recent_create() {
    let pool = build_pool().await;
    let app = build_app(pool.clone());

    // Empty home: widget absent.
    let body = body_of(app.clone(), "/").await;
    assert!(
        !body.contains("recent-actions"),
        "empty audit log shouldn't render widget: {body}"
    );

    // Create a row via the admin so an audit entry is written.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/ra_post")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("title=Hello"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::SEE_OTHER || resp.status() == StatusCode::OK,
        "create POST failed: {}",
        resp.status()
    );

    // Re-fetch home: widget should now name the new row.
    let body = body_of(app, "/").await;
    assert!(
        body.contains("recent-actions"),
        "widget should render after a create: {body}"
    );
    assert!(body.contains("Recent actions"), "header missing: {body}");
    assert!(
        body.contains("ra_post/1") || body.contains("ra_post</a>"),
        "expected entity link in widget: {body}"
    );
    assert!(
        body.to_lowercase().contains("create"),
        "expected create op in widget: {body}"
    );
}
