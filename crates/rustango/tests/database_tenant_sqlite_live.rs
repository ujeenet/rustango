//! End-to-end live test for the `DatabaseTenant<Sqlite>` extractor.
//!
//! Boots a minimal axum router with the
//! [`DatabaseTenantContext<Sqlite>`] installed, fakes the resolver
//! into returning a single in-memory SQLite org, and verifies that a
//! handler can both read the org and run a query through the
//! tenant's connection.
//!
//! This is the "the extractor actually works end-to-end" test;
//! pairs with the unit-level tests in
//! `database_pools_sqlite_live.rs` which exercise the pool registry
//! directly.

#![cfg(all(feature = "tenancy", feature = "sqlite"))]

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use rustango::extractors::{DatabaseTenant, DatabaseTenantContext};
use rustango::sql::sqlx::{self, Row};
use rustango::tenancy::{BackendKind, ChainResolver, DatabasePools, Org, OrgResolver};
use tower::ServiceExt;

/// Synthetic resolver that always returns the same baked-in Org.
/// Lets the test bypass the registry-DB query the standard resolver
/// chain would issue.
#[derive(Clone)]
struct FixedResolver(Org);

#[async_trait::async_trait]
impl OrgResolver for FixedResolver {
    async fn resolve(
        &self,
        _parts: &axum::http::request::Parts,
        _registry: &sqlx::PgPool,
    ) -> Result<Option<Org>, rustango::tenancy::TenancyError> {
        Ok(Some(self.0.clone()))
    }
}

fn fake_sqlite_org() -> Org {
    Org {
        id: rustango::sql::Auto::default(),
        slug: "acme".into(),
        display_name: "Acme".into(),
        storage_mode: "database".into(),
        backend_kind: "sqlite".into(),
        database_url: Some("sqlite::memory:".into()),
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
    }
}

/// A handler that returns `{slug}:{count}` after running a real query
/// on the tenant's SQLite connection. Proves both halves of the
/// extractor wiring — org metadata and live connection.
async fn handler(mut t: DatabaseTenant<sqlx::Sqlite>) -> impl IntoResponse {
    // The conn deref chain is: DatabaseConn -> PoolConnection ->
    // SqliteConnection. sqlx::Executor is implemented for
    // `&mut SqliteConnection`, so we need three levels of deref.
    let conn = t.conn();
    let row = sqlx::query("SELECT 42 as answer")
        .fetch_one(&mut ***conn)
        .await
        .expect("query");
    let answer: i32 = row.try_get("answer").expect("read answer");
    format!("{}:{}", t.org.slug, answer)
}

#[tokio::test]
async fn full_request_path_through_extractor() {
    let pools: Arc<DatabasePools<sqlx::Sqlite>> = Arc::new(DatabasePools::new(BackendKind::Sqlite));

    let resolver = ChainResolver::new().push(FixedResolver(fake_sqlite_org()));

    // Build a minimal context. The session secrets are unused in this
    // test (no auth on the route), but the type forces us to provide
    // them. Use throw-away dev bytes.
    let ctx = Arc::new(DatabaseTenantContext {
        pools,
        resolver,
        session_secret: rustango::tenancy::operator_console::SessionSecret::from_bytes(
            b"test_tenant_session_secret_32by!".to_vec(),
        ),
        operator_secret: rustango::tenancy::operator_console::SessionSecret::from_bytes(
            b"test_oper_session_secret____32b!".to_vec(),
        ),
        // The resolver above never touches the registry pool, but the
        // type still requires us to pass one. A lazy pool against an
        // unreachable address never connects — works as a placeholder.
        registry: sqlx::PgPool::connect_lazy("postgres://127.0.0.1:1/none").expect("lazy pg pool"),
    });

    let app: Router =
        Router::new()
            .route("/whoami", get(handler))
            .layer(axum::middleware::from_fn(
                move |mut req: Request, next: axum::middleware::Next| {
                    let ctx = ctx.clone();
                    async move {
                        req.extensions_mut().insert(ctx);
                        next.run(req).await
                    }
                },
            ));

    let req = Request::builder()
        .uri("/whoami")
        .body(Body::empty())
        .expect("build req");
    let resp = app.oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    let text = std::str::from_utf8(&body).expect("utf8");
    assert_eq!(text, "acme:42");
}
