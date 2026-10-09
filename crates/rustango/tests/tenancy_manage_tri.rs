//! Tenancy manage verbs and `api::create_tenant`, on every registry backend.
//! Tenants are database-mode on the registry's backend.

#![cfg(all(feature = "tenancy", feature = "testkit", feature = "sqlite"))]

use std::path::Path;

use rustango::core::Column as _;
use rustango::sql::{FetcherPool as _, Pool};
use rustango::tenancy::manage::api::{create_tenant, CreateTenantOpts};
use rustango::tenancy::{BackendKind, Org, StorageMode, TenancyError, TenantPools};
use rustango::tri_dialect_test;

#[cfg(any(feature = "postgres", feature = "mysql"))]
#[path = "support/scratch_db.rs"]
mod scratch_db;

async fn setup(pool: &Pool) {
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
}

fn name(tag: &str) -> String {
    format!("mt-{tag}-{}", std::process::id())
}

async fn create(
    pool: &Pool,
    dir: &Path,
    slug: &str,
    opts: CreateTenantOpts,
) -> Result<Org, TenancyError> {
    match pool.clone() {
        #[cfg(feature = "postgres")]
        Pool::Postgres(p) => create_tenant(&TenantPools::new(p), "", dir, slug, opts).await,
        #[cfg(feature = "mysql")]
        Pool::Mysql(p) => create_tenant(&TenantPools::new(p), "", dir, slug, opts).await,
        Pool::Sqlite(p) => create_tenant(&TenantPools::new(p), "", dir, slug, opts).await,
    }
}

/// A database-mode tenant on the registry's own backend: PG and MySQL reuse
/// the test database, SQLite gets a file.
fn tenant_opts(pool: &Pool, tmp: &Path) -> CreateTenantOpts {
    let (backend, url) = match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(_) => (
            BackendKind::Postgres,
            std::env::var("DATABASE_URL").unwrap(),
        ),
        #[cfg(feature = "mysql")]
        Pool::Mysql(_) => (BackendKind::MySql, std::env::var("MYSQL_TEST_URL").unwrap()),
        Pool::Sqlite(_) => (
            BackendKind::Sqlite,
            format!("sqlite://{}?mode=rwc", tmp.join("t.db").display()),
        ),
    };
    CreateTenantOpts {
        mode: StorageMode::Database,
        backend,
        database_url: Some(url),
        ..CreateTenantOpts::default()
    }
}

/// One tenant migration whose SQL fails on every backend, writing nothing.
fn broken_migration(dir: &Path) {
    let mig = rustango::migrate::Migration {
        name: "0001_fail".to_owned(),
        created_at: "2026-10-09T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: rustango::migrate::MigrationScope::Tenant,
        replaces: Vec::new(),
        snapshot: serde_json::from_value(serde_json::json!({ "tables": [] })).unwrap(),
        forward: vec![rustango::migrate::Operation::Data(
            rustango::migrate::DataOp {
                sql: "SELECT * FROM rustango_no_such_table_2392".into(),
                reverse_sql: None,
                reversible: false,
            },
        )],
    };
    rustango::migrate::file::write(&dir.join("0001_fail.json"), &mig).unwrap();
}

async fn org_by_slug(pool: &Pool, slug: &str) -> Option<Org> {
    let rows: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(slug.to_owned()))
        .fetch(pool)
        .await
        .expect("fetch org");
    rows.into_iter().next()
}

/// #2392 — a failed migration is an error, and the tenant stays inactive.
async fn create_tenant_returns_a_failed_migration(pool: &Pool) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    broken_migration(&dir);
    let slug = name("broken");
    let r = create(pool, &dir, &slug, tenant_opts(pool, tmp.path())).await;
    let err = r.expect_err("a failed migration must fail create_tenant");
    assert!(
        err.to_string().contains("rustango_no_such_table_2392"),
        "{err}"
    );
    let org = org_by_slug(pool, &slug)
        .await
        .expect("row kept for the operator");
    assert!(!org.active, "a half-migrated tenant must not resolve");
}

/// #2392 — a clean run activates the tenant.
async fn create_tenant_activates_after_migrating(pool: &Pool) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    let slug = name("clean");
    let org = create(pool, &dir, &slug, tenant_opts(pool, tmp.path()))
        .await
        .expect("create");
    assert!(org.active);
}

/// A fresh registry plus one tenant with its own ledger: a PG schema, a
/// MySQL database, a SQLite file. The guards drop the scratch databases.
#[allow(clippy::type_complexity)]
async fn own_registry(pool: &Pool, tmp: &Path) -> (String, Org, Vec<Box<dyn std::any::Any>>) {
    let mut guards: Vec<Box<dyn std::any::Any>> = Vec::new();
    let tenant = Org {
        slug: name("fp"),
        display_name: "fp".into(),
        ..rustango::testkit::org()
    };
    let (url, tenant) = match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(_) => {
            let reg = scratch_db::ScratchDb::create(
                &std::env::var("DATABASE_URL").unwrap(),
                "rustango_mt",
            )
            .await;
            let url = reg.url().to_owned();
            guards.push(Box::new(reg));
            let tenant = Org {
                storage_mode: "schema".into(),
                schema_name: Some("mt_fp".into()),
                ..tenant
            };
            (url, tenant)
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(_) => {
            let admin = std::env::var("MYSQL_TEST_URL").unwrap();
            let reg = scratch_db::ScratchDb::create(&admin, "rustango_mt").await;
            let db = scratch_db::ScratchDb::create(&admin, "rustango_mt_t").await;
            let url = reg.url().to_owned();
            let tenant = Org {
                backend_kind: "mysql".into(),
                database_url: Some(db.url().to_owned()),
                ..tenant
            };
            guards.push(Box::new(reg));
            guards.push(Box::new(db));
            (url, tenant)
        }
        Pool::Sqlite(_) => {
            let url = format!("sqlite://{}?mode=rwc", tmp.join("reg.db").display());
            let tenant = Org {
                backend_kind: "sqlite".into(),
                database_url: Some(format!("sqlite://{}?mode=rwc", tmp.join("t.db").display())),
                ..tenant
            };
            (url, tenant)
        }
    };
    (url, tenant, guards)
}

/// Apply `dir` to `org` only, then `forget-pending <name>` through the tenancy CLI.
async fn forget_after_tenant_applied<DB: rustango::sql::sqlx::Database>(
    pools: &TenantPools<DB>,
    url: &str,
    dir: &Path,
    org: &Org,
    migration: &str,
) -> Result<(), TenancyError>
where
    Pool: From<rustango::sql::sqlx::Pool<DB>>,
{
    rustango::tenancy::migrate::migrate_one_tenant(pools, org, dir, url, None)
        .await
        .expect("tenant migrates");
    let mut out = Vec::new();
    let args = ["forget-pending".to_owned(), migration.to_owned()];
    rustango::tenancy::manage::run_with_writer(pools, url, dir, args, &mut out).await
}

/// #2393 — a migration only a tenant ledger records is not forgotten.
async fn forget_pending_refuses_a_tenant_applied_migration(pool: &Pool) {
    let tmp = tempfile::tempdir().unwrap();
    let (url, mut org, _guards) = own_registry(pool, tmp.path()).await;
    let registry = Pool::connect(&url).await.expect("registry");
    rustango::testkit::migrate_framework(&registry)
        .await
        .expect("framework");
    org.save_pool(&registry).await.expect("insert org");

    let dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    let mig = rustango::migrate::Migration {
        name: "0001_tenant".to_owned(),
        created_at: "2026-10-09T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: rustango::migrate::MigrationScope::Tenant,
        replaces: Vec::new(),
        snapshot: serde_json::from_value(serde_json::json!({ "tables": [] })).unwrap(),
        forward: vec![rustango::migrate::Operation::Data(
            rustango::migrate::DataOp {
                sql: "SELECT 1".into(),
                reverse_sql: Some("SELECT 1".into()),
                reversible: true,
            },
        )],
    };
    let file = dir.join("0001_tenant.json");
    rustango::migrate::file::write(&file, &mig).unwrap();

    let r = match registry.clone() {
        #[cfg(feature = "postgres")]
        Pool::Postgres(p) => {
            forget_after_tenant_applied(&TenantPools::new(p), &url, &dir, &org, "0001_tenant").await
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(p) => {
            forget_after_tenant_applied(&TenantPools::new(p), &url, &dir, &org, "0001_tenant").await
        }
        Pool::Sqlite(p) => {
            forget_after_tenant_applied(&TenantPools::new(p), &url, &dir, &org, "0001_tenant").await
        }
    };
    registry.close().await;
    let err = r.expect_err("a tenant-applied migration must not be forgotten");
    assert!(
        err.to_string()
            .contains(&format!("already applied on tenant `{}`", org.slug)),
        "{err}"
    );
    assert!(file.exists(), "the JSON was deleted");
}

#[cfg(feature = "postgres")]
async fn schema_exists(pool: &rustango::sql::sqlx::PgPool, schema: &str) -> bool {
    rustango::sql::sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = $1)",
    )
    .bind(schema)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn schema_opts(schema: &str) -> CreateTenantOpts {
    CreateTenantOpts {
        mode: StorageMode::Schema,
        schema_name: Some(schema.to_owned()),
        no_migrate: true,
        ..CreateTenantOpts::default()
    }
}

/// #2394 — a schema-mode tenant does not adopt a schema that exists.
async fn an_existing_schema_is_not_adopted(pool: &Pool) {
    #[cfg(feature = "postgres")]
    if let Pool::Postgres(pg) = pool {
        let schema = format!("mt_app_{}", std::process::id());
        for sql in [
            format!("DROP SCHEMA IF EXISTS {schema} CASCADE"),
            format!("CREATE SCHEMA {schema}"),
            format!("CREATE TABLE {schema}.keep (id INT)"),
        ] {
            rustango::sql::sqlx::query(&sql).execute(pg).await.unwrap();
        }
        let dir = Path::new("no-migrations");

        let slug = name("adopt");
        let err = create(pool, dir, &slug, schema_opts(&schema))
            .await
            .expect_err("create_tenant adopted the schema");
        assert!(err.to_string().contains("already exists"), "{err}");
        assert!(org_by_slug(pool, &slug).await.is_none());

        let request = rustango::tenancy::provision::ProvisionRequest {
            slug: name("adopt2"),
            mode: StorageMode::Schema,
            backend: BackendKind::Postgres,
            display_name: None,
            database_url: None,
            schema_name: Some(schema.clone()),
            host_pattern: None,
            port: None,
            path_prefix: None,
            run_migrations: false,
            preflight: rustango::tenancy::preflight::Preflight::default(),
        };
        let url = std::env::var("DATABASE_URL").unwrap();
        let r = rustango::tenancy::provision::provision_tenant(
            &TenantPools::new(pg.clone()),
            &url,
            dir,
            &request,
            None,
        )
        .await;
        assert!(
            matches!(&r, Err(e) if e.to_string().contains("already exists")),
            "provision adopted the schema: {r:?}"
        );

        let kept: i64 =
            rustango::sql::sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {schema}.keep"))
                .fetch_one(pg)
                .await
                .expect("the app's table is untouched");
        assert_eq!(kept, 0);
        rustango::sql::sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(pg)
            .await
            .unwrap();
    }
    let _ = pool;
}

/// #2394 — when the Org row fails, the schema just made is dropped, so a
/// retry is not refused as taken.
async fn a_failed_insert_releases_its_schema(pool: &Pool) {
    #[cfg(feature = "postgres")]
    if let Pool::Postgres(pg) = pool {
        use rustango::sql::Auto;
        use rustango::tenancy::OrgHost;

        let mut owner = Org {
            slug: name("race-a"),
            display_name: "a".into(),
            database_url: Some(std::env::var("DATABASE_URL").unwrap()),
            ..rustango::testkit::org()
        };
        owner.save_pool(pool).await.expect("owner");
        let host = format!("{}.example.test", name("race"));
        let schema = format!("mt_race_{}", std::process::id());

        // Hold an uncommitted claim on `host`, so the INSERT loses the race.
        let mut tx = rustango::sql::transaction_pool(pool).await.expect("begin");
        let mut row = OrgHost {
            id: Auto::Unset,
            org_id: *owner.id.get().unwrap(),
            hostname: host.clone(),
            enabled: true,
            created_at: Auto::Unset,
        };
        row.insert_tx(&mut tx).await.expect("held host");
        let opts = CreateTenantOpts {
            host_pattern: Some(host.clone()),
            ..schema_opts(&schema)
        };
        let (p, slug) = (pool.clone(), name("race-b"));
        let create =
            tokio::spawn(async move { create(&p, Path::new("no-migrations"), &slug, opts).await });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        tx.commit().await.expect("commit");
        let r = create.await.expect("join");
        assert!(
            matches!(&r, Err(e) if e.to_string().contains("already used")),
            "{r:?}"
        );
        assert!(
            !schema_exists(pg, &schema).await,
            "the new schema was left behind"
        );
    }
    let _ = pool;
}

tri_dialect_test! {
    setup: setup,
    sqlite: file,
    scenarios: [
        create_tenant_returns_a_failed_migration,
        create_tenant_activates_after_migrating,
        forget_pending_refuses_a_tenant_applied_migration,
        an_existing_schema_is_not_adopted,
        a_failed_insert_releases_its_schema,
    ],
}
