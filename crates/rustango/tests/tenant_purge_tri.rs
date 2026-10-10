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

/// A purge leaves no run linked to the org, so none can resume it (#2292).
async fn purge_unlinks_its_unsucceeded_runs(pool: &Pool) {
    use rustango::tenancy::provision_store as store;
    let (slug, id) = tenant_with_host(pool, "runs").await;
    let run = store::open_run(pool, &slug, "database", "sqlite", None, None, None)
        .await
        .unwrap();
    let run_id = run.id.get().copied().unwrap();
    store::attach_org(pool, run_id, id).await.unwrap();
    purge_on(pool, &slug).await.expect("purge");
    let run = store::run_by_id(pool, run_id).await.unwrap().unwrap();
    assert_eq!(
        run.org_id, None,
        "a running run still points at the purged org"
    );
}

tri_dialect_test!(
    setup: setup,
    scenarios: [purge_deletes_org_with_extra_host, purge_unlinks_its_unsucceeded_runs]
);

/// #2569 — purge deletes the tenant's media objects, or counts what it
/// had to leave when no media storage is configured.
#[cfg(all(feature = "sqlite", feature = "media"))]
#[tokio::test]
async fn purge_deletes_the_tenants_media_objects() {
    use rustango::media::{MediaManager, SaveOpts};
    use rustango::storage::{BoxedStorage, InMemoryStorage, Storage as _, StorageRegistry};
    use rustango::tenancy::TenantPoolsConfig;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let url = |n: &str| format!("sqlite://{}?mode=rwc", dir.path().join(n).display());
    let registry = rustango::sql::sqlx::SqlitePool::connect(&url("reg.db"))
        .await
        .unwrap();
    let reg = Pool::Sqlite(registry.clone());
    setup(&reg).await;
    let disk = Arc::new(InMemoryStorage::new());
    let storage = StorageRegistry::new()
        .set("default", disk.clone() as BoxedStorage)
        .with_default("default");

    // A tenant database holding two uploads; its slug.
    let tenant = |name: &'static str| {
        let (url, reg, storage) = (url(name), reg.clone(), storage.clone());
        async move {
            let pool = Pool::connect(&url).await.unwrap();
            rustango::testkit::migrate_framework(&pool).await.unwrap();
            let mgr = MediaManager::new_pool(pool, storage);
            let mut keys = Vec::new();
            for file in ["a.txt", "b.txt"] {
                let m = mgr
                    .save_bytes(SaveOpts {
                        disk: "default".into(),
                        key_prefix: "docs".into(),
                        bytes: b"secret".to_vec(),
                        mime: "text/plain".into(),
                        original_filename: file.into(),
                        uploaded_by_id: None,
                        collection_id: None,
                        metadata: serde_json::json!({}),
                    })
                    .await
                    .unwrap();
                keys.push(m.storage_key);
            }
            let slug = format!("purge-media-{name}-{}", std::process::id());
            let mut org = Org {
                slug: slug.clone(),
                display_name: slug.clone(),
                backend_kind: "sqlite".into(),
                database_url: Some(url),
                ..rustango::testkit::org()
            };
            org.save_pool(&reg).await.unwrap();
            (slug, keys)
        }
    };
    let purge = |slug: String, config: TenantPoolsConfig| {
        let pools = TenantPools::new(registry.clone()).config(config);
        async move {
            let action = Action::Purge {
                purge_database: true,
            };
            decommission(&pools, &slug, action).await.unwrap()
        }
    };

    let (slug, kept) = tenant("bare.db").await;
    let report = purge(slug, TenantPoolsConfig::default()).await;
    assert_eq!((report.media_deleted, report.media_left), (0, 2));
    assert!(
        report.notes.iter().any(|n| n.contains("2 media objects")),
        "{:?}",
        report.notes
    );
    for key in &kept {
        assert!(disk.exists(key).await.unwrap());
    }

    let (slug, keys) = tenant("full.db").await;
    let config = TenantPoolsConfig {
        media_storage: Some(storage.clone()),
        ..TenantPoolsConfig::default()
    };
    let report = purge(slug, config).await;
    assert_eq!((report.media_deleted, report.media_left), (2, 0));
    for key in &keys {
        assert!(!disk.exists(key).await.unwrap(), "{key} survived the purge");
    }
}

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

/// A fresh PG database and a database-mode org pointing at it.
#[cfg(feature = "postgres")]
async fn org_with_database(pool: &Pool, tag: &str) -> (Org, String) {
    let Pool::Postgres(pg) = pool else {
        unreachable!()
    };
    let slug = format!("purge-{tag}-{}", std::process::id());
    let db = format!("rustango_purge_{tag}_{}", std::process::id());
    let _ = rustango::sql::sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"))
        .execute(pg)
        .await;
    rustango::sql::sqlx::query(&format!("CREATE DATABASE \"{db}\""))
        .execute(pg)
        .await
        .unwrap();
    let url = std::env::var("DATABASE_URL").unwrap();
    let mut org = Org {
        slug,
        display_name: tag.into(),
        backend_kind: "postgres".into(),
        database_url: Some(format!("{}/{db}", url.rsplit_once('/').unwrap().0)),
        ..rustango::testkit::org()
    };
    org.save_pool(pool).await.expect("insert org");
    (org, db)
}

#[cfg(feature = "postgres")]
async fn database_exists(pool: &Pool, db: &str) -> bool {
    let Pool::Postgres(pg) = pool else {
        unreachable!()
    };
    rustango::sql::sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM pg_database WHERE datname = $1",
    )
    .bind(db)
    .fetch_one(pg)
    .await
    .unwrap()
        == 1
}

/// Another pod's own tenant pool does not block a database-mode purge (#2291).
#[cfg(feature = "postgres")]
#[tokio::test]
#[allow(irrefutable_let_patterns)]
async fn database_purge_ends_the_tenants_own_sessions_on_other_pods() {
    let _guard = rustango::testkit::matrix::live_lock().lock().await;
    let Some(pool) = rustango::testkit::matrix::Backend::Postgres.pool().await else {
        eprintln!("DATABASE_URL not set — skipping");
        return;
    };
    setup(&pool).await;
    let Pool::Postgres(pg) = &pool else {
        unreachable!()
    };
    let (org, db) = org_with_database(&pool, "ownpod").await;
    let other_pod = TenantPools::new(pg.clone());
    let _held = other_pod
        .database_acquire(&org)
        .await
        .expect("other pod's connection");

    let report = purge_on(&pool, &org.slug).await.expect("purge");
    assert!(report.database_dropped.is_some());
    assert!(
        !database_exists(&pool, &db).await,
        "the tenant database survived the purge"
    );
}

/// A session the tenant's pools did not open blocks the drop, as before (#2291).
#[cfg(feature = "postgres")]
#[tokio::test]
#[allow(irrefutable_let_patterns)]
async fn database_purge_refuses_while_a_foreign_session_is_open() {
    use rustango::sql::sqlx::{Connection as _, PgConnection};
    let _guard = rustango::testkit::matrix::live_lock().lock().await;
    let Some(pool) = rustango::testkit::matrix::Backend::Postgres.pool().await else {
        eprintln!("DATABASE_URL not set — skipping");
        return;
    };
    setup(&pool).await;
    let (org, db) = org_with_database(&pool, "foreign").await;
    let url = org.database_url.clone().unwrap();
    let mut foreign = PgConnection::connect(&url).await.unwrap();

    let err = purge_on(&pool, &org.slug)
        .await
        .expect_err("a foreign session must block the drop");
    assert!(err.to_string().contains("did not open"), "{err}");
    assert!(
        database_exists(&pool, &db).await,
        "the database was dropped"
    );
    foreign
        .ping()
        .await
        .expect("the foreign session was killed");

    foreign.close().await.unwrap();
    purge_on(&pool, &org.slug)
        .await
        .expect("purge once it closed");
}

/// A row pointing at the registry's own database is refused before anything changes (#2291).
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
    let rows: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(slug.clone()))
        .fetch(&pool)
        .await
        .unwrap();
    assert!(
        rows[0].active,
        "the refused purge still deactivated the org"
    );
    org.delete_pool(&pool).await.unwrap();
}

/// A schema another tenant shares, or a reserved one, is never dropped (#2290).
#[cfg(feature = "postgres")]
#[tokio::test]
#[allow(irrefutable_let_patterns)]
async fn schema_purge_refuses_a_shared_or_reserved_schema() {
    let _guard = rustango::testkit::matrix::live_lock().lock().await;
    let Some(pool) = rustango::testkit::matrix::Backend::Postgres.pool().await else {
        eprintln!("DATABASE_URL not set — skipping");
        return;
    };
    setup(&pool).await;
    let Pool::Postgres(pg) = &pool else {
        unreachable!()
    };
    let shared = format!("purge_shared_{}", std::process::id());
    rustango::sql::sqlx::raw_sql(&format!(
        "CREATE SCHEMA IF NOT EXISTS \"{shared}\"; CREATE TABLE IF NOT EXISTS \"{shared}\".marker (id INT)"
    ))
    .execute(pg)
    .await
    .unwrap();
    // A legacy pair: one by slug default, one by explicit name.
    let mut owner = Org {
        slug: shared.replace('_', "-"),
        display_name: "owner".into(),
        storage_mode: "schema".into(),
        schema_name: Some(shared.clone()),
        ..rustango::testkit::org()
    };
    owner.save_pool(&pool).await.unwrap();
    let mut legacy = Org {
        slug: format!("{}-legacy", owner.slug),
        display_name: "legacy".into(),
        storage_mode: "schema".into(),
        schema_name: Some(shared.clone()),
        ..rustango::testkit::org()
    };
    legacy.save_pool(&pool).await.unwrap();

    let err = purge_on(&pool, &legacy.slug)
        .await
        .expect_err("shared schema");
    assert!(err.to_string().contains("another tenant"), "{err}");
    let left: i64 =
        rustango::sql::sqlx::query_scalar(&format!("SELECT count(*) FROM \"{shared}\".marker"))
            .fetch_one(pg)
            .await
            .expect("the shared schema survived");
    assert_eq!(left, 0);
    let rows: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(legacy.slug.clone()))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "the refused purge deleted the org");
    assert!(rows[0].active, "the refused purge deactivated the org");

    for org in [owner, legacy] {
        org.delete_pool(&pool).await.unwrap();
    }
    rustango::sql::sqlx::query(&format!("DROP SCHEMA \"{shared}\" CASCADE"))
        .execute(pg)
        .await
        .unwrap();
}

/// A row naming `public` is refused (#2290). In a private database, so a
/// missing guard would drop that database's `public`, not the suite's.
#[cfg(feature = "postgres")]
#[tokio::test]
#[allow(irrefutable_let_patterns)]
async fn schema_purge_refuses_a_reserved_schema() {
    let _guard = rustango::testkit::matrix::live_lock().lock().await;
    let Some(pool) = rustango::testkit::matrix::Backend::Postgres.pool().await else {
        eprintln!("DATABASE_URL not set — skipping");
        return;
    };
    let Pool::Postgres(pg) = &pool else {
        unreachable!()
    };
    let db = format!("rustango_purge_reserved_{}", std::process::id());
    let _ = rustango::sql::sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"))
        .execute(pg)
        .await;
    rustango::sql::sqlx::query(&format!("CREATE DATABASE \"{db}\""))
        .execute(pg)
        .await
        .unwrap();
    let url = std::env::var("DATABASE_URL").unwrap();
    let private =
        rustango::sql::sqlx::PgPool::connect(&format!("{}/{db}", url.rsplit_once('/').unwrap().0))
            .await
            .unwrap();
    let registry = Pool::from(private.clone());
    setup(&registry).await;
    let mut reserved = Org {
        slug: "reserved".into(),
        display_name: "public".into(),
        storage_mode: "schema".into(),
        schema_name: Some("public".into()),
        ..rustango::testkit::org()
    };
    reserved.save_pool(&registry).await.unwrap();

    let err = purge_on(&registry, "reserved").await.expect_err("public");
    assert!(err.to_string().contains("public"), "{err}");
    let rows: Vec<Org> = Org::objects()
        .fetch(&registry)
        .await
        .expect("public survived");
    assert!(rows[0].active, "the refused purge deactivated the org");

    private.close().await;
    rustango::sql::sqlx::query(&format!("DROP DATABASE \"{db}\" WITH (FORCE)"))
        .execute(pg)
        .await
        .unwrap();
}

/// Two tenants on one database, the other idle (no session): refused (#2291).
#[cfg(feature = "postgres")]
#[tokio::test]
#[allow(irrefutable_let_patterns)]
async fn database_purge_refuses_a_database_another_tenant_shares() {
    use rustango::sql::sqlx::{Connection as _, PgConnection};
    let _guard = rustango::testkit::matrix::live_lock().lock().await;
    let Some(pool) = rustango::testkit::matrix::Backend::Postgres.pool().await else {
        eprintln!("DATABASE_URL not set — skipping");
        return;
    };
    setup(&pool).await;
    let (a, db) = org_with_database(&pool, "sharea").await;
    let url = a.database_url.clone().unwrap();
    let mut b = Org {
        slug: format!("{}-b", a.slug),
        display_name: "b".into(),
        backend_kind: "postgres".into(),
        database_url: Some(url.clone()),
        ..rustango::testkit::org()
    };
    b.save_pool(&pool).await.unwrap();
    let mut seed = PgConnection::connect(&url).await.unwrap();
    rustango::sql::sqlx::raw_sql("CREATE TABLE b_data (id INT); INSERT INTO b_data VALUES (1)")
        .execute(&mut seed)
        .await
        .unwrap();
    seed.close().await.unwrap();

    let err = purge_on(&pool, &a.slug).await.expect_err("shared database");
    assert!(err.to_string().contains(&b.slug), "{err}");
    let mut check = PgConnection::connect(&url)
        .await
        .expect("the database survived");
    let n: i64 = rustango::sql::sqlx::query_scalar("SELECT count(*) FROM b_data")
        .fetch_one(&mut check)
        .await
        .unwrap();
    assert_eq!(n, 1, "B's data was touched");
    check.close().await.unwrap();
    assert!(
        org_active(&pool, &a.slug).await,
        "the refused purge deactivated A"
    );

    b.delete_pool(&pool).await.unwrap();
    purge_on(&pool, &a.slug)
        .await
        .expect("purge once A is alone");
    assert!(!database_exists(&pool, &db).await);
}

/// The registry spelled `localhost` instead of `127.0.0.1` slips past the text
/// guard; its own open sessions refuse it, before the org changes (#2291).
#[cfg(feature = "postgres")]
#[tokio::test]
#[allow(irrefutable_let_patterns)]
async fn database_purge_refuses_the_registry_under_another_host_name() {
    let _guard = rustango::testkit::matrix::live_lock().lock().await;
    let Some(pool) = rustango::testkit::matrix::Backend::Postgres.pool().await else {
        eprintln!("DATABASE_URL not set — skipping");
        return;
    };
    setup(&pool).await;
    let url = std::env::var("DATABASE_URL").unwrap();
    let alias = if url.contains("127.0.0.1") {
        url.replace("127.0.0.1", "localhost")
    } else {
        url.replace("localhost", "127.0.0.1")
    };
    assert_ne!(
        alias, url,
        "DATABASE_URL names neither localhost nor 127.0.0.1"
    );
    let slug = format!("purge-alias-{}", std::process::id());
    let mut org = Org {
        slug: slug.clone(),
        display_name: slug.clone(),
        backend_kind: "postgres".into(),
        database_url: Some(alias),
        ..rustango::testkit::org()
    };
    org.save_pool(&pool).await.unwrap();

    let err = purge_on(&pool, &slug).await.expect_err("registry by alias");
    assert!(err.to_string().contains("did not open"), "{err}");
    assert!(
        org_active(&pool, &slug).await,
        "the refused purge deactivated the org"
    );
    org.delete_pool(&pool).await.unwrap();
}

/// The org id is in the tag: another tenant's tagged session is foreign (#2291).
#[cfg(feature = "postgres")]
#[tokio::test]
#[allow(irrefutable_let_patterns)]
async fn database_purge_does_not_end_another_tenants_tagged_session() {
    let _guard = rustango::testkit::matrix::live_lock().lock().await;
    let Some(pool) = rustango::testkit::matrix::Backend::Postgres.pool().await else {
        eprintln!("DATABASE_URL not set — skipping");
        return;
    };
    setup(&pool).await;
    let Pool::Postgres(pg) = &pool else {
        unreachable!()
    };
    let (a, db) = org_with_database(&pool, "tagged").await;
    // Not in this registry, so only its session can speak for it.
    let b = Org {
        id: Auto::Set(i64::from(i32::MAX) + i64::from(std::process::id())),
        slug: format!("{}-b", a.slug),
        database_url: a.database_url.clone(),
        ..a.clone()
    };
    let other_pod = TenantPools::new(pg.clone());
    let mut held = other_pod.database_acquire(&b).await.unwrap();

    let err = purge_on(&pool, &a.slug)
        .await
        .expect_err("B's session is foreign");
    assert!(err.to_string().contains("did not open"), "{err}");
    let one: i32 = rustango::sql::sqlx::query_scalar("SELECT 1")
        .fetch_one(&mut **held)
        .await
        .expect("B's session was killed");
    assert_eq!(one, 1);

    drop(held);
    other_pod.invalidate(&b.slug).await;
    a.delete_pool(&pool).await.unwrap();
    rustango::sql::sqlx::query(&format!("DROP DATABASE \"{db}\" WITH (FORCE)"))
        .execute(pg)
        .await
        .unwrap();
}

#[cfg(feature = "postgres")]
async fn org_active(pool: &Pool, slug: &str) -> bool {
    let rows: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(slug.to_owned()))
        .fetch(pool)
        .await
        .unwrap();
    rows[0].active
}
