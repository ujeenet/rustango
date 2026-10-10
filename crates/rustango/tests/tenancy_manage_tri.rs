//! Tenancy manage verbs and `api::create_tenant`, on every registry backend.
//! Each scenario gets a scratch registry, so no rows outlive it.

#![cfg(all(feature = "tenancy", feature = "testkit", feature = "sqlite"))]

use std::path::{Path, PathBuf};
#[cfg(feature = "postgres")]
use std::time::Duration;

use rustango::core::Column as _;
use rustango::sql::{FetcherPool as _, Pool};
use rustango::tenancy::manage::api::{create_tenant, create_tenant_if_missing, CreateTenantOpts};
#[cfg(feature = "postgres")]
use rustango::tenancy::provision::{provision_tenant, ProvisionRequest};
use rustango::tenancy::{BackendKind, Org, StorageMode, TenancyError, TenantPools};
use rustango::tri_dialect_test;

#[cfg(any(feature = "postgres", feature = "mysql"))]
#[path = "support/scratch_db.rs"]
mod scratch_db;

/// Run `$body` with `$p` bound to a typed `TenantPools` over `$pool`.
macro_rules! typed {
    ($pool:expr, $p:ident => $body:expr) => {
        match $pool.clone() {
            #[cfg(feature = "postgres")]
            Pool::Postgres(x) => {
                let $p = TenantPools::new(x);
                $body
            }
            #[cfg(feature = "mysql")]
            Pool::Mysql(x) => {
                let $p = TenantPools::new(x);
                $body
            }
            Pool::Sqlite(x) => {
                let $p = TenantPools::new(x);
                $body
            }
        }
    };
}

/// The macro's pool only picks the backend; every scenario builds an [`Env`].
async fn setup(_: &Pool) {}

struct Env {
    url: String,
    registry: Pool,
    tmp: tempfile::TempDir,
    // Dropped last: each removes a scratch database.
    #[allow(dead_code)]
    guards: Vec<Box<dyn std::any::Any>>,
}

impl Env {
    async fn new(backend: &Pool) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        #[cfg_attr(not(any(feature = "postgres", feature = "mysql")), allow(unused_mut))]
        let mut guards: Vec<Box<dyn std::any::Any>> = Vec::new();
        let url = match backend {
            #[cfg(feature = "postgres")]
            Pool::Postgres(_) => scratch(&mut guards, "DATABASE_URL").await,
            #[cfg(feature = "mysql")]
            Pool::Mysql(_) => scratch(&mut guards, "MYSQL_TEST_URL").await,
            Pool::Sqlite(_) => format!("sqlite://{}?mode=rwc", tmp.path().join("reg.db").display()),
        };
        let registry = Pool::connect(&url).await.expect("registry");
        rustango::testkit::migrate_framework(&registry)
            .await
            .expect("framework tables");
        Self {
            url,
            registry,
            tmp,
            guards,
        }
    }

    fn backend(&self) -> BackendKind {
        match &self.registry {
            #[cfg(feature = "postgres")]
            Pool::Postgres(_) => BackendKind::Postgres,
            #[cfg(feature = "mysql")]
            Pool::Mysql(_) => BackendKind::MySql,
            Pool::Sqlite(_) => BackendKind::Sqlite,
        }
    }

    fn dir(&self, name: &str) -> PathBuf {
        let dir = self.tmp.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `create_tenant` options for a tenant with storage of its own; the
    /// registry's own database is refused (#2320).
    async fn tenant_opts(&mut self, tag: &str) -> CreateTenantOpts {
        let org = self.own_tenant(tag).await;
        CreateTenantOpts {
            mode: StorageMode::parse(&org.storage_mode).unwrap(),
            backend: self.backend(),
            database_url: org.database_url,
            schema_name: org.schema_name,
            ..CreateTenantOpts::default()
        }
    }

    /// An unsaved, active tenant with a ledger of its own: a PG schema, a
    /// MySQL scratch database, a SQLite file.
    async fn own_tenant(&mut self, tag: &str) -> Org {
        let base = Org {
            slug: format!("mt-{tag}"),
            display_name: tag.into(),
            ..rustango::testkit::org()
        };
        match self.backend() {
            BackendKind::Postgres => Org {
                storage_mode: "schema".into(),
                schema_name: Some(format!("mt_{tag}")),
                ..base
            },
            #[cfg(feature = "mysql")]
            BackendKind::MySql => {
                let url = scratch(&mut self.guards, "MYSQL_TEST_URL").await;
                Org {
                    backend_kind: "mysql".into(),
                    database_url: Some(url),
                    ..base
                }
            }
            _ => Org {
                backend_kind: "sqlite".into(),
                database_url: Some(format!(
                    "sqlite://{}?mode=rwc",
                    self.tmp.path().join(format!("{tag}.db")).display()
                )),
                ..base
            },
        }
    }

    async fn org(&self, slug: &str) -> Option<Org> {
        let rows: Vec<Org> = Org::objects()
            .where_(Org::slug.eq(slug.to_owned()))
            .fetch(&self.registry)
            .await
            .expect("fetch org");
        rows.into_iter().next()
    }

    async fn create(
        &self,
        dir: &Path,
        slug: &str,
        opts: CreateTenantOpts,
    ) -> Result<Org, TenancyError> {
        typed!(self.registry, p => create_tenant(&p, &self.url, dir, slug, opts).await)
    }

    async fn create_if_missing(
        &self,
        dir: &Path,
        slug: &str,
        opts: CreateTenantOpts,
    ) -> Result<Org, TenancyError> {
        typed!(self.registry, p => create_tenant_if_missing(&p, &self.url, dir, slug, opts).await)
    }

    /// Apply `dir` to `org` alone.
    async fn migrate(&self, org: &Org, dir: &Path) {
        typed!(self.registry, p => {
            rustango::tenancy::migrate::migrate_one_tenant(&p, org, dir, &self.url, None)
                .await
                .expect("tenant migrates");
        })
    }

    /// `org`'s applied project migrations.
    async fn applied(&self, org: &Org) -> std::collections::HashSet<String> {
        let pool = typed!(self.registry, p => p.scoped_pool_dyn(org).await.expect("tenant pool"));
        rustango::migrate::applied_set_pool(&pool)
            .await
            .expect("ledger")
    }

    async fn run(&self, dir: &Path, args: &[&str]) -> Result<(), TenancyError> {
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        let mut out = Vec::new();
        typed!(self.registry, p => rustango::tenancy::manage::run_with_writer(&p, &self.url, dir, args, &mut out).await)
    }
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
async fn scratch(guards: &mut Vec<Box<dyn std::any::Any>>, var: &str) -> String {
    let db = scratch_db::ScratchDb::create(&std::env::var(var).unwrap(), "rustango_mt").await;
    let url = db.url().to_owned();
    guards.push(Box::new(db));
    url
}

/// One tenant migration running `sql`.
fn migration(dir: &Path, name: &str, sql: &str) {
    let mig = rustango::migrate::Migration {
        name: name.to_owned(),
        created_at: "2026-10-09T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: rustango::migrate::MigrationScope::Tenant,
        replaces: Vec::new(),
        snapshot: serde_json::from_value(serde_json::json!({ "tables": [] })).unwrap(),
        forward: vec![rustango::migrate::Operation::Data(
            rustango::migrate::DataOp {
                sql: sql.into(),
                reverse_sql: Some("SELECT 1".into()),
                reversible: true,
            },
        )],
    };
    rustango::migrate::file::write(&dir.join(format!("{name}.json")), &mig).unwrap();
}

/// Fails on every backend and writes nothing.
const BROKEN: &str = "SELECT * FROM rustango_no_such_table_2392";

/// #2392 — a failed migration is an error, and the tenant stays inactive.
async fn create_tenant_returns_a_failed_migration(pool: &Pool) {
    let mut env = Env::new(pool).await;
    let dir = env.dir("migrations");
    migration(&dir, "0001_fail", BROKEN);
    let opts = env.tenant_opts("broken").await;
    let err = env
        .create(&dir, "broken", opts)
        .await
        .expect_err("a failed migration must fail create_tenant");
    assert!(
        err.to_string().contains("rustango_no_such_table_2392"),
        "{err}"
    );
    let org = env.org("broken").await.expect("row kept for the operator");
    assert!(!org.active, "a half-migrated tenant must not resolve");
}

/// #2392 — `create_tenant_if_missing` finishes a create that failed at migrate.
async fn create_tenant_if_missing_finishes_a_failed_create(pool: &Pool) {
    let mut env = Env::new(pool).await;
    let dir = env.dir("migrations");
    migration(&dir, "0001_fail", BROKEN);
    let opts = env.tenant_opts("retry").await;
    assert!(env.create(&dir, "retry", opts).await.is_err());
    std::fs::remove_file(dir.join("0001_fail.json")).unwrap();
    migration(&dir, "0001_ok", "SELECT 1");
    let org = env
        .create_if_missing(&dir, "retry", CreateTenantOpts::default())
        .await
        .expect("resume");
    assert!(org.active, "the failed create was not finished");
}

/// #2392 — a suspended tenant is not reactivated by `create_tenant_if_missing`.
async fn create_tenant_if_missing_leaves_a_suspended_tenant(pool: &Pool) {
    let env = Env::new(pool).await;
    let mut org = Org {
        slug: "paused".into(),
        display_name: "paused".into(),
        backend_kind: env.backend().as_str().into(),
        database_url: Some(env.url.clone()),
        active: false,
        ..rustango::testkit::org()
    };
    org.save_pool(&env.registry).await.expect("insert org");
    let got = env
        .create_if_missing(
            &env.dir("migrations"),
            "paused",
            CreateTenantOpts::default(),
        )
        .await
        .expect("existing org");
    assert!(!got.active, "a suspended tenant was reactivated");
}

/// #2392 — a clean run activates the tenant.
async fn create_tenant_activates_after_migrating(pool: &Pool) {
    let mut env = Env::new(pool).await;
    let opts = env.tenant_opts("clean").await;
    let org = env
        .create(&env.dir("migrations"), "clean", opts)
        .await
        .expect("create");
    assert!(org.active);
}

/// #2392 — creating a tenant does not migrate the others.
async fn create_tenant_migrates_only_the_new_tenant(pool: &Pool) {
    let mut env = Env::new(pool).await;
    let mut other = env.own_tenant("other").await;
    other.save_pool(&env.registry).await.expect("other org");
    let first = env.dir("first");
    migration(&first, "0000_first", "SELECT 1");
    env.migrate(&other, &first).await;

    let dir = env.dir("migrations");
    migration(&dir, "0001_new", "SELECT 1");
    let opts = env.tenant_opts("new").await;
    let new = env.create(&dir, "new", opts).await.expect("create");
    assert!(env.applied(&new).await.contains("0001_new"));
    assert!(
        !env.applied(&other).await.contains("0001_new"),
        "an unrelated tenant was migrated"
    );
}

/// #2393 — a migration only an inactive tenant's ledger records is not forgotten.
async fn forget_pending_refuses_a_tenant_applied_migration(pool: &Pool) {
    let mut env = Env::new(pool).await;
    let mut org = Org {
        active: false,
        ..env.own_tenant("fp").await
    };
    org.save_pool(&env.registry).await.expect("insert org");
    let dir = env.dir("migrations");
    migration(&dir, "0001_tenant", "SELECT 1");
    env.migrate(&org, &dir).await;

    let err = env
        .run(&dir, &["forget-pending", "0001_tenant"])
        .await
        .expect_err("a tenant-applied migration must not be forgotten");
    assert!(
        err.to_string()
            .contains(&format!("already applied on tenant `{}`", org.slug)),
        "{err}"
    );
    assert!(
        dir.join("0001_tenant.json").exists(),
        "the JSON was deleted"
    );
}

/// #2393 — a tenant whose ledger can't be read refuses, and a missing SQLite
/// file is not created by looking.
async fn forget_pending_refuses_an_unreadable_tenant(pool: &Pool) {
    let env = Env::new(pool).await;
    let missing = env.tmp.path().join("missing.db");
    let mut org = Org {
        slug: "gone".into(),
        display_name: "gone".into(),
        backend_kind: "sqlite".into(),
        database_url: Some(format!("sqlite://{}?mode=rwc", missing.display())),
        ..rustango::testkit::org()
    };
    org.save_pool(&env.registry).await.expect("insert org");
    let dir = env.dir("migrations");
    migration(&dir, "0001_tenant", "SELECT 1");

    let err = env
        .run(&dir, &["forget-pending", "0001_tenant"])
        .await
        .expect_err("an unreadable tenant must refuse");
    assert!(
        err.to_string().contains("could not read tenant `gone`"),
        "{err}"
    );
    assert!(!missing.exists(), "forget-pending created the tenant file");
    assert!(
        dir.join("0001_tenant.json").exists(),
        "the JSON was deleted"
    );
}

#[cfg(feature = "postgres")]
async fn schema_exists(pool: &Pool, schema: &str) -> bool {
    let Pool::Postgres(pg) = pool else {
        unreachable!()
    };
    rustango::sql::sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = $1)",
    )
    .bind(schema)
    .fetch_one(pg)
    .await
    .unwrap()
}

fn schema_opts(backend: BackendKind, schema: &str) -> CreateTenantOpts {
    CreateTenantOpts {
        mode: StorageMode::Schema,
        backend,
        schema_name: Some(schema.to_owned()),
        no_migrate: true,
        ..CreateTenantOpts::default()
    }
}

#[cfg(feature = "postgres")]
fn schema_request(slug: &str, schema: &str, host: Option<String>) -> ProvisionRequest {
    ProvisionRequest {
        slug: slug.to_owned(),
        mode: StorageMode::Schema,
        backend: BackendKind::Postgres,
        display_name: None,
        database_url: None,
        schema_name: Some(schema.to_owned()),
        host_pattern: host,
        port: None,
        path_prefix: None,
        run_migrations: false,
        preflight: rustango::tenancy::preflight::Preflight::default(),
    }
}

/// #2394 — a schema-mode tenant does not adopt a schema that exists. Off
/// Postgres, schema mode itself is refused and no row is written.
async fn an_existing_schema_is_not_adopted(pool: &Pool) {
    let env = Env::new(pool).await;
    let dir = env.dir("migrations");
    if env.backend() != BackendKind::Postgres {
        let r = env
            .create(&dir, "adopt", schema_opts(env.backend(), "adopt"))
            .await;
        assert!(matches!(r, Err(TenancyError::Validation(_))), "{r:?}");
        assert!(env.org("adopt").await.is_none());
        return;
    }
    #[cfg(feature = "postgres")]
    {
        for sql in ["CREATE SCHEMA app", "CREATE TABLE app.keep (id INT)"] {
            rustango::sql::raw_execute_pool(&env.registry, sql, Vec::new())
                .await
                .unwrap();
        }
        let err = env
            .create(&dir, "adopt", schema_opts(BackendKind::Postgres, "app"))
            .await
            .expect_err("create_tenant adopted the schema");
        assert!(err.to_string().contains("already exists"), "{err}");
        assert!(err.to_string().contains("DROP SCHEMA"), "{err}");
        assert!(env.org("adopt").await.is_none());

        let r = typed!(env.registry, p => provision_tenant(&p, &env.url, &dir, &schema_request("adopt2", "app", None), None).await);
        assert!(
            matches!(&r, Err(e) if e.to_string().contains("already exists")),
            "provision adopted the schema: {r:?}"
        );
        assert!(env.org("adopt2").await.is_none());
        let kept: Vec<(i64,)> = rustango::sql::raw_query_pool(
            "SELECT COUNT(*) FROM app.keep",
            Vec::new(),
            &env.registry,
        )
        .await
        .expect("the app's table is untouched");
        assert_eq!(kept[0].0, 0);
    }
}

/// Run `create` while an uncommitted extra-host row holds `host`; commit it
/// only once `schema` exists, so the INSERT loses after the schema is made.
#[cfg(feature = "postgres")]
async fn losing_the_insert<T, F>(env: &Env, host: &str, schema: &str, create: F) -> T
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let mut owner = Org {
        slug: "owner".into(),
        display_name: "owner".into(),
        database_url: Some(env.url.clone()),
        ..rustango::testkit::org()
    };
    owner.save_pool(&env.registry).await.expect("owner");
    let mut tx = rustango::sql::transaction_pool(&env.registry)
        .await
        .expect("begin");
    let mut row = rustango::tenancy::OrgHost {
        id: rustango::sql::Auto::Unset,
        org_id: *owner.id.get().unwrap(),
        hostname: host.to_owned(),
        enabled: true,
        created_at: rustango::sql::Auto::Unset,
    };
    row.insert_tx(&mut tx).await.expect("held host");
    let create = tokio::spawn(create);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !schema_exists(&env.registry, schema).await {
        assert!(
            std::time::Instant::now() < deadline,
            "the schema was never created"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tx.commit().await.expect("commit");
    create.await.expect("join")
}

/// #2394 — when `create_tenant`'s Org row fails, the schema it made is dropped.
async fn create_tenant_releases_its_schema_on_a_failed_insert(pool: &Pool) {
    let env = Env::new(pool).await;
    if env.backend() != BackendKind::Postgres {
        return;
    }
    #[cfg(feature = "postgres")]
    {
        let host = "race.example.test";
        let opts = CreateTenantOpts {
            host_pattern: Some(host.to_owned()),
            ..schema_opts(BackendKind::Postgres, "race")
        };
        let (registry, url) = (env.registry.clone(), env.url.clone());
        let r = losing_the_insert(&env, host, "race", async move {
            typed!(registry, p => create_tenant(&p, &url, Path::new("none"), "race", opts).await)
        })
        .await;
        assert!(
            matches!(&r, Err(e) if e.to_string().contains("already used")),
            "{r:?}"
        );
        assert!(
            !schema_exists(&env.registry, "race").await,
            "the new schema was left behind"
        );
    }
}

/// #2394 — the same for `provision_tenant`.
async fn provision_releases_its_schema_on_a_failed_insert(pool: &Pool) {
    let env = Env::new(pool).await;
    if env.backend() != BackendKind::Postgres {
        return;
    }
    #[cfg(feature = "postgres")]
    {
        let host = "race.example.test";
        let request = schema_request("race", "race", Some(host.to_owned()));
        let (registry, url) = (env.registry.clone(), env.url.clone());
        let r = losing_the_insert(&env, host, "race", async move {
            typed!(registry, p => provision_tenant(&p, &url, Path::new("none"), &request, None).await)
        })
        .await;
        assert!(
            matches!(&r, Err(e) if e.to_string().contains("already used")),
            "{r:?}"
        );
        assert!(
            !schema_exists(&env.registry, "race").await,
            "the new schema was left behind"
        );
    }
}

tri_dialect_test! {
    setup: setup,
    sqlite: file,
    scenarios: [
        create_tenant_returns_a_failed_migration,
        create_tenant_if_missing_finishes_a_failed_create,
        create_tenant_if_missing_leaves_a_suspended_tenant,
        create_tenant_activates_after_migrating,
        create_tenant_migrates_only_the_new_tenant,
        forget_pending_refuses_a_tenant_applied_migration,
        forget_pending_refuses_an_unreadable_tenant,
        an_existing_schema_is_not_adopted,
        create_tenant_releases_its_schema_on_a_failed_insert,
        provision_releases_its_schema_on_a_failed_insert,
    ],
}
