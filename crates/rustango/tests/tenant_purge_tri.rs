//! Purging a tenant with an extra host deletes the Org on every backend (#1930).

#![cfg(all(feature = "tenancy", feature = "testkit"))]

use rustango::core::Column as _;
use rustango::sql::{Auto, FetcherPool as _, Pool};
use rustango::tenancy::decommission::{decommission, Action, Report};
use rustango::tenancy::{Org, OrgHost, TenancyError, TenantPools};
use rustango::tri_dialect_test;

async fn setup(pool: &Pool) {
    // Shared registry tables: create what is missing, never drop them.
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
}

async fn purge_on(pool: &Pool, slug: &str) -> Result<Report, TenancyError> {
    let action = Action::Purge {
        purge_database: true,
    };
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(p) => decommission(&TenantPools::new(p.clone()), slug, action).await,
        #[cfg(feature = "mysql")]
        Pool::Mysql(p) => decommission(&TenantPools::new(p.clone()), slug, action).await,
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(p) => decommission(&TenantPools::new(p.clone()), slug, action).await,
    }
}

async fn tenant_with_host(pool: &Pool, tag: &str) -> (String, i64) {
    let slug = format!("purge-{tag}-{}", std::process::id());
    let mut org = Org {
        slug: slug.clone(),
        display_name: slug.clone(),
        // A database-mode org with a non-Postgres backend drops nothing;
        // the registry side is what this suite is about.
        backend_kind: "sqlite".into(),
        database_url: Some("sqlite::memory:".into()),
        ..rustango::testkit::org()
    };
    org.save_pool(pool).await.expect("insert org");
    let id = *org.id.get().expect("org id");
    let mut host = OrgHost {
        id: Auto::default(),
        org_id: id,
        hostname: format!("{slug}.example.test"),
        enabled: true,
        created_at: Auto::default(),
    };
    host.save_pool(pool).await.expect("insert host");
    (slug, id)
}

async fn purge_deletes_org_with_extra_host(pool: &Pool) {
    let (slug, id) = tenant_with_host(pool, "host").await;
    let report = purge_on(pool, &slug).await.expect("purge");
    assert!(report.row_deleted);
    let orgs: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(slug.clone()))
        .fetch(pool)
        .await
        .unwrap();
    assert!(orgs.is_empty(), "Org row survived the purge");
    let hosts: Vec<OrgHost> = OrgHost::objects()
        .where_(OrgHost::org_id.eq(id))
        .fetch(pool)
        .await
        .unwrap();
    assert!(hosts.is_empty(), "host rows survived the purge");
}

tri_dialect_test!(
    setup: setup,
    scenarios: [purge_deletes_org_with_extra_host]
);

/// Schema-mode purge drops the cached scoped pool too (#1930). PG-only.
#[cfg(feature = "postgres")]
#[tokio::test]
#[allow(irrefutable_let_patterns)]
async fn schema_purge_evicts_the_scoped_pool() {
    let _guard = rustango::testkit::matrix::live_lock().lock().await;
    let Some(pool) = rustango::testkit::matrix::Backend::Postgres.pool().await else {
        eprintln!("DATABASE_URL not set — skipping");
        return;
    };
    setup(&pool).await;
    let Pool::Postgres(pg) = &pool else {
        unreachable!()
    };
    let slug = format!("purge-schema-{}", std::process::id());
    rustango::sql::sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS \"{slug}\""))
        .execute(pg)
        .await
        .unwrap();
    let mut org = Org {
        slug: slug.clone(),
        display_name: slug.clone(),
        storage_mode: "schema".into(),
        schema_name: Some(slug.clone()),
        ..rustango::testkit::org()
    };
    org.save_pool(&pool).await.expect("insert org");
    let pools = TenantPools::new(pg.clone());
    pools.scoped_pool(&org).await.expect("scoped pool");
    assert_eq!(pools.cached_scoped_pool_count().await, 1);

    let report = decommission(
        &pools,
        &slug,
        Action::Purge {
            purge_database: false,
        },
    )
    .await
    .expect("purge");
    assert_eq!(report.schema_dropped.as_deref(), Some(slug.as_str()));
    assert_eq!(
        pools.cached_scoped_pool_count().await,
        0,
        "scoped pool kept after purge"
    );
}

/// Another pod's idle connection does not block a database-mode purge (#2291).
#[cfg(feature = "postgres")]
#[tokio::test]
#[allow(irrefutable_let_patterns)]
async fn database_purge_drops_a_database_another_pod_holds_open() {
    use rustango::sql::sqlx::{Connection as _, PgConnection};
    let _guard = rustango::testkit::matrix::live_lock().lock().await;
    let Some(pool) = rustango::testkit::matrix::Backend::Postgres.pool().await else {
        eprintln!("DATABASE_URL not set — skipping");
        return;
    };
    setup(&pool).await;
    let Pool::Postgres(pg) = &pool else {
        unreachable!()
    };
    let slug = format!("purge-db-{}", std::process::id());
    let db = format!("rustango_purge_db_{}", std::process::id());
    let _ = rustango::sql::sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"))
        .execute(pg)
        .await;
    rustango::sql::sqlx::query(&format!("CREATE DATABASE \"{db}\""))
        .execute(pg)
        .await
        .unwrap();
    let url = std::env::var("DATABASE_URL").unwrap();
    let tenant_url = format!("{}/{db}", url.rsplit_once('/').unwrap().0);
    let mut org = Org {
        slug: slug.clone(),
        display_name: slug.clone(),
        backend_kind: "postgres".into(),
        database_url: Some(tenant_url.clone()),
        ..rustango::testkit::org()
    };
    org.save_pool(&pool).await.expect("insert org");
    let _other_pod = PgConnection::connect(&tenant_url).await.unwrap();

    let report = purge_on(&pool, &slug).await.expect("purge");
    assert!(report.database_dropped.is_some());
    let left: i64 =
        rustango::sql::sqlx::query_scalar("SELECT count(*) FROM pg_database WHERE datname = $1")
            .bind(&db)
            .fetch_one(pg)
            .await
            .unwrap();
    assert_eq!(left, 0, "the tenant database survived the purge");
}

/// A row pointing at the registry's own database is never force-dropped (#2291).
#[cfg(feature = "postgres")]
#[tokio::test]
#[allow(irrefutable_let_patterns)]
async fn database_purge_refuses_the_registry_database() {
    let _guard = rustango::testkit::matrix::live_lock().lock().await;
    let Some(pool) = rustango::testkit::matrix::Backend::Postgres.pool().await else {
        eprintln!("DATABASE_URL not set — skipping");
        return;
    };
    setup(&pool).await;
    let Pool::Postgres(pg) = &pool else {
        unreachable!()
    };
    let slug = format!("purge-registry-{}", std::process::id());
    let mut org = Org {
        slug: slug.clone(),
        display_name: slug.clone(),
        backend_kind: "postgres".into(),
        database_url: Some(std::env::var("DATABASE_URL").unwrap()),
        ..rustango::testkit::org()
    };
    org.save_pool(&pool).await.expect("insert org");

    let err = purge_on(&pool, &slug)
        .await
        .expect_err("registry drop must be refused");
    assert!(err.to_string().contains("registry"), "{err}");
    let one: i32 = rustango::sql::sqlx::query_scalar("SELECT 1")
        .fetch_one(pg)
        .await
        .expect("the registry database survived");
    assert_eq!(one, 1);
    org.delete_pool(&pool).await.unwrap();
}
