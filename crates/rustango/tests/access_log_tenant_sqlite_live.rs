#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "testkit"))]
//! Backing test for `docs/logging.md` — the access-log and tenant sections.
//!
//! A log line says *which* tenant the request was for — issue #1463.
//!
//! Tenant identity is an axum extractor, so it lives in the request and
//! dies with it. Every access-log line named the method, path, status
//! and IP, and no line in the framework named the tenant: investigating
//! "tenant A saw tenant B's data" meant grepping logs that could not
//! distinguish the two.
//!
//! `tenant_log` holds a slot open across the handler that the resolver
//! fills, so the middleware outside can read back what was resolved
//! inside. These tests drive a real router through a real registry —
//! the assertion is on the rendered log line, not on the plumbing.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::routing::get;
use axum::Router;
use http::request::Parts;
use http::Request;
use rustango::access_log::{AccessLogLayer, AccessLogRouterExt, TenantField};
use rustango::sql::Pool;
use rustango::tenancy::{ChainResolver, OrgResolver};
use tower::ServiceExt;

/// The host cache and the registry breaker are process-global, so these
/// tests cannot run unserialised — a neighbour's `reset_org_cache()`
/// lands between this test's seed and its resolve.
fn lock() -> &'static tokio::sync::Mutex<()> {
    static M: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Captures `tracing` output for assertion.
#[derive(Clone, Default)]
struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

async fn registry() -> (Pool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("reg.db").display());
    let pool = Pool::connect(&url).await.expect("sqlite");
    rustango::testkit::migrate_framework(&pool)
        .await
        .expect("framework tables");
    rustango::testkit::reset_org_cache();
    rustango::testkit::reset_registry_breaker();
    let mut org = rustango::tenancy::Org {
        id: rustango::sql::Auto::Unset,
        slug: "acme".into(),
        display_name: "Acme".into(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: Some("sqlite::memory:".into()),
        host_pattern: Some("acme.app.test".into()),
        ..rustango::testkit::org()
    };
    org.insert_pool(&pool).await.expect("insert org");
    (pool, dir)
}

fn parts_for_host(host: &str) -> Parts {
    Request::builder()
        .uri("/")
        .header("host", host)
        .body(())
        .expect("request")
        .into_parts()
        .0
}

/// A router whose handler resolves the tenant, which is what the
/// `Tenant` extractor does on a real request. No logging of its own —
/// the span test asserts on span output and must not be able to read
/// the access log's field by accident.
fn resolving_router(pool: Pool) -> Router {
    let pool = Arc::new(pool);
    Router::new().route(
        "/",
        get(move |req: Request<Body>| {
            let pool = Arc::clone(&pool);
            async move {
                let (parts, _) = req.into_parts();
                let _ = ChainResolver::standard("app.test")
                    .resolve(&parts, &pool)
                    .await;
                "ok"
            }
        }),
    )
}

fn app(pool: Pool, layer: AccessLogLayer) -> Router {
    resolving_router(pool).access_log(layer)
}

/// Drive one request and return what the subscriber wrote.
async fn log_line_for(host: &str, layer: AccessLogLayer) -> String {
    let (pool, _dir) = registry().await;
    let buf = CaptureWriter::default();
    let writer = buf.clone();
    // `with_ansi(false)`: enabling the `ansi` feature (#1480) made
    // `fmt` colour by default, and escape codes land *between* the
    // characters a substring assertion looks for — `tenant=acme`
    // becomes `\x1b[3mtenant\x1b[0m\x1b[2m=\x1b[0macme`. A test that
    // reads rendered output must ask for plain text.
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .with_max_level(tracing::Level::INFO)
        .with_target(true)
        .finish();
    // Thread-local default; `#[tokio::test]` is a current-thread runtime,
    // so the whole request is polled on this thread.
    let _guard = tracing::subscriber::set_default(subscriber);

    let res = app(pool, layer)
        .oneshot(
            Request::builder()
                .uri("/")
                .header("host", host)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), 200);

    let bytes = buf.0.lock().unwrap().clone();
    String::from_utf8(bytes).unwrap_or_default()
}

/// The funnel: every request path resolves through `ChainResolver`, so
/// recording there is what makes the identity available downstream.
#[tokio::test]
async fn the_resolver_publishes_the_tenant_to_the_log_context() {
    let _g = lock().lock().await;
    let (pool, _dir) = registry().await;

    let seen = rustango::tenant_log::scope(async {
        let org = ChainResolver::standard("app.test")
            .resolve(&parts_for_host("acme.app.test"), &pool)
            .await
            .expect("resolve");
        assert!(org.is_some(), "the seeded host must resolve");
        rustango::tenant_log::current()
    })
    .await;

    let seen = seen.expect("the resolver must publish the tenant it found");
    assert_eq!(seen.slug, "acme");
    assert!(seen.id.is_some(), "a persisted org carries its id");
}

/// The headline. Without the scope held open across the handler this
/// reads `tenant=-`: the resolver runs below the middleware, so there
/// is nothing for the log line to read unless the slot outlives it.
#[tokio::test]
async fn an_access_log_line_names_the_tenant() {
    let _g = lock().lock().await;
    let out = log_line_for("acme.app.test", AccessLogLayer::default()).await;
    assert!(
        out.contains("tenant=acme"),
        "expected the resolved tenant on the access-log line, got:\n{out}"
    );
    assert!(
        out.contains("rustango::access_log"),
        "expected the access_log target, got:\n{out}"
    );
}

/// An apex request legitimately has no tenant. It must be visibly
/// distinct from one whose identity went missing.
#[tokio::test]
async fn an_unresolved_request_logs_a_dash() {
    let _g = lock().lock().await;
    let out = log_line_for("app.test", AccessLogLayer::default()).await;
    assert!(
        out.contains("tenant=-"),
        "an apex request must log `tenant=-`, not a blank field, got:\n{out}"
    );
}

/// The slug is operator-chosen and often the customer's name; a
/// deployment shipping logs off-site can label by id instead.
#[tokio::test]
async fn tenant_field_id_logs_the_org_id_instead_of_the_slug() {
    let _g = lock().lock().await;
    let out = log_line_for(
        "acme.app.test",
        AccessLogLayer::default().tenant_field(TenantField::Id),
    )
    .await;
    assert!(
        !out.contains("tenant=acme"),
        "the slug must not appear when labelling by id, got:\n{out}"
    );
    assert!(
        out.contains("tenant=1"),
        "expected the org id on the line, got:\n{out}"
    );
}

/// The other half of the design: with `TracingLayer` installed the
/// tenant lands on the request span, so every event emitted after
/// resolution — the ORM's included — carries it in span context
/// without any subsystem knowing what a tenant is.
#[cfg(feature = "admin")]
#[tokio::test]
async fn the_request_span_carries_the_tenant() {
    use rustango::tracing_layer::TracingLayer;
    use tower::ServiceBuilder;

    let _g = lock().lock().await;
    let (pool, _dir) = registry().await;
    let buf = CaptureWriter::default();
    let writer = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .with_max_level(tracing::Level::INFO)
        // Print the span on close, when every recorded field is set.
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let svc = ServiceBuilder::new()
        .layer(TracingLayer::new())
        .service(resolving_router(pool));
    let res = svc
        .oneshot(
            Request::builder()
                .uri("/")
                .header("host", "acme.app.test")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), 200);

    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap_or_default();
    assert!(
        out.contains("http.request{"),
        "expected the request span in the output, got:\n{out}"
    );
    assert!(
        out.contains("tenant=\"acme\""),
        "expected the tenant recorded on the http.request span, got:\n{out}"
    );
}

#[tokio::test]
async fn tenant_field_off_never_labels() {
    let _g = lock().lock().await;
    let out = log_line_for(
        "acme.app.test",
        AccessLogLayer::default().tenant_field(TenantField::Off),
    )
    .await;
    assert!(
        out.contains("tenant=-"),
        "`Off` must not label even a resolved tenant, got:\n{out}"
    );
}
