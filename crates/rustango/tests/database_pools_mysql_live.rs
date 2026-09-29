//! Live MySQL tests for `DatabasePools<MySql>`.
//!
//! Env-gated on `MYSQL_TEST_URL`, the name every other MySQL suite
//! reads and the one CI sets. Mirrors the in-memory SQLite live tests
//! in `database_pools_sqlite_live.rs` so the two backends stay
//! behaviorally aligned. Skips silently when the env var is unset
//! — `MYSQL_TEST_URL=mysql://user:pass@localhost:3306/rustango_test
//! cargo test --features mysql,...` enables them.
//!
//! It read `MYSQL_URL` until #1415. Nothing sets that, so the two
//! tests below that need a server had never run anywhere — while the
//! file still reported `4 passed`, because the other two need no
//! database and pass on their own.
//!
//! The tests run against ONE shared database (not per-test) for
//! simplicity; they only do `SELECT 1`-style queries that don't
//! mutate schema, so re-running is safe.

#![cfg(all(feature = "tenancy", feature = "mysql"))]

use rustango::sql::sqlx::{self, Row};
use rustango::tenancy::{BackendKind, DatabasePools, Org};

fn fake_mysql_org(slug: &str, url: &str) -> Org {
    Org {
        slug: slug.to_owned(),
        display_name: slug.to_owned(),
        backend_kind: "mysql".into(),
        database_url: Some(url.to_owned()),
        ..rustango::testkit::org()
    }
}

#[tokio::test]
async fn acquire_returns_working_mysql_connection() {
    let Ok(url) = std::env::var("MYSQL_TEST_URL") else {
        eprintln!("MYSQL_TEST_URL not set — skipping");
        return;
    };
    let pools: DatabasePools<sqlx::MySql> = DatabasePools::new(BackendKind::MySql);
    let org = fake_mysql_org("acme", &url);

    let mut conn = pools.acquire(&org).await.expect("acquire mysql connection");
    // DatabaseConn → PoolConnection<MySql> → MySqlConnection; query
    // wants `&mut MySqlConnection`.
    let row = sqlx::query("SELECT 1 as one")
        .fetch_one(&mut **conn)
        .await
        .expect("query SELECT 1");
    let one: i32 = row.try_get("one").expect("read one");
    assert_eq!(one, 1);
}

#[tokio::test]
async fn pool_cached_on_repeat_acquire() {
    let Ok(url) = std::env::var("MYSQL_TEST_URL") else {
        eprintln!("MYSQL_TEST_URL not set — skipping");
        return;
    };
    let pools: DatabasePools<sqlx::MySql> = DatabasePools::new(BackendKind::MySql);
    let org = fake_mysql_org("acme", &url);

    let first = pools.pool_for_org(&org).await.expect("first acquire");
    let second = pools.pool_for_org(&org).await.expect("second acquire");
    assert!(std::sync::Arc::ptr_eq(
        &first.pool_arc(),
        &second.pool_arc()
    ));
}

/// `Tenant<MySql>` resolves from a `DatabaseTenantContext`; `t.pool()` and
/// the deferred `pool_conn()` both reach the tenant's database (#1802).
#[tokio::test]
async fn tenant_extractor_reads_the_database_tenant_context() {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use rustango::extractors::{DatabaseTenantContext, Tenant};
    use rustango::sql::Pool;
    use rustango::tenancy::session::SessionSecret;
    use rustango::tenancy::{ChainResolver, OrgResolver, TenancyError};
    use tower::ServiceExt as _;

    struct FixedResolver(Org);

    #[async_trait::async_trait]
    impl OrgResolver for FixedResolver {
        async fn resolve(
            &self,
            _parts: &axum::http::request::Parts,
            _registry: &Pool,
        ) -> Result<Option<Org>, TenancyError> {
            Ok(Some(self.0.clone()))
        }
    }

    let Ok(url) = std::env::var("MYSQL_TEST_URL") else {
        eprintln!("MYSQL_TEST_URL not set — skipping");
        return;
    };
    let secret = SessionSecret::from_bytes(b"mysql-tenant-extractor-secret-32".to_vec());
    let ctx = Arc::new(DatabaseTenantContext::<sqlx::MySql> {
        pools: Arc::new(DatabasePools::new(BackendKind::MySql)),
        resolver: ChainResolver::new().push(FixedResolver(fake_mysql_org("acme", &url))),
        session_secret: secret.clone(),
        operator_secret: secret,
        registry: Pool::Mysql(sqlx::MySqlPool::connect_lazy(&url).expect("registry")),
    });
    let app = axum::Router::new()
        .route(
            "/",
            axum::routing::get(|mut t: Tenant<sqlx::MySql>| async move {
                #[allow(unreachable_patterns)] // single-variant in mysql-only builds
                let via_pool: i64 = match t.pool() {
                    Pool::Mysql(p) => sqlx::query_scalar("SELECT 1").fetch_one(p).await,
                    _ => panic!("t.pool() is not MySQL"),
                }
                .expect("t.pool()");
                let conn = t.pool_conn().await.expect("deferred conn");
                let via_conn: i64 = sqlx::query_scalar("SELECT 2")
                    .fetch_one(&mut **conn)
                    .await
                    .expect("pool_conn()");
                format!("{}:{via_pool}:{via_conn}", t.org.slug)
            }),
        )
        .layer(axum::middleware::from_fn(
            move |mut req: Request<Body>, next: axum::middleware::Next| {
                let ctx = ctx.clone();
                async move {
                    req.extensions_mut().insert(ctx);
                    next.run(req).await
                }
            },
        ));
    let r = app
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let b = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
    assert_eq!(std::str::from_utf8(&b).unwrap(), "acme:1:2");
}

#[tokio::test]
async fn rejects_postgres_org() {
    // No DB connection required for this validation path — runs even
    // without MYSQL_TEST_URL.
    let pools: DatabasePools<sqlx::MySql> = DatabasePools::new(BackendKind::MySql);
    let mut org = fake_mysql_org("acme", "mysql://nobody:nothing@127.0.0.1:0/none");
    org.backend_kind = "postgres".into();

    let err = pools.pool_for_org(&org).await.expect_err("should reject");
    assert!(
        err.to_string().contains("postgres") && err.to_string().contains("mysql"),
        "error should name both backends, got: {err}"
    );
}

#[tokio::test]
async fn rejects_schema_mode() {
    // Same — no DB needed.
    let pools: DatabasePools<sqlx::MySql> = DatabasePools::new(BackendKind::MySql);
    let mut org = fake_mysql_org("acme", "mysql://nobody:nothing@127.0.0.1:0/none");
    org.storage_mode = "schema".into();

    let err = pools.pool_for_org(&org).await.expect_err("should reject");
    let msg = err.to_string();
    assert!(
        msg.contains("database-mode") || msg.contains("Schema-mode"),
        "error should explain database-mode-only, got: {msg}"
    );
}
