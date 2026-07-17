//! Pure-SQLite stack regression test — zero Postgres in the binary.
//!
//! Locks in the v0.34 Phase B.1+B.2 achievement: a downstream app can
//! wire `DatabaseTenantContext<Sqlite>` with `registry: Pool::Sqlite`,
//! mount the `DatabaseTenant<Sqlite>` extractor, and serve real HTTP
//! requests against an in-memory SQLite registry and per-tenant
//! SQLite databases — without depending on Postgres at runtime.
//!
//! Equivalent to what the `sqlite_tenant_demo` example shows + what
//! the Playwright walk-through verified manually, but executable in
//! CI as a normal `cargo test` step.

#![cfg(all(feature = "tenancy", feature = "sqlite"))]

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::header::HOST;
use axum::http::StatusCode;
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use rustango::extractors::{DatabaseTenant, DatabaseTenantContext};
use rustango::sql::sqlx::{self, Row};
use rustango::tenancy::{
    session::SessionSecret, BackendKind, ChainResolver, DatabasePools, Org, OrgResolver,
    TenancyError,
};
use tower::ServiceExt;

/// Resolver that maps `Host: <slug>.localhost` → synthetic Org.
/// Doesn't touch the registry pool (which can therefore be any
/// backend — sqlite, in this test).
#[derive(Clone)]
struct HostResolver;

#[async_trait::async_trait]
impl OrgResolver for HostResolver {
    async fn resolve(
        &self,
        parts: &axum::http::request::Parts,
        _registry: &rustango::sql::Pool,
    ) -> Result<Option<Org>, TenancyError> {
        let host = parts
            .headers
            .get(HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        // No dot → no subdomain → not a tenant request (apex-shape).
        if !host.contains('.') {
            return Ok(None);
        }
        let slug = host.split('.').next().unwrap_or("");
        if slug.is_empty() {
            return Ok(None);
        }
        Ok(Some(Org {
            id: rustango::sql::Auto::default(),
            slug: slug.to_owned(),
            display_name: slug.to_owned(),
            storage_mode: "database".into(),
            backend_kind: "sqlite".into(),
            database_url: None,
            schema_name: None,
            host_pattern: None,
            port: None,
            path_prefix: None,
            active: true,
            created_at: chrono::Utc::now(),
            brand_name: None,
            brand_tagline: None,
            logo_path: None,
            favicon_path: None,
            primary_color: None,
            theme_mode: None,
            sso_enabled: false,
            sso_provider: None,
            sso_issuer_url: None,
            sso_client_id: None,
            sso_secret_ref: None,
        }))
    }
}

async fn handler(mut t: DatabaseTenant<sqlx::Sqlite>) -> impl IntoResponse {
    sqlx::query("CREATE TABLE IF NOT EXISTS posts (id INTEGER PRIMARY KEY)")
        .execute(&mut ***t.conn())
        .await
        .expect("create");
    sqlx::query("INSERT INTO posts DEFAULT VALUES")
        .execute(&mut ***t.conn())
        .await
        .expect("insert");
    let row = sqlx::query("SELECT COUNT(*) as n FROM posts")
        .fetch_one(&mut ***t.conn())
        .await
        .expect("count");
    let n: i64 = row.try_get("n").expect("n");
    format!("{}:{n}", t.org.slug)
}

/// Build a router with a SQLite-everywhere stack. Mirrors what
/// `examples/sqlite_tenant_demo.rs` does. `ns` is a per-test
/// namespace so concurrent tokio tests don't share `cache=shared`
/// SQLite databases (every test must isolate to assert counts
/// independently).
fn build_app(ns: &str) -> Router {
    let template = format!("sqlite:file:purestack_{ns}_{{slug}}?mode=memory&cache=shared");
    let pools = Arc::new(
        DatabasePools::<sqlx::Sqlite>::new(BackendKind::Sqlite).with_url_template(&template),
    );

    let resolver = ChainResolver::new().push(HostResolver);

    let ctx = Arc::new(DatabaseTenantContext {
        pools,
        resolver,
        session_secret: SessionSecret::from_bytes(b"test_tenant_secret_____32bytes!!".to_vec()),
        operator_secret: SessionSecret::from_bytes(b"test_oper_secret_______32bytes!!".to_vec()),
        // Registry pool is itself SQLite — proves zero PG anywhere.
        registry: rustango::sql::Pool::Sqlite(
            sqlx::SqlitePool::connect_lazy("sqlite::memory:").expect("lazy sqlite pool"),
        ),
    });

    Router::new()
        .route("/whoami", get(handler))
        .layer(middleware::from_fn(
            move |mut req: Request, next: middleware::Next| {
                let ctx = ctx.clone();
                async move {
                    req.extensions_mut().insert(ctx);
                    next.run(req).await
                }
            },
        ))
}

async fn call(app: Router, host: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .uri("/whoami")
        .header(HOST, host)
        .body(Body::empty())
        .expect("build req");
    let resp = app.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    let text = std::str::from_utf8(&bytes).expect("utf8").to_owned();
    (status, text)
}

#[tokio::test]
async fn two_tenants_get_separate_databases() {
    let app = build_app("test1");
    let (status_a, body_a) = call(app.clone(), "acme.localhost").await;
    assert_eq!(status_a, StatusCode::OK);
    assert_eq!(body_a, "acme:1");

    let (status_b, body_b) = call(app, "beta.localhost").await;
    assert_eq!(status_b, StatusCode::OK);
    assert_eq!(body_b, "beta:1");
}

#[tokio::test]
async fn pool_cache_persists_within_one_app() {
    // Two requests to the SAME app instance hit the SAME pool, so the
    // second request to `acme` should see the row inserted by the
    // first.
    let app = build_app("test2");
    let (_, body1) = call(app.clone(), "acme.localhost").await;
    assert_eq!(body1, "acme:1");
    let (_, body2) = call(app, "acme.localhost").await;
    assert_eq!(body2, "acme:2");
}

#[tokio::test]
async fn unknown_host_returns_404() {
    // Apex (no dot) → resolver returns None → extractor rejects with
    // 404 (the `NotFound` rejection).
    let (status, _) = call(build_app("test3"), "localhost").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
