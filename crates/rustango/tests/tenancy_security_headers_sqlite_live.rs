#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "admin"))]
//! #1699 — `server::Builder::security_headers` reaches both branches of
//! the Host dispatch: the operator console and the tenant side (login,
//! admin), which the api-router layer never covered.

use axum::body::Body;
use axum::http::Request;
use rustango::security_headers::SecurityHeadersLayer;
use rustango::sql::sqlx;
use tower::ServiceExt;

async fn app(headers: bool) -> (axum::Router, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
    let pool = sqlx::SqlitePool::connect(&url).await.expect("connect");
    let mut b = rustango::server::Builder::from_pool(pool, url, "localhost");
    if headers {
        b = b.security_headers(SecurityHeadersLayer::strict());
    }
    (b.into_router().await.expect("assemble"), tmp)
}

async fn xfo(app: &axum::Router, host: &str, uri: &str) -> Option<String> {
    let req = Request::builder()
        .uri(uri)
        .header("host", host)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    resp.headers()
        .get("x-frame-options")
        .map(|v| v.to_str().unwrap().to_owned())
}

#[tokio::test]
async fn both_branches_carry_the_configured_headers() {
    let (app, _tmp) = app(true).await;
    assert_eq!(
        xfo(&app, "localhost", "/login").await.as_deref(),
        Some("DENY")
    );
    assert_eq!(
        xfo(&app, "acme.localhost", "/login").await.as_deref(),
        Some("DENY"),
        "the tenant branch must carry them too"
    );
}

#[tokio::test]
async fn nothing_is_added_without_the_setter() {
    let (app, _tmp) = app(false).await;
    assert_eq!(xfo(&app, "acme.localhost", "/login").await, None);
}
