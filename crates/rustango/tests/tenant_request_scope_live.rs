#![cfg(all(
    feature = "tenancy",
    feature = "sessions",
    feature = "sqlite",
    feature = "testkit"
))]
//! One `RequestTenant` per request, on its own host and under apex +
//! `/<prefix>/`, where every tenant shares one cookie: the dispatcher marks
//! it, `CacheScope` scopes sessions and flags by it, `KeyBy::Tenant` counts
//! by it, and the unresolved router is `Global`. App routes see the
//! tenant-relative path, tenant extractors serve the dispatcher's tenant,
//! and CSRF exemptions match the tenant-relative path.
//!
//! PG runs schema mode (skips without `DATABASE_URL`), SQLite and MySQL
//! database mode (MySQL skips without `MYSQL_TEST_URL`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::OriginalUri;
use axum::extract::Query;
use axum::http::{Request, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use rustango::cache::{BoxedCache, CacheScope, InMemoryCache};
use rustango::cache_page::CachePageLayer;
use rustango::extractors::TenantScope;
use rustango::idempotency::{IdempotencyLayer, IdempotencyRouterExt as _};
use rustango::rate_limit::KeyBy;
use rustango::rate_limit_cache::{CacheRateLimitLayer, CacheRateLimitRouterExt as _};
use rustango::sessions::Session;
use rustango::sql::sqlx;
use rustango::tenancy::{Org, RequestTenant, TenantPathPrefix, TenantPools, TenantSlug};
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
    // One route serves both routings: the dispatcher strips the prefix.
    let rl = Router::new()
        .route("/rl", get(|| async { "ok" }))
        .cache_rate_limit(limited);
    let csrf = rustango::forms::csrf::with_config(
        rustango::forms::csrf::CsrfConfig::default().exempt_prefix("/beacon"),
    );
    let posts = Router::new()
        .route("/beacon", post(|| async { "beacon" }))
        .route("/guarded", post(|| async { "guarded" }))
        .layer(csrf);
    let rl = rl
        .route(
            "/about",
            get(|req: Request<Body>| async move {
                let orig = req.extensions().get::<OriginalUri>().map(|o| o.0.clone());
                let prefix = TenantPathPrefix::get(req.extensions()).map_or("-", |p| p.as_str());
                format!(
                    "path={} orig={} prefix={prefix}",
                    req.uri().path(),
                    orig.map_or_else(String::new, |o| o.path().to_owned())
                )
            }),
        )
        .route("/who", get(|t: TenantScope| async move { t.org.slug }))
        .merge(posts);
    #[cfg(feature = "hmac-auth")]
    let rl = rl.merge(
        Router::new()
            .route("/signed", post(|| async { "signed" }))
            .layer(rustango::hmac_auth::HmacAuthLayer::new(Arc::new(
                |k: &str| (k == HMAC_KEY).then(|| HMAC_SECRET.to_vec()),
            ))),
    );
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

#[cfg(feature = "hmac-auth")]
const HMAC_KEY: &str = "k1";
#[cfg(feature = "hmac-auth")]
const HMAC_SECRET: &[u8] = b"hmac-secret-for-the-prefix-test!";

/// A POST to `path` on `slug`, signed for `signed_path` (the URL the client sees).
#[cfg(feature = "hmac-auth")]
fn signed_post(routing: Routing, slug: &str, path: &str, signed_path: &str) -> Request<Body> {
    let req = routing.request(slug, path, None);
    let host = req.headers()["host"].to_str().unwrap().to_owned();
    let (date, auth) =
        rustango::hmac_auth::sign_now(HMAC_KEY, HMAC_SECRET, "POST", &host, signed_path, "", b"");
    routing
        .builder(slug, path, None)
        .method("POST")
        .header("x-date", date)
        .header("authorization", auth)
        .body(Body::empty())
        .unwrap()
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

        // App routes see the tenant-relative path; the prefix stays readable.
        let pre = match routing {
            Routing::OwnHost => String::new(),
            Routing::ApexPrefix => format!("/{a}"),
        };
        let shown = if pre.is_empty() { "-" } else { pre.as_str() };
        assert_eq!(
            call(app, routing.request(a, "/about?x=1", None)).await,
            format!("path=/about orig={pre}/about prefix={shown}"),
            "{routing:?}"
        );
        // The extractor serves the dispatcher's tenant, never a forged one.
        for (what, app) in [("", app), ("forged ", &forged)] {
            assert_eq!(
                call(app, routing.request(a, "/who", None)).await,
                a,
                "{what}{routing:?}"
            );
        }
        // A CSRF exemption matches the tenant-relative path; a sibling is still checked.
        let post_to = |path: &str| {
            routing
                .builder(a, path, None)
                .method("POST")
                .body(Body::empty())
                .unwrap()
        };
        assert_eq!(
            send(app, post_to("/beacon")).await.0,
            StatusCode::OK,
            "{routing:?}"
        );
        assert_eq!(
            send(app, post_to("/guarded")).await.0,
            StatusCode::FORBIDDEN,
            "{routing:?}"
        );
        // HMAC signs the URL the client sent, prefix included, so a signature
        // for one tenant's path never passes on another's.
        #[cfg(feature = "hmac-auth")]
        {
            let real = format!("{pre}/signed");
            let ok = signed_post(routing, a, "/signed", &real);
            assert_eq!(
                send(app, ok).await,
                (StatusCode::OK, "signed".into()),
                "{routing:?}"
            );
            if matches!(routing, Routing::ApexPrefix) {
                let replay = signed_post(routing, b, "/signed", &real);
                assert_eq!(
                    send(app, replay).await.0,
                    StatusCode::UNAUTHORIZED,
                    "replay on {b}"
                );
                let stripped = signed_post(routing, a, "/signed", "/signed");
                assert_eq!(
                    send(app, stripped).await.0,
                    StatusCode::UNAUTHORIZED,
                    "stripped path"
                );
            }
        }
        // The tenant admin under the prefix keeps its prefixed paths.
        let gate = app
            .clone()
            .oneshot(routing.request(a, "/admin", None))
            .await
            .unwrap();
        assert_eq!(
            gate.headers()["location"].to_str().unwrap(),
            format!(
                "{pre}/login?next={}",
                rustango::url_codec::url_encode(&format!("{pre}/admin"))
            ),
            "{routing:?}"
        );

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
    let mut o = Org::new(slug.to_owned());
    o.storage_mode = storage.into();
    o.backend_kind = backend.into();
    o.database_url = url;
    o.schema_name = (storage == "schema").then(|| slug.to_owned());
    o.host_pattern = Some(format!("{slug}.localhost"));
    o.path_prefix = Some(format!("/{slug}"));
    o
}

/// The boot warning when path-prefix tenants share the console's apex.
const SHARED_APEX: &str = "path-prefix tenants share the apex origin";

/// `boot`'s output and the warnings it logged.
async fn boot_log<T>(boot: impl std::future::Future<Output = T>) -> (T, String) {
    use tracing::instrument::WithSubscriber as _;
    let buf = rustango::testkit::CaptureWriter::default();
    let sink = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || sink.clone())
        .with_max_level(tracing::Level::WARN)
        .finish();
    let out = boot.with_subscriber(subscriber).await;
    (out, buf.contents())
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
    let root: BoxedCache = Arc::new(InMemoryCache::new());
    let boot = |pool: sqlx::SqlitePool, url: String| {
        let root = root.clone();
        boot_log(async move {
            rustango::server::Builder::from_pool(pool, url, "localhost")
                .api(api(root.clone()))
                .unresolved_tenant(unresolved(&root))
                .into_router()
                .await
                .expect("assemble")
        })
    };
    let (_, log) = boot(pool.clone(), url.clone()).await;
    assert!(
        !log.contains(SHARED_APEX),
        "warned with no prefix tenant: {log}"
    );
    for slug in ["acme", "globex"] {
        let db = format!(
            "sqlite://{}?mode=rwc",
            tmp.path().join(format!("{slug}.db")).display()
        );
        let mut o = org(slug, "database", "sqlite", Some(db));
        o.insert(&pools.registry_pool()).await.unwrap();
    }
    // Path-prefix tenants share the console's origin: one warning at boot.
    let (app, log) = boot(pool.clone(), url).await;
    assert_eq!(log.matches(SHARED_APEX).count(), 1, "{log}");
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
