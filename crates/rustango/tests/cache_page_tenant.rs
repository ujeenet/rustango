//! `CachePageLayer` must not share a page between tenants that the
//! resolver tells apart by something other than `Host` (#1674).
//!
//! One Host, the standard resolver chain, `X-Org` picks the tenant.

#![cfg(all(feature = "cache-page", feature = "tenancy", feature = "sqlite"))]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Request};
use axum::routing::get;
use axum::{Extension, Router};
use http_body_util::BodyExt as _;
use rustango::cache::InMemoryCache;
use rustango::cache_page::CachePageLayer;
use rustango::extractors::DatabaseTenantContext;
use rustango::sql::{sqlx, Pool};
use rustango::tenancy::{session::SessionSecret, BackendKind, ChainResolver, DatabasePools, Org};
use tower::ServiceExt as _;

async fn registry() -> Pool {
    let pool: Pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect")
        .into();
    rustango::testkit::create_tables_for::<Org>(&pool)
        .await
        .expect("orgs table");
    for slug in ["acme", "globex"] {
        let mut org = Org {
            slug: slug.to_owned(),
            display_name: slug.to_owned(),
            backend_kind: "sqlite".into(),
            ..rustango::testkit::org()
        };
        org.insert_pool(&pool).await.expect("insert org");
    }
    pool
}

async fn context() -> Arc<DatabaseTenantContext<sqlx::Sqlite>> {
    Arc::new(DatabaseTenantContext {
        pools: Arc::new(DatabasePools::<sqlx::Sqlite>::new(BackendKind::Sqlite)),
        resolver: ChainResolver::standard("app.test"),
        session_secret: SessionSecret::from_bytes(b"test_tenant_secret_____32bytes!!".to_vec()),
        operator_secret: SessionSecret::from_bytes(b"test_oper_secret_______32bytes!!".to_vec()),
        registry: registry().await,
    })
}

/// The page names the tenant it was rendered for, and counts renders.
fn page(hits: Arc<AtomicU32>) -> Router {
    Router::new().route(
        "/page",
        get(move |headers: HeaderMap| {
            let hits = hits.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                headers
                    .get("x-org")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_owned()
            }
        }),
    )
}

async fn fetch(app: &Router, org: &str) -> String {
    let req = Request::get("/page")
        .header("host", "app.test")
        .header("x-org", org)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}

/// The builder's order: the tenant context wraps the user's layers.
#[tokio::test]
async fn header_resolved_tenants_get_their_own_page() {
    let hits = Arc::new(AtomicU32::new(0));
    let app = page(hits.clone())
        .layer(CachePageLayer::new(Arc::new(InMemoryCache::new())))
        .layer(Extension(context().await));

    assert_eq!(fetch(&app, "acme").await, "acme");
    assert_eq!(
        fetch(&app, "globex").await,
        "globex",
        "globex got acme's page"
    );
    assert_eq!(fetch(&app, "acme").await, "acme");
    assert_eq!(hits.load(Ordering::SeqCst), 2, "same tenant still hits");
}

/// Outside the tenant context the layer cannot tell tenants apart, so
/// it must not serve a shared hit.
#[tokio::test]
async fn no_tenant_context_means_no_shared_hit() {
    let hits = Arc::new(AtomicU32::new(0));
    let app = page(hits.clone())
        .layer(Extension(context().await))
        .layer(CachePageLayer::new(Arc::new(InMemoryCache::new())));

    assert_eq!(fetch(&app, "acme").await, "acme");
    assert_eq!(
        fetch(&app, "globex").await,
        "globex",
        "globex got acme's page"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

/// `tenant_agnostic` restores plain Host-keyed caching.
#[tokio::test]
async fn tenant_agnostic_route_caches_without_a_context() {
    let hits = Arc::new(AtomicU32::new(0));
    let app = page(hits.clone())
        .layer(CachePageLayer::new(Arc::new(InMemoryCache::new())).tenant_agnostic(true));

    assert_eq!(fetch(&app, "acme").await, "acme");
    assert_eq!(fetch(&app, "acme").await, "acme");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}
