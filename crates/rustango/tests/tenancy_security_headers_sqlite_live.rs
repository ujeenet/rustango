#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "admin"))]
//! #1699, #1700 — `server::Builder`'s security headers, Host allowlist
//! and HTTPS redirect reach both branches of the Host dispatch: the
//! operator console and the tenant side (app routes, login, admin),
//! which the api-router layers never covered.
//!
//! The registry is not migrated, so tenant-host requests error (500):
//! any response from that branch is enough, because the layer wraps it.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use rustango::security_headers::SecurityHeadersLayer;
use rustango::sql::sqlx;
use tower::ServiceExt;

type Builder = rustango::server::Builder<sqlx::Sqlite>;

async fn build(f: impl FnOnce(Builder) -> Builder) -> (axum::Router, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
    let pool = sqlx::SqlitePool::connect(&url).await.expect("connect");
    // An api route with no layer of its own, so a layer moved onto the
    // admin routes alone would miss it.
    let api = axum::Router::new().route("/app", get(|| async { "ok" }));
    let b = Builder::from_pool(pool, url, "localhost").api(api);
    (f(b).into_router().await.expect("assemble"), tmp)
}

async fn app(headers: bool) -> (axum::Router, tempfile::TempDir) {
    build(|b| {
        if headers {
            b.security_headers(SecurityHeadersLayer::strict())
        } else {
            b
        }
    })
    .await
}

async fn status(app: &axum::Router, host: &str, uri: &str) -> StatusCode {
    let req = Request::builder()
        .uri(uri)
        .header("host", host)
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
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

/// #1700 — a disallowed Host is refused on the tenant login and admin,
/// not only on the api routes. (The console answers only the apex.)
#[tokio::test]
async fn the_host_allowlist_covers_both_branches() {
    use rustango::host_validation::AllowedHostsLayer;
    let (app, _tmp) =
        build(|b| b.allowed_hosts(AllowedHostsLayer::new(["localhost", ".localhost"]))).await;
    for uri in ["/login", "/app"] {
        assert_eq!(
            status(&app, "evil.example", uri).await,
            StatusCode::BAD_REQUEST
        );
    }
    assert_ne!(
        status(&app, "localhost", "/login").await,
        StatusCode::BAD_REQUEST
    );
    assert_ne!(
        status(&app, "acme.localhost", "/login").await,
        StatusCode::BAD_REQUEST
    );
}

/// #1700 — plain HTTP is redirected on both branches.
#[tokio::test]
async fn the_https_redirect_covers_both_branches() {
    use rustango::ssl_redirect::SslRedirectLayer;
    let (app, _tmp) = build(|b| b.ssl_redirect(SslRedirectLayer::new())).await;
    for (host, uri) in [
        ("localhost", "/login"),
        ("acme.localhost", "/login"),
        ("acme.localhost", "/app"),
    ] {
        assert_eq!(
            status(&app, host, uri).await,
            StatusCode::MOVED_PERMANENTLY,
            "{host}{uri} must redirect"
        );
    }
}
