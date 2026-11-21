#![cfg(all(feature = "tenancy", feature = "sqlite", feature = "testkit"))]
//! `server::Builder::atomic_requests`: a write request runs in one block on
//! its tenant's pool, and every tenant extractor in the request serves that
//! same pool, even when the pool cache evicts it mid-request.
//!
//! PG runs schema mode (skips without `DATABASE_URL`), SQLite and MySQL
//! database mode (MySQL skips without `MYSQL_TEST_URL`).

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Query;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use rustango::atomic_request::AtomicRequests;
use rustango::extractors::{Tenant, TenantScope};
use rustango::sql::{in_atomic, raw_execute_pool, raw_query_pool, sqlx, ExecError, Pool};
use rustango::tenancy::{Org, TenancyError, TenantPools, TenantPoolsConfig};
use tower::ServiceExt as _;

#[cfg(any(feature = "postgres", feature = "mysql"))]
#[path = "support/scratch_db.rs"]
mod scratch_db;

/// The org cache is process-global.
fn lock() -> &'static tokio::sync::Mutex<()> {
    static M: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| tokio::sync::Mutex::new(()))
}

const TABLE: &str = "areqt_item";

#[derive(serde::Deserialize)]
struct Op {
    #[serde(default = "created")]
    status: u16,
}

fn created() -> u16 {
    201
}

async fn insert(pool: &Pool, id: i64) -> Result<u64, ExecError> {
    let sql = format!(
        "INSERT INTO {} (id) VALUES ({id})",
        pool.dialect().quote_ident(TABLE)
    );
    raw_execute_pool(pool, &sql, vec![]).await
}

/// Two writes through `TenantScope`, then `status`.
async fn write(scope: TenantScope, Query(op): Query<Op>) -> Response {
    let in_block = in_atomic(scope.pool());
    insert(scope.pool(), 1).await.unwrap();
    insert(scope.pool(), 2).await.unwrap();
    let status = StatusCode::from_u16(op.status).unwrap();
    (status, [("x-in-block", in_block.to_string())]).into_response()
}

/// Inside the layer, before the extractors: with `?evict=true`, look up the
/// other tenant's pool, which evicts this one from a cache of one, and
/// report whether a fresh lookup of this tenant's pool is still the block's.
async fn evict<DB: sqlx::Database>(
    pools: Arc<TenantPools<DB>>,
    orgs: Arc<Vec<Org>>,
    req: Request<Body>,
    next: axum::middleware::Next,
) -> Response
where
    Pool: From<sqlx::Pool<DB>>,
{
    if !req.uri().query().unwrap_or("").contains("evict=true") {
        return next.run(req).await;
    }
    let host = req.headers()["host"].to_str().unwrap().to_owned();
    let (own, other): (Vec<&Org>, Vec<&Org>) = orgs
        .iter()
        .partition(|o| host.starts_with(&format!("{}.", o.slug)));
    pools.scoped_pool_dyn(other[0]).await.unwrap();
    let fresh = pools.scoped_pool_dyn(own[0]).await.unwrap();
    let evicted = !in_atomic(&fresh);
    let mut resp = next.run(req).await;
    resp.headers_mut()
        .insert("x-evicted", evicted.to_string().parse().unwrap());
    resp
}

/// `Tenant::conn` lends the block's connection: its write rolls back with
/// the request. `pool_conn` and `into_conn` are refused in the block.
macro_rules! conn_route {
    ($db:ty) => {
        |mut t: Tenant<$db>, Query(op): Query<Op>| async move {
            let sql = format!("INSERT INTO {TABLE} (id) VALUES (5)");
            sqlx::query(&sql)
                .execute(&mut *t.conn().await.unwrap())
                .await
                .unwrap();
            // `pool()` gives the connection back, so ORM calls run again.
            insert(t.pool(), 6).await.unwrap();
            let refused = |r: Result<(), TenancyError>| {
                matches!(r, Err(TenancyError::Exec(ExecError::InAtomic { .. })))
            };
            let pool_conn = refused(t.pool_conn().await.map(drop));
            let into_conn = refused(t.into_conn().await.map(drop));
            let status = StatusCode::from_u16(op.status).unwrap();
            (
                status,
                format!("pool_conn={pool_conn} into_conn={into_conn}"),
            )
                .into_response()
        }
    };
}

/// The app's routes for a `DB` tenancy server.
macro_rules! api {
    ($db:ty, $pools:expr, $orgs:expr) => {{
        let (pools, orgs) = ($pools.clone(), $orgs.clone());
        Router::new()
            .route("/w", post(write))
            .route("/conn", post(conn_route!($db)))
            .layer(axum::middleware::from_fn(move |req, next| {
                evict::<$db>(pools.clone(), orgs.clone(), req, next)
            }))
    }};
}

fn org(slug: &str, storage: &str, backend: &str, url: Option<String>) -> Org {
    let mut o = Org::new(slug.to_owned());
    o.storage_mode = storage.parse().unwrap();
    o.backend_kind = backend.parse().unwrap();
    o.database_url = url;
    o.schema_name = (storage == "schema").then(|| slug.to_owned());
    o.host_pattern = Some(format!("{slug}.localhost"));
    o
}

/// A cache of one pool per kind, so one lookup for another tenant evicts.
fn cap_one() -> TenantPoolsConfig {
    TenantPoolsConfig {
        max_cached_database_pools: 1,
        max_cached_scoped_pools: 1,
        scoped_pool_max_connections: 8,
        schema_tenant_roles: false,
        ..TenantPoolsConfig::default()
    }
}

async fn migrate_registry<DB: sqlx::Database>(pools: &TenantPools<DB>, url: &str)
where
    Pool: From<sqlx::Pool<DB>>,
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

async fn post_to(app: &Router, slug: &str, path: &str) -> Response {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", format!("{slug}.localhost"))
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}

async fn ids(tenant: &Pool, table: &str) -> Vec<i64> {
    let sql = format!("SELECT id FROM {table} ORDER BY id");
    let rows: Vec<(i64,)> = raw_query_pool(&sql, vec![], tenant).await.unwrap();
    rows.into_iter().map(|r| r.0).collect()
}

async fn clear(tenant: &Pool, table: &str) {
    raw_execute_pool(tenant, &format!("DELETE FROM {table}"), vec![])
        .await
        .unwrap();
}

/// `tenant` reads `a`'s rows; `table` names them from there.
async fn scenario(app: &Router, a: &str, tenant: &Pool, table: &str) {
    // A failed request leaves the tenant's table empty; a 201 keeps both rows.
    for status in [500, 400, 403] {
        let resp = post_to(app, a, &format!("/w?status={status}")).await;
        assert_eq!(resp.status().as_u16(), status);
        assert_eq!(resp.headers()["x-in-block"], "true");
        assert_eq!(
            ids(tenant, table).await,
            Vec::<i64>::new(),
            "{status} kept its writes"
        );
    }
    assert_eq!(post_to(app, a, "/w").await.status(), StatusCode::CREATED);
    assert_eq!(ids(tenant, table).await, vec![1, 2]);
    clear(tenant, table).await;

    // Evicted between the layer and the extractor: still the block's pool.
    let resp = post_to(app, a, "/w?status=500&evict=true").await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(resp.headers()["x-evicted"], "true", "nothing was evicted");
    assert_eq!(
        resp.headers()["x-in-block"],
        "true",
        "the extractor's pool is not the block's"
    );
    assert_eq!(
        ids(tenant, table).await,
        Vec::<i64>::new(),
        "writes after an eviction escaped the block"
    );

    // `Tenant::conn` writes on the block: rolled back by a 500, kept by a 201.
    let resp = post_to(app, a, "/conn?status=500").await;
    let body = axum::body::to_bytes(resp.into_body(), 256).await.unwrap();
    assert_eq!(&body[..], b"pool_conn=true into_conn=true");
    assert_eq!(
        ids(tenant, table).await,
        Vec::<i64>::new(),
        "t.conn() escaped the block"
    );
    post_to(app, a, "/conn?status=201").await;
    assert_eq!(ids(tenant, table).await, vec![5, 6]);
    clear(tenant, table).await;
}

/// A tenant pool cache of 0 is refused at boot under `atomic_requests`.
async fn refuses_an_uncached_pool<DB: sqlx::Database>(registry: sqlx::Pool<DB>, url: &str)
where
    Pool: From<sqlx::Pool<DB>>,
{
    let cfg = TenantPoolsConfig {
        max_cached_database_pools: 0,
        ..cap_one()
    };
    let built = rustango::server::Builder::from_pool(registry, url, "localhost")
        .tenant_pools(cfg)
        .atomic_requests(AtomicRequests::new())
        .into_router()
        .await;
    let err = built.err().expect("booted with an uncached tenant pool");
    assert!(err.to_string().contains("cached tenant pools"), "{err}");
}

#[tokio::test]
async fn sqlite_database_mode_runs_each_write_in_the_tenant_block() {
    let _g = lock().lock().await;
    rustango::testkit::reset_org_cache();
    let tmp = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display());
    let registry = sqlx::SqlitePool::connect(&url).await.unwrap();
    let builder = rustango::server::Builder::from_pool(registry.clone(), url.clone(), "localhost")
        .tenant_pools(cap_one())
        .atomic_requests(AtomicRequests::new());
    let pools = builder.pools().clone();
    migrate_registry(&pools, &url).await;
    let mut orgs = Vec::new();
    let mut tenants = Vec::new();
    for slug in ["acme", "globex"] {
        let db = format!(
            "sqlite://{}?mode=rwc",
            tmp.path().join(format!("{slug}.db")).display()
        );
        let tenant = Pool::connect(&db).await.unwrap();
        raw_execute_pool(
            &tenant,
            &format!("CREATE TABLE {TABLE} (id BIGINT PRIMARY KEY)"),
            vec![],
        )
        .await
        .unwrap();
        tenants.push(tenant);
        let mut o = org(slug, "database", "sqlite", Some(db));
        o.insert(&pools.registry_pool()).await.unwrap();
        orgs.push(o);
    }
    let orgs = Arc::new(orgs);
    let app = builder
        .api(api!(sqlx::Sqlite, pools, orgs))
        .into_router()
        .await
        .expect("assemble");
    scenario(&app, "acme", &tenants[0], TABLE).await;
    refuses_an_uncached_pool(registry, &url).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn pg_schema_mode_runs_each_write_in_the_tenant_block() {
    use rustango::testkit::matrix::Backend;
    let Some(admin) =
        Backend::Postgres.admin_url("pg_schema_mode_runs_each_write_in_the_tenant_block")
    else {
        return;
    };
    let _g = lock().lock().await;
    rustango::testkit::reset_org_cache();
    let db = scratch_db::ScratchDb::create(&admin, "rustango_areq_tenant").await;
    let registry = sqlx::PgPool::connect(db.url()).await.unwrap();
    let builder = rustango::server::Builder::from_pool(registry.clone(), db.url(), "localhost")
        .tenant_pools(cap_one())
        .atomic_requests(AtomicRequests::new());
    let pools = builder.pools().clone();
    migrate_registry(&pools, db.url()).await;
    let reg = Pool::from(registry.clone());
    let mut orgs = Vec::new();
    for slug in ["acme", "globex"] {
        for sql in [
            format!("CREATE SCHEMA {slug}"),
            format!("CREATE TABLE {slug}.{TABLE} (id BIGINT PRIMARY KEY)"),
        ] {
            raw_execute_pool(&reg, &sql, vec![]).await.unwrap();
        }
        let mut o = org(slug, "schema", "postgres", None);
        o.insert(&pools.registry_pool()).await.unwrap();
        orgs.push(o);
    }
    let orgs = Arc::new(orgs);
    let app = builder
        .api(api!(sqlx::Postgres, pools, orgs))
        .into_router()
        .await
        .expect("assemble");
    scenario(&app, "acme", &reg, &format!("acme.{TABLE}")).await;
    refuses_an_uncached_pool(registry.clone(), db.url()).await;
    drop(app);
    registry.close().await;
}

/// MySQL database mode; skips when `MYSQL_TEST_URL` is unset.
#[cfg(feature = "mysql")]
#[tokio::test]
async fn mysql_database_mode_runs_each_write_in_the_tenant_block() {
    use rustango::testkit::matrix::Backend;
    let Some(admin) =
        Backend::MySql.admin_url("mysql_database_mode_runs_each_write_in_the_tenant_block")
    else {
        return;
    };
    let _g = lock().lock().await;
    rustango::testkit::reset_org_cache();
    let db = scratch_db::ScratchDb::create(&admin, "rustango_areq_reg").await;
    let registry = sqlx::MySqlPool::connect(db.url()).await.unwrap();
    let builder = rustango::server::Builder::from_pool(registry.clone(), db.url(), "localhost")
        .tenant_pools(cap_one())
        .atomic_requests(AtomicRequests::new());
    let pools = builder.pools().clone();
    migrate_registry(&pools, db.url()).await;
    let mut orgs = Vec::new();
    let mut tenants = Vec::new();
    let mut dbs = Vec::new();
    for slug in ["acme", "globex"] {
        let tdb = scratch_db::ScratchDb::create(&admin, &format!("rustango_areq_{slug}")).await;
        let tenant = Pool::connect(tdb.url()).await.unwrap();
        raw_execute_pool(
            &tenant,
            &format!("CREATE TABLE {TABLE} (id BIGINT PRIMARY KEY)"),
            vec![],
        )
        .await
        .unwrap();
        tenants.push(tenant);
        let mut o = org(slug, "database", "mysql", Some(tdb.url().to_owned()));
        o.insert(&pools.registry_pool()).await.unwrap();
        orgs.push(o);
        dbs.push(tdb);
    }
    let orgs = Arc::new(orgs);
    let app = builder
        .api(api!(sqlx::MySql, pools, orgs))
        .into_router()
        .await
        .expect("assemble");
    scenario(&app, "acme", &tenants[0], TABLE).await;
    refuses_an_uncached_pool(registry.clone(), db.url()).await;
    drop(app);
    for t in tenants {
        t.close().await;
    }
    registry.close().await;
}
