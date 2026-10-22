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

/// #2569 — purge deletes the tenant's media objects, lists what it had
/// to leave, and never touches a database it may share.
#[cfg(all(feature = "sqlite", feature = "media"))]
#[tokio::test]
async fn purge_deletes_the_tenants_media_objects() {
    use rustango::storage::Storage as _;
    use rustango::tenancy::TenantPoolsConfig;

    let dir = tempfile::tempdir().unwrap();
    let path = |n: &str| dir.path().join(n);
    let url = |n: &str| format!("sqlite://{}?mode=rwc", path(n).display());
    let registry = rustango::sql::sqlx::SqlitePool::connect(&url("reg.db"))
        .await
        .unwrap();
    let reg = Pool::Sqlite(registry.clone());
    setup(&reg).await;
    let (disk, storage) = media_disk();
    let org_on = |name: &str, url: String| {
        let (reg, slug) = (
            reg.clone(),
            format!("purge-media-{name}-{}", std::process::id()),
        );
        async move {
            let mut org = Org {
                slug: slug.clone(),
                display_name: slug.clone(),
                backend_kind: "sqlite".into(),
                database_url: Some(url),
                ..rustango::testkit::org()
            };
            org.save_pool(&reg).await.unwrap();
            slug
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
    let with_storage = || TenantPoolsConfig {
        media_storage: Some(storage.clone()),
        ..TenantPoolsConfig::default()
    };
    let all_exist = |keys: Vec<String>| {
        let disk = disk.clone();
        async move {
            for key in keys {
                assert!(disk.exists(&key).await.unwrap(), "{key} was deleted");
            }
        }
    };

    // No media storage configured: the keys are listed, the objects stay.
    let kept = upload(&Pool::connect(&url("bare.db")).await.unwrap(), &storage).await;
    let report = purge(
        org_on("bare", url("bare.db")).await,
        TenantPoolsConfig::default(),
    )
    .await;
    assert_eq!(report.media_deleted, 0);
    let mut left: Vec<String> = report.media_left.iter().map(|(_, k)| k.clone()).collect();
    left.sort();
    let mut want = kept.clone();
    want.sort();
    assert_eq!(left, want);
    all_exist(kept.clone()).await;

    // Configured: this tenant's objects go, another tenant's on the same disk stay.
    let keys = upload(&Pool::connect(&url("full.db")).await.unwrap(), &storage).await;
    let report = purge(org_on("full", url("full.db")).await, with_storage()).await;
    assert_eq!((report.media_deleted, report.media_left.len()), (2, 0));
    for key in &keys {
        assert!(!disk.exists(key).await.unwrap(), "{key} survived the purge");
    }
    all_exist(kept).await;

    // The registry's own database, and one two tenants share, are left alone.
    let reg_keys = upload(&reg, &storage).await;
    let report = purge(org_on("onreg", url("reg.db")).await, with_storage()).await;
    assert_eq!(report.media_deleted, 0, "{:?}", report.notes);
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("it is the registry's")),
        "{:?}",
        report.notes
    );
    all_exist(reg_keys).await;
    let twin_keys = upload(&Pool::connect(&url("twin.db")).await.unwrap(), &storage).await;
    org_on("twin-a", url("twin.db")).await;
    let report = purge(org_on("twin-b", url("twin.db")).await, with_storage()).await;
    assert_eq!(report.media_deleted, 0, "{:?}", report.notes);
    assert!(
        report.notes.iter().any(|n| n.contains("uses it too")),
        "{:?}",
        report.notes
    );
    all_exist(twin_keys).await;

    // A missing tenant file is not created by the purge.
    purge(org_on("gone", url("gone.db")).await, with_storage()).await;
    assert!(
        !path("gone.db").exists(),
        "the purge created the tenant file"
    );
}

/// #2569 — MySQL identity is the live server, so a host alias of the
/// registry's or another tenant's database is still refused.
#[cfg(all(feature = "mysql", feature = "media"))]
#[tokio::test]
#[allow(irrefutable_let_patterns)]
async fn mysql_purge_sees_through_a_host_alias() {
    use rustango::storage::Storage as _;
    use rustango::tenancy::TenantPoolsConfig;
    let _guard = rustango::testkit::matrix::live_lock().lock().await;
    let Some(pool) = rustango::testkit::matrix::Backend::MySql.pool().await else {
        eprintln!("MYSQL_TEST_URL not set — skipping");
        return;
    };
    setup(&pool).await;
    let Pool::Mysql(my) = &pool else {
        unreachable!()
    };
    let base = std::env::var("MYSQL_TEST_URL").unwrap();
    let alias = |url: &str| {
        if url.contains("@127.0.0.1") {
            url.replace("@127.0.0.1", "@localhost")
        } else {
            url.replace("@localhost", "@127.0.0.1")
        }
    };
    assert_ne!(
        alias(&base),
        base,
        "MYSQL_TEST_URL host is no localhost / 127.0.0.1"
    );
    let pid = std::process::id();
    let db_url = |db: &str| format!("{}/{db}", base.rsplit_once('/').unwrap().0);
    let (shared, own) = (
        format!("rustango_purge_tw_{pid}"),
        format!("rustango_purge_own_{pid}"),
    );
    for db in [&shared, &own] {
        rustango::sql::sqlx::query(&format!("CREATE DATABASE IF NOT EXISTS `{db}`"))
            .execute(my)
            .await
            .unwrap();
    }
    let (disk, storage) = media_disk();
    let pools = TenantPools::new(my.clone()).config(TenantPoolsConfig {
        media_storage: Some(storage.clone()),
        ..TenantPoolsConfig::default()
    });
    let org_on = |tag: &str, url: String| {
        let (pool, slug) = (pool.clone(), format!("purge-my-{tag}-{pid}"));
        async move {
            let mut org = Org {
                slug: slug.clone(),
                display_name: slug.clone(),
                backend_kind: "mysql".into(),
                database_url: Some(url),
                ..rustango::testkit::org()
            };
            org.save_pool(&pool).await.unwrap();
            slug
        }
    };
    let purge = |slug: String| {
        let pools = &pools;
        async move {
            let action = Action::Purge {
                purge_database: true,
            };
            decommission(pools, &slug, action).await.unwrap()
        }
    };
    let notes_say = |report: &Report, what: &str| {
        assert_eq!(report.media_deleted, 0, "{:?}", report.notes);
        assert!(
            report.notes.iter().any(|n| n.contains(what)),
            "{:?}",
            report.notes
        );
    };

    let reg_keys = upload(&pool, &storage).await;
    notes_say(
        &purge(org_on("onreg", alias(&base)).await).await,
        "it is the registry's",
    );
    let tenant = Pool::connect(&db_url(&shared)).await.unwrap();
    let twin_keys = upload(&tenant, &storage).await;
    tenant.close().await;
    org_on("twin-a", db_url(&shared)).await;
    notes_say(
        &purge(org_on("twin-b", alias(&db_url(&shared))).await).await,
        "uses it too",
    );
    for key in reg_keys.iter().chain(&twin_keys) {
        assert!(disk.exists(key).await.unwrap(), "{key} was deleted");
    }
    let tenant = Pool::connect(&db_url(&own)).await.unwrap();
    let keys = upload(&tenant, &storage).await;
    tenant.close().await;
    let report = purge(org_on("own", db_url(&own)).await).await;
    assert_eq!(report.media_deleted, 2, "{:?}", report.notes);
    for key in &keys {
        assert!(!disk.exists(key).await.unwrap(), "{key} survived the purge");
    }

    let twin_a = format!("purge-my-twin-a-{pid}");
    decommission(
        &pools,
        &twin_a,
        Action::Purge {
            purge_database: true,
        },
    )
    .await
    .unwrap();
    let mine = rustango::media::Media::objects()
        .where_(rustango::media::Media::storage_key.is_in(reg_keys))
        .compile_delete()
        .unwrap();
    rustango::sql::delete_pool(&pool, &mine).await.unwrap();
    for db in [&shared, &own] {
        rustango::sql::sqlx::query(&format!("DROP DATABASE IF EXISTS `{db}`"))
            .execute(my)
            .await
            .unwrap();
    }
}

/// An in-memory media disk and a registry holding it as `default`.
#[cfg(feature = "media")]
fn media_disk() -> (
    std::sync::Arc<rustango::storage::InMemoryStorage>,
    rustango::storage::StorageRegistry,
) {
    let disk = std::sync::Arc::new(rustango::storage::InMemoryStorage::new());
    let storage = rustango::storage::StorageRegistry::new()
        .set("default", disk.clone() as rustango::storage::BoxedStorage)
        .with_default("default");
    (disk, storage)
}

/// Two uploads into `pool`, creating the media table if needed; their keys.
#[cfg(feature = "media")]
async fn upload(pool: &Pool, storage: &rustango::storage::StorageRegistry) -> Vec<String> {
    use rustango::media::{Media, MediaManager, SaveOpts};
    rustango::testkit::create_tables_for::<Media>(pool)
        .await
        .unwrap();
    let mgr = MediaManager::new_pool(pool.clone(), storage.clone());
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
    keys
}

/// #2569 — schema mode: media in the tenant's schema is deleted; with the
/// schema gone (a retried purge), `public.rustango_media` is never read.
#[cfg(all(feature = "postgres", feature = "media"))]
#[tokio::test]
#[allow(irrefutable_let_patterns)]
async fn schema_purge_deletes_only_the_tenant_schemas_media() {
    use rustango::storage::Storage as _;
    use rustango::tenancy::TenantPoolsConfig;
    let _guard = rustango::testkit::matrix::live_lock().lock().await;
    let Some(pool) = rustango::testkit::matrix::Backend::Postgres.pool().await else {
        eprintln!("DATABASE_URL not set — skipping");
        return;
    };
    setup(&pool).await;
    let Pool::Postgres(pg) = &pool else {
        unreachable!()
    };
    let (disk, storage) = media_disk();
    let pools = TenantPools::new(pg.clone()).config(TenantPoolsConfig {
        media_storage: Some(storage.clone()),
        ..TenantPoolsConfig::default()
    });
    let public_keys = upload(&pool, &storage).await;
    let schema_org = |tag: &str| {
        let slug = format!("purge_media_{tag}_{}", std::process::id());
        Org {
            slug: slug.replace('_', "-"),
            display_name: slug.clone(),
            storage_mode: "schema".into(),
            schema_name: Some(slug),
            ..rustango::testkit::org()
        }
    };
    let action = Action::Purge {
        purge_database: false,
    };

    // The schema exists and holds the tenant's media.
    let mut org = schema_org("live");
    let schema = org.schema_name.clone().unwrap();
    rustango::sql::sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\""))
        .execute(pg)
        .await
        .unwrap();
    org.save_pool(&pool).await.unwrap();
    let scoped = Pool::Postgres(pools.scoped_pool(&org).await.unwrap());
    let keys = upload(&scoped, &storage).await;
    drop(scoped);
    let report = decommission(&pools, &org.slug, action)
        .await
        .expect("purge");
    assert_eq!(report.media_deleted, 2, "{:?}", report.notes);
    for key in &keys {
        assert!(!disk.exists(key).await.unwrap(), "{key} survived the purge");
    }

    // A retried purge: the schema is already gone.
    let mut org = schema_org("gone");
    org.save_pool(&pool).await.unwrap();
    let report = decommission(&pools, &org.slug, action)
        .await
        .expect("purge");
    assert_eq!(report.media_deleted, 0, "{:?}", report.notes);
    for key in &public_keys {
        assert!(
            disk.exists(key).await.unwrap(),
            "public's {key} was deleted"
        );
    }
    let mine = rustango::media::Media::objects()
        .where_(rustango::media::Media::storage_key.is_in(public_keys))
        .compile_delete()
        .unwrap();
    rustango::sql::delete_pool(&pool, &mine).await.unwrap();
}

/// #2569 — database mode on Postgres deletes the tenant database's media.
#[cfg(all(feature = "postgres", feature = "media"))]
#[tokio::test]
#[allow(irrefutable_let_patterns)]
async fn database_purge_deletes_the_tenants_media() {
    use rustango::storage::Storage as _;
    use rustango::tenancy::TenantPoolsConfig;
    let _guard = rustango::testkit::matrix::live_lock().lock().await;
    let Some(pool) = rustango::testkit::matrix::Backend::Postgres.pool().await else {
        eprintln!("DATABASE_URL not set — skipping");
        return;
    };
    setup(&pool).await;
    let Pool::Postgres(pg) = &pool else {
        unreachable!()
    };
    let (disk, storage) = media_disk();
    let (org, db) = org_with_database(&pool, "media").await;
    let tenant = Pool::connect(org.database_url.as_deref().unwrap())
        .await
        .unwrap();
    let keys = upload(&tenant, &storage).await;
    tenant.close().await;
    let pools = TenantPools::new(pg.clone()).config(TenantPoolsConfig {
        media_storage: Some(storage),
        ..TenantPoolsConfig::default()
    });
    let action = Action::Purge {
        purge_database: true,
    };
    let report = decommission(&pools, &org.slug, action)
        .await
        .expect("purge");
    assert_eq!(report.media_deleted, 2, "{:?}", report.notes);
    for key in &keys {
        assert!(!disk.exists(key).await.unwrap(), "{key} survived the purge");
    }
    assert!(!database_exists(&pool, &db).await);
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
    let pg = pool.as_postgres().expect("a Postgres pool");
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
    let pg = pool.as_postgres().expect("a Postgres pool");
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
