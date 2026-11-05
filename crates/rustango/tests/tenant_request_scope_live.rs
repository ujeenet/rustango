#![cfg(all(
    feature = "tenancy",
    feature = "sessions",
    feature = "sqlite",
    feature = "testkit"
))]
//! One `RequestTenant` per request, on its own host and under apex +
//! `/<prefix>/`, where every tenant shares one cookie: the dispatcher marks
//! it, `CacheScope` scopes sessions and flags by it, `KeyBy::Tenant` counts
//! by it, and the unresolved router is `Global`.
//!
//! PG runs schema mode (skips without `DATABASE_URL`), SQLite and MySQL
//! database mode (MySQL skips without `MYSQL_TEST_URL`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::Query;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use rustango::cache::{BoxedCache, CacheScope, InMemoryCache};
use rustango::cache_page::CachePageLayer;
use rustango::idempotency::{IdempotencyLayer, IdempotencyRouterExt as _};
use rustango::rate_limit::KeyBy;
use rustango::rate_limit_cache::{CacheRateLimitLayer, CacheRateLimitRouterExt as _};
use rustango::sessions::Session;
use rustango::sql::{sqlx, Auto};
use rustango::tenancy::{Org, RequestTenant, TenantPools, TenantSlug};
use tower::ServiceExt as _;

#[cfg(any(feature = "postgres", feature = "mysql"))]
#[path = "support/scratch_db.rs"]
mod scratch_db;

/// The org cache is process-global.
fn lock() -> &'static tokio::sync::Mutex<()> {
    static M: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[derive(serde::Deserialize)]
struct Op {
    op: String,
    #[serde(default)]
    user: String,
}

/// What the dispatcher stored, before anything in the app resolves.
fn marker(req: &Request<Body>) -> String {
    match RequestTenant::get(req.extensions()) {
        Some(RequestTenant::Tenant(t)) => format!("tenant:{}", t.slug),
        Some(RequestTenant::Global) => "global".into(),
        Some(_) => "unknown".into(),
        None => "none".into(),
    }
}

/// `/page` and `/<prefix>/page` behind the page cache: the marker and a render count.
fn pages(root: &BoxedCache) -> Router {
    let renders = Arc::new(AtomicUsize::new(0));
    let render = move |req: Request<Body>| {
        let n = renders.fetch_add(1, Ordering::SeqCst);
        async move { format!("{} #{n}", marker(&req)) }
    };
    Router::new()
        .route("/page", get(render.clone()))
        .route("/{prefix}/page", get(render))
        .layer(CachePageLayer::new(root.clone()))
}

/// A counter a keyed POST bumps once per key and tenant.
fn runs() -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(0))
}

fn run(n: &AtomicUsize) -> String {
    format!("run #{}", n.fetch_add(1, Ordering::SeqCst))
}

/// One shared root cache, as an app holds it; the handler scopes per request.
fn api(root: BoxedCache) -> Router {
    let limited =
        CacheRateLimitLayer::new(root.clone(), 1, Duration::from_secs(3600)).key_by(KeyBy::Tenant);
    let rl = Router::new()
        .route("/rl", get(|| async { "ok" }))
        .route("/{prefix}/rl", get(|| async { "ok" }))
        .cache_rate_limit(limited);
    let (idem, n) = (IdempotencyLayer::new(root.clone()), runs());
    rl.merge(pages(&root))
        .fallback(move |Query(q): Query<Op>, req: Request<Body>| {
            let (root, n) = (root.clone(), n.clone());
            async move {
                if q.op == "run" {
                    return run(&n);
                }
                let seen = marker(&req);
                let (mut parts, _) = req.into_parts();
                let sid = parts
                    .headers
                    .get("cookie")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|c| c.strip_prefix("rustango_session="))
                    .unwrap_or("")
                    .to_owned();
                let scope = CacheScope::from_request(&root, &mut parts)
                    .await
                    .expect("a resolved tenant");
                let (sessions, flags) = (scope.sessions(), scope.flags());
                match q.op.as_str() {
                    "marker" => seen,
                    "login" => {
                        let mut s = Session::new();
                        s.set("user_id", q.user.clone());
                        sessions.save(&s).await.unwrap()
                    }
                    "whoami" => match sessions.load(&sid).await.unwrap() {
                        Some(s) => s.get::<String>("user_id").unwrap_or_default(),
                        None => "anonymous".into(),
                    },
                    "beta_on" => {
                        flags.enable_for_user("beta", &q.user).await.unwrap();
                        "ok".into()
                    }
                    "beta" => flags.is_enabled_for("beta", &q.user).await.to_string(),
                    _ => "?".into(),
                }
            }
        })
        .idempotency(idem)
}

/// The Builder's no-tenant router: says what it was marked; a POST runs.
fn unresolved(root: &BoxedCache) -> Router {
    let n = runs();
    pages(root)
        .fallback(move |req: Request<Body>| {
            let n = n.clone();
            async move {
                if req.method() == axum::http::Method::POST {
                    run(&n)
                } else {
                    marker(&req)
                }
            }
        })
        .idempotency(IdempotencyLayer::new(root.clone()))
}

/// How a request reaches a tenant.
#[derive(Clone, Copy, Debug)]
enum Routing {
    OwnHost,
    ApexPrefix,
}

impl Routing {
    fn request(self, slug: &str, path: &str, sid: Option<&str>) -> Request<Body> {
        self.builder(slug, path, sid).body(Body::empty()).unwrap()
    }

    /// A POST with `Idempotency-Key: k`.
    fn keyed(self, slug: &str, path: &str) -> Request<Body> {
        self.builder(slug, path, None)
            .method("POST")
            .header("idempotency-key", "k")
            .body(Body::empty())
            .unwrap()
    }

    fn builder(self, slug: &str, path: &str, sid: Option<&str>) -> axum::http::request::Builder {
        let (host, uri) = match self {
            Self::OwnHost => (format!("{slug}.localhost"), path.to_owned()),
            Self::ApexPrefix => ("localhost".to_owned(), format!("/{slug}{path}")),
        };
        let mut b = Request::builder().uri(uri).header("host", host);
        if let Some(sid) = sid {
            b = b.header("cookie", format!("rustango_session={sid}"));
        }
        b
    }
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, String) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

async fn call(app: &Router, req: Request<Body>) -> String {
    let (status, body) = send(app, req).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// `a` and `b` are two tenants of `app`, which shares one root cache.
async fn scenario(app: &Router, a: &str, b: &str) {
    let mut leaks = Vec::new();
    // An outer layer's marker naming another tenant is overwritten.
    let forged = app.clone().layer(axum::middleware::from_fn(
        |mut req: Request<Body>, next: axum::middleware::Next| async move {
            req.extensions_mut()
                .insert(RequestTenant::Tenant(TenantSlug::new(999, "forged")));
            next.run(req).await
        },
    ));
    for routing in [Routing::OwnHost, Routing::ApexPrefix] {
        let go = |slug: &str, q: &str, sid: Option<&str>| {
            call(app, routing.request(slug, &format!("/probe?{q}"), sid))
        };
        for (what, app) in [("", app), ("forged ", &forged)] {
            let marked = call(app, routing.request(a, "/probe?op=marker", None)).await;
            assert_eq!(marked, format!("tenant:{a}"), "{what}{routing:?}");
            assert_eq!(
                call(app, routing.request("nobody", "/probe", None)).await,
                "global",
                "{what}{routing:?}: the unresolved router"
            );
        }

        // Idempotency: the tenant's own retry replays; the other tenant runs.
        let first = call(app, routing.keyed(a, "/probe?op=run")).await;
        let again = call(app, routing.keyed(a, "/probe?op=run")).await;
        assert_eq!(again, first, "{routing:?}: own retry");
        if call(app, routing.keyed(b, "/probe?op=run")).await == first {
            leaks.push(format!("{routing:?}: {b} replayed {a}'s response"));
        }
        // Page cache: a hit inside the tenant, not across.
        let page = call(app, routing.request(a, "/page", None)).await;
        assert!(page.starts_with(&format!("tenant:{a} ")), "{page}");
        assert_eq!(call(app, routing.request(a, "/page", None)).await, page);
        if call(app, routing.request(b, "/page", None)).await == page {
            leaks.push(format!("{routing:?}: {b} got {a}'s page"));
        }
        // The unresolved router is Global: one page and one replay for all.
        let page = call(app, routing.request("nobody", "/page", None)).await;
        assert!(page.starts_with("global "), "{page}");
        assert_eq!(
            call(app, routing.request("nobody", "/page", None)).await,
            page
        );
        let first = call(app, routing.keyed("nobody", "/probe")).await;
        assert_eq!(call(app, routing.keyed("nobody", "/probe")).await, first);

        // Each works inside its own tenant, so a miss below is not vacuous.
        let sid = go(a, "op=login&user=5", None).await;
        assert_eq!(go(a, "op=whoami", Some(&sid)).await, "5", "{routing:?}");
        if go(b, "op=whoami", Some(&sid)).await != "anonymous" {
            leaks.push(format!("{routing:?}: {a}'s session loaded under {b}"));
        }

        let user = format!("{routing:?}");
        go(a, &format!("op=beta_on&user={user}"), None).await;
        assert_eq!(go(a, &format!("op=beta&user={user}"), None).await, "true");
        if go(b, &format!("op=beta&user={user}"), None).await != "false" {
            leaks.push(format!(
                "{routing:?}: {a}'s flag override applied under {b}"
            ));
        }
    }
    // One bucket per tenant, whichever way it was reached.
    let rl = |slug: &str, routing: Routing| send(app, routing.request(slug, "/rl", None));
    assert_eq!(rl(a, Routing::OwnHost).await.0, StatusCode::OK);
    assert_eq!(
        rl(a, Routing::ApexPrefix).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    if rl(b, Routing::ApexPrefix).await.0 != StatusCode::OK {
        leaks.push(format!("{a}'s rate limit spent {b}'s"));
    }
    assert!(leaks.is_empty(), "{leaks:#?}");
}

/// With the registry down a new host's tenant is `Unknown`: a keyed POST
/// gets 503 and its page is never cached. Run last: it closes the registry.
async fn failed_resolution(app: &Router, registry: impl std::future::Future<Output = ()>) {
    registry.await;
    let (status, body) = send(app, Routing::OwnHost.keyed("zzz", "/probe?op=run")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let page = |n| send(app, Routing::OwnHost.request(n, "/page", None));
    let (one, two) = (page("zzz").await.1, page("zzz").await.1);
    assert!(one.starts_with("unknown "), "{one}");
    assert_ne!(one, two, "a page cached for an unknown tenant");
}

fn org(slug: &str, storage: &str, backend: &str, url: Option<String>) -> Org {
    Org {
        id: Auto::default(),
        slug: slug.to_owned(),
        display_name: slug.to_owned(),
        storage_mode: storage.into(),
        backend_kind: backend.into(),
        database_url: url,
        schema_name: (storage == "schema").then(|| slug.to_owned()),
        host_pattern: Some(format!("{slug}.localhost")),
        port: None,
        path_prefix: Some(format!("/{slug}")),
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

async fn migrate_registry<DB: sqlx::Database>(pools: &TenantPools<DB>, url: &str)
where
    rustango::sql::Pool: From<sqlx::Pool<DB>>,
{
    let migrations = tempfile::tempdir().unwrap();
    let mut buf: Vec<u8> = Vec::new();
    rustango::tenancy::manage::run_with_writer(
        pools,
        url,
        migrations.path(),
        vec!["migrate-registry".to_owned()],
        &mut buf,
    )
    .await
    .expect("migrate-registry");
}

#[tokio::test]
async fn sqlite_database_mode_scopes_by_the_request_tenant() {
    let _g = lock().lock().await;
    rustango::testkit::reset_org_cache();
    let tmp = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
    let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
    let pools = TenantPools::<sqlx::Sqlite>::new(pool.clone());
    migrate_registry(&pools, &url).await;
    for slug in ["acme", "globex"] {
        let db = format!(
            "sqlite://{}?mode=rwc",
            tmp.path().join(format!("{slug}.db")).display()
        );
        let mut o = org(slug, "database", "sqlite", Some(db));
        o.insert(&pools.registry_pool()).await.unwrap();
    }
    let root: BoxedCache = Arc::new(InMemoryCache::new());
    let app = rustango::server::Builder::from_pool(pool.clone(), url, "localhost")
        .api(api(root.clone()))
        .unresolved_tenant(unresolved(&root))
        .into_router()
        .await
        .expect("assemble");
    scenario(&app, "acme", "globex").await;
    failed_resolution(&app, pool.close()).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn pg_schema_mode_scopes_by_the_request_tenant() {
    let Ok(admin) = std::env::var("DATABASE_URL") else {
        return;
    };
    let _g = lock().lock().await;
    rustango::testkit::reset_org_cache();
    let db = scratch_db::ScratchDb::create(&admin, "tenant_sessions_flags").await;
    let pool = sqlx::PgPool::connect(db.url()).await.unwrap();
    let pools = TenantPools::<sqlx::Postgres>::new(pool.clone());
    migrate_registry(&pools, db.url()).await;
    for slug in ["acme", "globex"] {
        sqlx::query(&format!("CREATE SCHEMA {slug}"))
            .execute(&pool)
            .await
            .unwrap();
        let mut o = org(slug, "schema", "postgres", None);
        o.insert(&pools.registry_pool()).await.unwrap();
    }
    let root: BoxedCache = Arc::new(InMemoryCache::new());
    let app = rustango::server::Builder::from_pool(pool.clone(), db.url(), "localhost")
        .api(api(root.clone()))
        .unresolved_tenant(unresolved(&root))
        .into_router()
        .await
        .expect("assemble");
    scenario(&app, "acme", "globex").await;
    failed_resolution(&app, pool.close()).await;
    drop(app);
    drop(pools);
}

/// MySQL database mode; skips when `MYSQL_TEST_URL` is unset.
#[cfg(feature = "mysql")]
#[tokio::test]
async fn mysql_database_mode_scopes_by_the_request_tenant() {
    let Ok(admin) = std::env::var("MYSQL_TEST_URL") else {
        return;
    };
    let _g = lock().lock().await;
    rustango::testkit::reset_org_cache();
    let db = scratch_db::ScratchDb::create(&admin, "rustango_req_tenant").await;
    let pool = sqlx::MySqlPool::connect(db.url()).await.unwrap();
    let pools = TenantPools::<sqlx::MySql>::new(pool.clone());
    migrate_registry(&pools, db.url()).await;
    for slug in ["acme", "globex"] {
        // Nothing here reads the tenant database, so both share the scratch one.
        let mut o = org(slug, "database", "mysql", Some(db.url().to_owned()));
        o.insert(&pools.registry_pool()).await.unwrap();
    }
    let root: BoxedCache = Arc::new(InMemoryCache::new());
    let app = rustango::server::Builder::from_pool(pool.clone(), db.url(), "localhost")
        .api(api(root.clone()))
        .unresolved_tenant(unresolved(&root))
        .into_router()
        .await
        .expect("assemble");
    scenario(&app, "acme", "globex").await;
    failed_resolution(&app, pool.close()).await;
    drop(app);
    drop(pools);
}
