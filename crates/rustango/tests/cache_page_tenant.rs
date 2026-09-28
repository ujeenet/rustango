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
use rustango::tenancy::{
    session::SessionSecret, BackendKind, ChainResolver, DatabasePools, Org, OrgResolver,
    TenancyError,
};
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
    context_with(ChainResolver::standard("app.test")).await
}

async fn context_with(resolver: ChainResolver) -> Arc<DatabaseTenantContext<sqlx::Sqlite>> {
    Arc::new(DatabaseTenantContext {
        pools: Arc::new(DatabasePools::<sqlx::Sqlite>::new(BackendKind::Sqlite)),
        resolver,
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

/// Errs on `x-fail`, else matches no tenant; counts calls.
struct TestResolver(Arc<AtomicU32>);

#[async_trait::async_trait]
impl OrgResolver for TestResolver {
    async fn resolve(
        &self,
        parts: &axum::http::request::Parts,
        _registry: &Pool,
    ) -> Result<Option<Org>, TenancyError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        if parts.headers.contains_key("x-fail") {
            return Err(TenancyError::Resolution("boom".into()));
        }
        Ok(None)
    }
}

fn app_with(hits: Arc<AtomicU32>, ctx: Arc<DatabaseTenantContext<sqlx::Sqlite>>) -> Router {
    page(hits)
        .layer(CachePageLayer::new(Arc::new(InMemoryCache::new())))
        .layer(Extension(ctx))
}

async fn get_with(app: &Router, header: Option<(&str, &str)>) {
    let mut req = Request::get("/page").header("host", "app.test");
    if let Some((k, v)) = header {
        req = req.header(k, v);
    }
    app.clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
}

/// A resolver error must not produce a shared hit.
#[tokio::test]
async fn resolver_error_is_not_cached() {
    let hits = Arc::new(AtomicU32::new(0));
    let resolver = ChainResolver::new().push(TestResolver(Arc::new(AtomicU32::new(0))));
    let app = app_with(hits.clone(), context_with(resolver).await);
    get_with(&app, Some(("x-fail", "1"))).await;
    get_with(&app, Some(("x-fail", "1"))).await;
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

/// No matching tenant is still cacheable, under "no tenant".
#[tokio::test]
async fn no_matching_tenant_is_cached() {
    let hits = Arc::new(AtomicU32::new(0));
    let resolver = ChainResolver::new().push(TestResolver(Arc::new(AtomicU32::new(0))));
    let app = app_with(hits.clone(), context_with(resolver).await);
    get_with(&app, None).await;
    get_with(&app, None).await;
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

/// A `TenantSlug` already on the request is used; the resolver is not asked.
#[tokio::test]
async fn tenant_slug_extension_skips_resolution() {
    use rustango::tenancy::TenantSlug;
    let calls = Arc::new(AtomicU32::new(0));
    let resolver = ChainResolver::new().push(TestResolver(calls.clone()));
    let ctx = context_with(resolver).await;
    let hits = Arc::new(AtomicU32::new(0));
    let layer = CachePageLayer::new(Arc::new(InMemoryCache::new()));
    let acme = page(hits.clone())
        .layer(layer.clone())
        .layer(Extension(TenantSlug("acme".into())))
        .layer(Extension(ctx.clone()));
    let globex = page(hits.clone())
        .layer(layer)
        .layer(Extension(TenantSlug("globex".into())))
        .layer(Extension(ctx));
    get_with(&acme, None).await;
    get_with(&acme, None).await;
    get_with(&globex, None).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0, "resolver was asked");
    assert_eq!(hits.load(Ordering::SeqCst), 2, "one render per tenant");
}

/// With no `Host` header (HTTP/2) the URI authority keys the page.
#[tokio::test]
async fn authority_keys_the_page_without_a_host_header() {
    let app = Router::new()
        .route(
            "/page",
            get(|uri: axum::http::Uri| async move {
                uri.authority().map(|a| a.to_string()).unwrap_or_default()
            }),
        )
        .layer(CachePageLayer::new(Arc::new(InMemoryCache::new())).tenant_agnostic(true));
    for host in ["a.test", "b.test"] {
        let req = Request::get(format!("http://{host}/page"))
            .body(Body::empty())
            .unwrap();
        let body = app.clone().oneshot(req).await.unwrap().into_body();
        let body = body.collect().await.unwrap().to_bytes();
        assert_eq!(body, host.as_bytes(), "{host} got another host's page");
    }
}

/// Mounted through the real server builder (`TenantContext` path).
#[tokio::test]
async fn server_builder_keys_the_page_on_the_tenant() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
    let sq = sqlx::SqlitePool::connect(&url).await.expect("connect");
    let pool = Pool::Sqlite(sq.clone());
    rustango::testkit::create_tables_for::<Org>(&pool)
        .await
        .expect("orgs");
    for slug in ["acme", "globex"] {
        let mut org = Org {
            slug: slug.to_owned(),
            display_name: slug.to_owned(),
            backend_kind: "sqlite".into(),
            ..rustango::testkit::org()
        };
        org.insert_pool(&pool).await.expect("insert org");
    }
    let hits = Arc::new(AtomicU32::new(0));
    let api = page(hits.clone()).layer(CachePageLayer::new(Arc::new(InMemoryCache::new())));
    let app = rustango::server::Builder::<sqlx::Sqlite>::from_pool(sq, url, "localhost")
        .api(api)
        .into_router()
        .await
        .expect("assemble");

    for org in ["acme", "globex", "acme"] {
        let req = Request::get("/page")
            .header("host", "shared.localhost")
            .header("x-org", org)
            .body(Body::empty())
            .unwrap();
        let body = app.clone().oneshot(req).await.unwrap().into_body();
        let body = body.collect().await.unwrap().to_bytes();
        assert_eq!(body, org.as_bytes(), "{org} got another tenant's page");
    }
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}
