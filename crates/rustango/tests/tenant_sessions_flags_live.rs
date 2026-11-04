#![cfg(all(
    feature = "tenancy",
    feature = "sessions",
    feature = "sqlite",
    feature = "testkit"
))]
//! A session or flag override saved under one tenant stays there, on its own
//! host and under apex + `/<prefix>/`, where every tenant shares one cookie.
//!
//! PG runs schema mode and skips when `DATABASE_URL` is unset; SQLite runs database mode.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::Query;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::Router;
use rustango::cache::{BoxedCache, InMemoryCache};
use rustango::extractors::TenantScope;
use rustango::feature_flags::FeatureFlags;
use rustango::sessions::{Session, SessionStore};
use rustango::sql::{sqlx, Auto};
use rustango::tenancy::{Org, TenantPools, TenantSlug};
use tower::ServiceExt as _;

#[cfg(feature = "postgres")]
#[path = "support/scratch_db.rs"]
mod scratch_db;

/// The org cache is process-global.
fn lock() -> &'static tokio::sync::Mutex<()> {
    static M: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// The stores a handler uses for the request's tenant.
fn stores(root: &BoxedCache, t: &TenantScope) -> (SessionStore, FeatureFlags) {
    let tenant = TenantSlug::of(&t.org).expect("registry org");
    (
        SessionStore::new(root.clone()).for_tenant(&tenant),
        FeatureFlags::new(root.clone()).for_tenant(&tenant),
    )
}

#[derive(serde::Deserialize)]
struct Op {
    op: String,
    #[serde(default)]
    user: String,
}

/// One shared root cache, as an app holds it; the handler scopes per request.
fn api(root: BoxedCache) -> Router {
    Router::new().fallback(move |t: TenantScope, Query(q): Query<Op>, h: HeaderMap| {
        let root = root.clone();
        async move {
            let (sessions, flags) = stores(&root, &t);
            let sid = h
                .get("cookie")
                .and_then(|v| v.to_str().ok())
                .and_then(|c| c.strip_prefix("rustango_session="))
                .unwrap_or("")
                .to_owned();
            match q.op.as_str() {
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
}

/// How a request reaches a tenant.
#[derive(Clone, Copy, Debug)]
enum Routing {
    OwnHost,
    ApexPrefix,
}

impl Routing {
    fn request(self, slug: &str, query: &str, sid: Option<&str>) -> Request<Body> {
        let (host, uri) = match self {
            Self::OwnHost => (format!("{slug}.localhost"), format!("/probe?{query}")),
            Self::ApexPrefix => ("localhost".to_owned(), format!("/{slug}/probe?{query}")),
        };
        let mut b = Request::builder().uri(uri).header("host", host);
        if let Some(sid) = sid {
            b = b.header("cookie", format!("rustango_session={sid}"));
        }
        b.body(Body::empty()).unwrap()
    }
}

async fn call(app: &Router, req: Request<Body>) -> String {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let body = String::from_utf8_lossy(&body).into_owned();
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// `a` and `b` are two tenants of `app`, which shares one root cache.
async fn scenario(app: &Router, a: &str, b: &str) {
    let mut leaks = Vec::new();
    for routing in [Routing::OwnHost, Routing::ApexPrefix] {
        let go = |slug: &str, q: &str, sid: Option<&str>| call(app, routing.request(slug, q, sid));
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
    assert!(leaks.is_empty(), "{leaks:#?}");
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
async fn sqlite_database_mode_keeps_sessions_and_flags_per_tenant() {
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
    let app = rustango::server::Builder::from_pool(pool, url, "localhost")
        .api(api(root))
        .into_router()
        .await
        .expect("assemble");
    scenario(&app, "acme", "globex").await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn pg_schema_mode_keeps_sessions_and_flags_per_tenant() {
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
        .api(api(root))
        .into_router()
        .await
        .expect("assemble");
    scenario(&app, "acme", "globex").await;
    drop(app);
    drop(pools);
    pool.close().await;
}
