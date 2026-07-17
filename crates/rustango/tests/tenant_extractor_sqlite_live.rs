//! v0.38 — proof that `Tenant<sqlx::Sqlite>` extracts at runtime
//! through `TenantPools<sqlx::Sqlite>::database_acquire`. Pairs with
//! `tenant_pools_sqlite_live.rs` (unit-level pool test) +
//! `database_tenant_sqlite_live.rs` (DatabaseTenant<Sqlite> path).
//!
//! Confirms `fn handler(t: Tenant<sqlx::Sqlite>)` resolves the org,
//! checks out a sqlite connection, and answers a real query — no
//! Postgres in the binary.

#![cfg(all(feature = "sqlite", feature = "tenancy", not(feature = "postgres")))]

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use rustango::extractors::{Tenant, TenantContext};
use rustango::sql::sqlx::{self, Row};
use rustango::tenancy::{
    session::SessionSecret, ChainResolver, Org, OrgResolver, TenancyError, TenantPools,
};
use tower::ServiceExt;

/// Synthetic resolver: returns a single fixed sqlite org for any
/// request. Bypasses the registry-lookup path the production
/// resolver chain uses.
#[derive(Clone)]
struct FixedResolver(Org);

#[async_trait::async_trait]
impl OrgResolver for FixedResolver {
    async fn resolve(
        &self,
        _parts: &axum::http::request::Parts,
        _registry: &rustango::sql::Pool,
    ) -> Result<Option<Org>, TenancyError> {
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
        database_url: Some(
            "sqlite:file:tenant_extractor_sqlite_test?mode=memory&cache=shared".into(),
        ),
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
    }
}

async fn handler(mut t: Tenant<sqlx::Sqlite>) -> impl IntoResponse {
    let conn = t.pool_conn().await.expect("acquire tenant connection");
    let row = sqlx::query("SELECT 42 as answer")
        .fetch_one(&mut **conn)
        .await
        .expect("query");
    let answer: i64 = row.try_get("answer").expect("answer");
    format!("{}:{}", t.org.slug, answer)
}

#[tokio::test]
async fn tenant_sqlite_extractor_resolves_and_queries() {
    let registry: sqlx::SqlitePool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("registry pool");
    let pools: TenantPools<sqlx::Sqlite> = TenantPools::new(registry);

    let resolver = ChainResolver::new().push(FixedResolver(fake_sqlite_org()));

    let ctx = Arc::new(TenantContext::<sqlx::Sqlite> {
        pools: Arc::new(pools),
        resolver,
        session_secret: SessionSecret::from_bytes(b"test_tenant_session_secret_32by!".to_vec()),
        operator_secret: SessionSecret::from_bytes(b"test_oper_session_secret____32b!".to_vec()),
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
