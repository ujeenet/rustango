#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "admin"))]
//! #1699 — `server::Builder::security_headers` reaches both branches of
//! the Host dispatch: the operator console and the tenant side (app
//! routes, login, admin), which the api-router layer never covered.
//!
//! The registry is not migrated, so tenant-host requests error (500):
//! any response from that branch is enough, because the layer wraps it.

use axum::body::Body;
use axum::http::Request;
use axum::routing::get;
use rustango::security_headers::SecurityHeadersLayer;
use rustango::sql::sqlx;
use tower::ServiceExt;

async fn app(headers: bool) -> (axum::Router, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
    let pool = sqlx::SqlitePool::connect(&url).await.expect("connect");
    // An api route with no layer of its own, so a layer moved onto the
    // admin routes alone would miss it.
    let api = axum::Router::new().route("/app", get(|| async { "ok" }));
    let mut b = rustango::server::Builder::from_pool(pool, url, "localhost").api(api);
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
    for (host, uri) in [
        ("localhost", "/login"),
        ("acme.localhost", "/login"),
        ("acme.localhost", "/app"),
    ] {
        assert_eq!(
            xfo(&app, host, uri).await.as_deref(),
            Some("DENY"),
            "{host}{uri} must carry the headers"
        );
    }
}

#[tokio::test]
async fn nothing_is_added_without_the_setter() {
    let (app, _tmp) = app(false).await;
    assert_eq!(xfo(&app, "acme.localhost", "/login").await, None);
}
