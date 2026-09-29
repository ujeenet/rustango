#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "admin"))]
//! #1788 — an operator-console request logs one access-log line, with or
//! without `Builder::observability`, and so does the standalone console.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::Request;
use rustango::access_log::AccessLogLayer;
use rustango::sql::sqlx;
use tower::ServiceExt;

type Builder = rustango::server::Builder<sqlx::Sqlite>;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Access-log lines one apex `GET /login?next=…` writes through `app`.
async fn access_lines(app: axum::Router) -> Vec<String> {
    let buf = Capture::default();
    let writer = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .with_max_level(tracing::Level::INFO)
        .with_target(true)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let req = Request::builder()
        .uri("/login?next=%2Fsecret-path")
        .header("host", "localhost")
        .body(Body::empty())
        .unwrap();
    app.oneshot(req).await.unwrap();
    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    out.lines()
        .filter(|l| l.contains("rustango::access_log"))
        .map(str::to_owned)
        .collect()
}

/// One line, and the console's `next` stays redacted.
fn assert_one_redacted(lines: &[String]) {
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(!lines[0].contains("secret-path"), "{lines:?}");
}

async fn built(observability: bool) -> (axum::Router, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
    let pool = sqlx::SqlitePool::connect(&url).await.expect("connect");
    let mut b = Builder::from_pool(pool, url, "localhost");
    if observability {
        b = b.observability(Some(AccessLogLayer::new()));
    }
    (b.into_router().await.expect("assemble"), tmp)
}

#[tokio::test]
async fn the_builder_logs_a_console_request_once() {
    let (app, _tmp) = built(true).await;
    assert_one_redacted(&access_lines(app).await);
}

#[tokio::test]
async fn without_observability_the_console_still_logs_once() {
    let (app, _tmp) = built(false).await;
    assert_one_redacted(&access_lines(app).await);
}

#[tokio::test]
async fn the_standalone_console_logs_once() {
    let pool = rustango::sql::Pool::connect("sqlite::memory:")
        .await
        .expect("sqlite");
    let secret = rustango::session::SessionSecret::from_bytes(vec![7; 32]);
    let app = rustango::tenancy::operator_console::router(pool, secret);
    assert_one_redacted(&access_lines(app).await);
}
