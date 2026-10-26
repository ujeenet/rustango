//! Tenancy manage verbs and `api::create_tenant`, on every registry backend.
//! Each scenario gets a scratch registry, so no rows outlive it.

#![cfg(all(feature = "tenancy", feature = "testkit", feature = "sqlite"))]

use std::path::{Path, PathBuf};
#[cfg(feature = "postgres")]
use std::time::Duration;

use rustango::core::Column as _;
use rustango::sql::{FetcherPool as _, Pool};
use rustango::tenancy::manage::api::{create_tenant, create_tenant_if_missing, CreateTenantOpts};
use rustango::tenancy::provision::{provision_tenant, ProvisionOutcome, ProvisionRequest};
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
            backend: Some(self.backend()),
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

    /// A database-mode tenant URL on the registry's backend; PG and MySQL
    /// ones carry a password.
    async fn tenant_db_url(&mut self, tag: &str) -> String {
        match self.backend() {
            #[cfg(feature = "postgres")]
            BackendKind::Postgres => scratch(&mut self.guards, "DATABASE_URL").await,
            #[cfg(feature = "mysql")]
            BackendKind::MySql => scratch(&mut self.guards, "MYSQL_TEST_URL").await,
            _ => format!(
                "sqlite://{}?mode=rwc",
                self.tmp.path().join(format!("{tag}.db")).display()
            ),
        }
    }

    /// A URL the registry's driver does not speak. The SQLite file one is
    /// reachable, so only the backend check can refuse it.
    fn foreign_url(&self) -> String {
        match self.backend() {
            BackendKind::Sqlite => "postgres://u:pw@127.0.0.1:1/x".into(),
            _ => format!(
                "sqlite://{}?mode=rwc",
                self.tmp.path().join("foreign.db").display()
            ),
        }
    }

    /// A same-backend URL nothing answers on.
    fn unreachable_url(&self) -> String {
        match self.backend() {
            BackendKind::Postgres => "postgres://u:pw@127.0.0.1:1/x".into(),
            BackendKind::MySql => "mysql://u:pw@127.0.0.1:1/x".into(),
            BackendKind::Sqlite => format!(
                "sqlite://{}?mode=rwc",
                self.tmp.path().join("no/such/dir/x.db").display()
            ),
        }
    }

    fn db_request(&self, slug: &str, url: &str) -> ProvisionRequest {
        ProvisionRequest {
            backend: Some(self.backend()),
            ..ProvisionRequest::database(slug, url)
        }
    }

    async fn provision(
        &self,
        request: &ProvisionRequest,
    ) -> Result<ProvisionOutcome, TenancyError> {
        let dir = self.dir("migrations");
        typed!(self.registry, p => provision_tenant(&p, &self.url, &dir, request, None).await)
    }

    async fn run(&self, dir: &Path, args: &[&str]) -> Result<(), TenancyError> {
        self.run_out(dir, args).await.map(drop)
    }

    async fn run_out(&self, dir: &Path, args: &[&str]) -> Result<String, TenancyError> {
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        let mut out = Vec::new();
        typed!(self.registry, p => rustango::tenancy::manage::run_with_writer(&p, &self.url, dir, args, &mut out).await)?;
        Ok(String::from_utf8_lossy(&out).into_owned())
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
        merges: Vec::new(),
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
        backend: Some(backend),
        schema_name: Some(schema.to_owned()),
        no_migrate: true,
        ..CreateTenantOpts::default()
    }
}

#[cfg(feature = "postgres")]
fn schema_request(slug: &str, schema: &str, host: Option<String>) -> ProvisionRequest {
    ProvisionRequest {
        backend: Some(BackendKind::Postgres),
        host_pattern: host,
        run_migrations: false,
        ..ProvisionRequest::new(
            slug,
            rustango::tenancy::provision::TenantStorage::Schema(Some(schema.to_owned())),
        )
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

/// A schema named after a database role would lead that role's default
/// `search_path` ("$user", public), so it is refused, and none is made.
async fn a_schema_named_after_a_role_is_refused(pool: &Pool) {
    let env = Env::new(pool).await;
    if env.backend() != BackendKind::Postgres {
        return;
    }
    #[cfg(feature = "postgres")]
    {
        let dir = env.dir("migrations");
        let role: Vec<(String,)> = rustango::sql::raw_query_pool(
            "SELECT CAST(current_user AS TEXT)",
            Vec::new(),
            &env.registry,
        )
        .await
        .unwrap();
        let role = role[0].0.clone();
        let err = env
            .create(&dir, "rolecli", schema_opts(BackendKind::Postgres, &role))
            .await
            .expect_err("create_tenant took the role's schema");
        assert!(err.to_string().contains("role"), "{err}");
        let r = env
            .provision(&schema_request("roleprov", &role, None))
            .await;
        assert!(
            matches!(&r, Err(e) if e.to_string().contains("role")),
            "provision took the role's schema: {r:?}"
        );
        assert!(!schema_exists(&env.registry, &role).await, "schema made");
        assert!(env.org("rolecli").await.is_none());
        assert!(env.org("roleprov").await.is_none());
    }
}

/// A tenant that already lives in a role-named schema is named by `check --deploy`.
async fn check_deploy_names_a_role_named_schema(pool: &Pool) {
    let env = Env::new(pool).await;
    if env.backend() != BackendKind::Postgres {
        return;
    }
    let role: Vec<(String,)> =
        rustango::sql::raw_query_pool("SELECT CAST(current_user AS TEXT)", Vec::new(), &env.registry)
            .await
            .unwrap();
    let mut org = Org {
        slug: "rolehome".into(),
        display_name: "rolehome".into(),
        storage_mode: "schema".into(),
        schema_name: Some(role[0].0.clone()),
        ..rustango::testkit::org()
    };
    org.save_pool(&env.registry).await.expect("insert org");
    let dir = env.dir("migrations");
    let mut out = Vec::new();
    let args = vec!["check".to_owned(), "--deploy".to_owned()];
    let _ = typed!(env.registry, p => rustango::tenancy::manage::run_with_writer(&p, &env.url, &dir, args, &mut out).await);
    let out = String::from_utf8_lossy(&out);
    assert!(out.contains("`rolehome`") && out.contains("database role"), "{out}");
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

/// A create of a taken slug is refused before it opens a run.
async fn a_duplicate_slug_leaves_no_run(pool: &Pool) {
    use rustango::tenancy::provision_store::ProvisioningRun;
    let mut env = Env::new(pool).await;
    let dir = env.dir("migrations");
    let opts = env.tenant_opts("dup").await;
    env.create(&dir, "dup", opts).await.expect("first create");
    let opts = env.tenant_opts("dup2").await;
    let err = env.create(&dir, "dup", opts).await.expect_err("taken slug");
    assert!(err.to_string().contains("already exists"), "{err}");
    let runs: Vec<ProvisioningRun> = ProvisioningRun::objects()
        .where_(ProvisioningRun::slug.eq("dup".to_owned()))
        .fetch(&env.registry)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1, "the refused create left a run: {runs:?}");
}

/// Two creates naming one schema at once, by name or slug default: one
/// tenant lives there, the other is refused. Both create paths race.
async fn concurrent_creates_never_share_a_schema(pool: &Pool) {
    let env = Env::new(pool).await;
    if env.backend() != BackendKind::Postgres {
        return;
    }
    #[cfg(feature = "postgres")]
    for round in 0..20 {
        let shared = format!("sh{round}");
        // Odd rounds: the API tenant's slug is the schema, by default.
        let (api_slug, api_schema) = if round % 2 == 1 {
            (shared.clone(), None)
        } else {
            (format!("{shared}b"), Some(shared.clone()))
        };
        let barrier = tokio::sync::Barrier::new(2);
        let engine = async {
            barrier.wait().await;
            let request = schema_request(&format!("{shared}a"), &shared, None);
            typed!(env.registry, p => provision_tenant(&p, &env.url, Path::new("none"), &request, None).await)
                .map(drop)
        };
        let api = async {
            barrier.wait().await;
            let opts = CreateTenantOpts {
                schema_name: api_schema,
                ..schema_opts(BackendKind::Postgres, "unused")
            };
            env.create(Path::new("none"), &api_slug, opts)
                .await
                .map(drop)
        };
        let (x, y) = tokio::join!(engine, api);
        assert_eq!(
            x.is_ok() as u8 + y.is_ok() as u8,
            1,
            "round {round}: {x:?} {y:?}"
        );
        let refused = x.err().or(y.err()).unwrap().to_string();
        assert!(refused.contains("already"), "round {round}: {refused}");
        let orgs: Vec<Org> = Org::objects().fetch(&env.registry).await.unwrap();
        let sharing = orgs
            .iter()
            .filter(|o| o.schema_name.as_deref().unwrap_or(&o.slug) == shared)
            .count();
        assert_eq!(sharing, 1, "round {round}: tenants share schema `{shared}`");
    }
}

/// `dumpdata` / `loaddata` run on the `--tenant` pool and touch only
/// that tenant; without it they cover registry models only.
async fn data_verbs_stay_in_their_tenant(pool: &Pool) {
    let mut env = Env::new(pool).await;
    let dir = env.dir("migrations");
    for (tag, user) in [("a", "alice"), ("b", "bob")] {
        let opts = env.tenant_opts(tag).await;
        let slug = format!("mt-{tag}");
        env.create(&dir, &slug, opts).await.expect("tenant");
        env.run(
            &dir,
            &[
                "create-user",
                &slug,
                user,
                "--skip-validation",
                "--password",
                "pw",
            ],
        )
        .await
        .expect("user");
    }
    let dump = |slug: Option<&'static str>, model: &'static str| {
        let mut args = vec!["dumpdata", "--model", model, "--indent", "0"];
        args.extend(slug.map(|s| ["--tenant", s]).into_iter().flatten());
        let env = &env;
        let dir = dir.clone();
        async move { env.run_out(&dir, &args).await }
    };

    let a = dump(Some("mt-a"), "User").await.expect("dump mt-a");
    assert!(a.contains("alice") && !a.contains("bob"), "{a}");
    let orgs = dump(None, "Org").await.expect("registry dump");
    assert!(orgs.contains("mt-a") && orgs.contains("mt-b"), "{orgs}");
    let err = dump(None, "User")
        .await
        .expect_err("a tenant model on the registry");
    assert!(err.to_string().contains("--tenant"), "{err}");

    // Alice's row, renamed, into mt-b only.
    let mut rows: serde_json::Value = serde_json::from_str(a.trim()).unwrap();
    let row = &mut rows[0];
    row["pk"] = 99.into();
    row["fields"]["username"] = "carol".into();
    let fixture = env.tmp.path().join("carol.json");
    std::fs::write(&fixture, serde_json::to_string(&rows).unwrap()).unwrap();
    let fixture = fixture.to_str().unwrap();
    let err = env
        .run(&dir, &["loaddata", fixture, "--fail-fast"])
        .await
        .expect_err("a tenant fixture on the registry");
    assert!(err.to_string().contains("--tenant"), "{err}");
    env.run(&dir, &["loaddata", fixture, "--tenant", "mt-b"])
        .await
        .expect("load into mt-b");
    let b = dump(Some("mt-b"), "User").await.expect("dump mt-b");
    assert!(b.contains("carol") && b.contains("bob"), "{b}");
    let a = dump(Some("mt-a"), "User").await.expect("dump mt-a");
    assert!(!a.contains("carol"), "the load reached another tenant: {a}");

    // No --model: only that side's models. Unmanaged tables (TOTP and the
    // like) are made on first use; a missing one is skipped.
    let all = env
        .run_out(&dir, &["dumpdata", "--tenant", "mt-a", "--indent", "0"])
        .await
        .expect("full tenant dump");
    assert!(all.contains("alice"), "{all}");
    assert!(!all.contains("mt-b") && !all.contains("bob"), "{all}");
    let reg = env
        .run_out(&dir, &["dumpdata", "--indent", "0"])
        .await
        .expect("full registry dump");
    assert!(reg.contains("mt-b") && !reg.contains("alice"), "{reg}");
}

/// PG schema mode: a table missing from the tenant schema is refused, not
/// read from or written to `public`.
async fn schema_mode_data_verbs_skip_public(pool: &Pool) {
    let mut env = Env::new(pool).await;
    if env.backend() != BackendKind::Postgres {
        return;
    }
    let dir = env.dir("migrations");
    for tag in ["a", "b"] {
        let opts = env.tenant_opts(tag).await;
        env.create(&dir, &format!("mt-{tag}"), opts)
            .await
            .expect("tenant");
    }
    let user = [
        "create-user",
        "mt-b",
        "bob",
        "--skip-validation",
        "--password",
        "pw",
    ];
    env.run(&dir, &user).await.expect("user");
    let b = env
        .run_out(
            &dir,
            &[
                "dumpdata", "--tenant", "mt-b", "--model", "User", "--indent", "0",
            ],
        )
        .await
        .expect("dump mt-b");
    let fixture = env.tmp.path().join("bob.json");
    std::fs::write(&fixture, b.trim()).unwrap();
    rustango::sql::raw_execute_pool(
        &env.registry,
        "DROP TABLE mt_a.rustango_users CASCADE",
        Vec::new(),
    )
    .await
    .unwrap();

    for args in [
        &["dumpdata", "--tenant", "mt-a", "--model", "User"][..],
        &["dumpdata", "--tenant", "mt-a"][..],
        &["loaddata", fixture.to_str().unwrap(), "--tenant", "mt-a"][..],
    ] {
        let err = env
            .run(&dir, args)
            .await
            .expect_err("fell through to public");
        assert!(err.to_string().contains("mt_a"), "{args:?}: {err}");
    }
    let public: Vec<rustango::tenancy::auth::User> = rustango::tenancy::auth::User::objects()
        .fetch(&env.registry)
        .await
        .unwrap();
    assert!(public.is_empty(), "loaddata wrote to public");
}

/// One key for every scenario here, so parallel tests never disagree.
fn with_secret_key() {
    std::env::set_var(
        "RUSTANGO_SECRET_KEY",
        "tenancy-manage-tri-key-0123456789abcdef",
    );
}

/// The preflight probes the URL an `env://` reference names, and the row
/// keeps the reference.
async fn provision_resolves_an_env_reference(pool: &Pool) {
    let mut env = Env::new(pool).await;
    let url = env.tenant_db_url("envref").await;
    let var = format!(
        "RUSTANGO_MT_ENVREF_{}",
        env.backend().as_str().to_uppercase()
    );
    std::env::set_var(&var, &url);
    let reference = format!("env://{var}");
    env.provision(&env.db_request("envref", &reference))
        .await
        .expect("an env:// reference provisions");
    let org = env.org("envref").await.expect("row");
    assert!(org.active);
    assert_eq!(org.database_url.as_deref(), Some(reference.as_str()));
}

/// `api::create_tenant` probes before it writes, like the provisioner.
async fn create_tenant_probes_before_writing(pool: &Pool) {
    let env = Env::new(pool).await;
    let opts = CreateTenantOpts {
        mode: StorageMode::Database,
        backend: Some(env.backend()),
        database_url: Some(env.unreachable_url()),
        ..CreateTenantOpts::default()
    };
    let err = env
        .create(&env.dir("migrations"), "unreach", opts)
        .await
        .expect_err("nothing answers");
    assert!(
        env.org("unreach").await.is_none(),
        "a row was written before the probe: {err}"
    );
}

/// Another backend's URL is refused at provision and at edit.
async fn a_url_of_another_backend_is_refused(pool: &Pool) {
    let mut env = Env::new(pool).await;
    let foreign = env.foreign_url();
    let err = env
        .provision(&env.db_request("foreign", &foreign))
        .await
        .err()
        .expect("another backend's URL");
    assert!(err.to_string().contains("registry runs on"), "{err}");
    assert!(env.org("foreign").await.is_none());

    let url = env.tenant_db_url("edited").await;
    let mut org = Org {
        slug: "edited".into(),
        display_name: "edited".into(),
        backend_kind: env.backend().as_str().into(),
        database_url: Some(url.clone()),
        ..rustango::testkit::org()
    };
    org.save_pool(&env.registry).await.expect("insert org");
    let dir = env.dir("migrations");
    let err = env
        .run(&dir, &["edit-tenant", "edited", "--database-url", &foreign])
        .await
        .expect_err("edit to another backend");
    assert!(err.to_string().contains("registry runs on"), "{err}");
    let kept = env.org("edited").await.expect("row").database_url;
    assert_eq!(kept.as_deref(), Some(url.as_str()));
}

/// A literal URL with a password is stored sealed by provision and edit,
/// and still opens through the default resolver.
async fn a_password_url_is_stored_sealed(pool: &Pool) {
    with_secret_key();
    let mut env = Env::new(pool).await;
    let url = env.tenant_db_url("sealed").await;
    env.provision(&env.db_request("sealed", &url))
        .await
        .expect("provisions");
    let org = env.org("sealed").await.expect("row");
    assert!(org.active);
    let stored = org.database_url.clone().expect("url");
    if env.backend() == BackendKind::Sqlite {
        assert_eq!(stored, url, "no password, nothing to seal");
        // sqlx refuses `?password=` on SQLite, so one is never stored, and
        // the refusal masks it.
        let with_password = format!("{url}&password=hunter2");
        let err = env
            .run(
                &env.dir("migrations"),
                &["edit-tenant", "sealed", "--database-url", &with_password],
            )
            .await
            .expect_err("sqlx refuses the parameter")
            .to_string();
        assert!(!err.contains("hunter2"), "{err}");
        let kept = env.org("sealed").await.expect("row").database_url;
        assert_eq!(kept.as_deref(), Some(url.as_str()));
        return;
    }
    assert!(stored.starts_with("enc:v1:"), "{stored}");
    let opened = typed!(env.registry, p => p.resolved_database_url(&org).await.expect("opens"));
    assert_eq!(opened, url);

    let dir = env.dir("migrations");
    env.run(&dir, &["edit-tenant", "sealed", "--database-url", &url])
        .await
        .expect("edit");
    let edited = env
        .org("sealed")
        .await
        .expect("row")
        .database_url
        .expect("url");
    assert!(
        edited.starts_with("enc:v1:") && edited != stored,
        "{edited}"
    );
}

/// A reference resolving to a bare secret is never echoed: not by
/// provision, the run store, or the CLI.
async fn a_resolved_secret_is_never_echoed(pool: &Pool) {
    use rustango::tenancy::provision_store::{ProvisioningEvent, ProvisioningRun};
    let env = Env::new(pool).await;
    let var = format!("RUSTANGO_MT_LEAK_{}", env.backend().as_str().to_uppercase());
    std::env::set_var(&var, "hunter2-leak");
    let reference = format!("env://{var}");
    let dir = env.dir("migrations");
    let request = env.db_request("leak", &reference);
    let r = typed!(env.registry, p => rustango::tenancy::provision::provision_tenant_recorded(
        &p, &env.url, &dir, &request, None, None, None).await.map(drop));
    let err = r.expect_err("not a URL").to_string();
    assert!(!err.contains("hunter2"), "{err}");
    let runs: Vec<ProvisioningRun> = ProvisioningRun::objects()
        .fetch(&env.registry)
        .await
        .unwrap();
    let events: Vec<ProvisioningEvent> = ProvisioningEvent::objects()
        .fetch(&env.registry)
        .await
        .unwrap();
    let stored = format!("{runs:?}{events:?}");
    assert!(!runs.is_empty() && !stored.contains("hunter2"), "{stored}");

    let mut org = Org {
        slug: "leak-edit".into(),
        display_name: "leak-edit".into(),
        backend_kind: env.backend().as_str().into(),
        database_url: Some(env.unreachable_url()),
        ..rustango::testkit::org()
    };
    org.save_pool(&env.registry).await.expect("insert org");
    let err = env
        .run(
            &dir,
            &["edit-tenant", "leak-edit", "--database-url", &reference],
        )
        .await
        .expect_err("not a URL")
        .to_string();
    assert!(
        !err.contains("hunter2") && err.contains(&reference),
        "{err}"
    );
}

/// `migrate-tenant-storage --to database` checks the target like create does.
async fn migrate_storage_checks_the_target_url(pool: &Pool) {
    let mut env = Env::new(pool).await;
    if env.backend() != BackendKind::Postgres {
        return;
    }
    let dir = env.dir("migrations");
    let opts = env.tenant_opts("mv").await;
    env.create(&dir, "mt-mv", opts)
        .await
        .expect("schema tenant");
    let foreign = env.foreign_url();
    let err = env
        .run(
            &dir,
            &[
                "migrate-tenant-storage",
                "mt-mv",
                "--to",
                "database",
                "--database-url",
                &foreign,
                "--dry-run",
            ],
        )
        .await
        .expect_err("another backend's URL");
    assert!(err.to_string().contains("registry runs on"), "{err}");
}

/// The registry under another host name, or a SQLite hard link, passes
/// the endpoint check; the connection check names it (a warning for now).
async fn the_registry_under_an_alias_is_flagged(pool: &Pool) {
    let env = Env::new(pool).await;
    let alias = match env.backend() {
        BackendKind::Sqlite if cfg!(unix) => {
            let link = env.tmp.path().join("alias.db");
            std::fs::hard_link(env.tmp.path().join("reg.db"), &link).unwrap();
            format!("sqlite://{}", link.display())
        }
        BackendKind::Sqlite => return,
        _ if env.url.contains("127.0.0.1") => env.url.replace("127.0.0.1", "localhost"),
        _ => env.url.replace("localhost", "127.0.0.1"),
    };
    assert_ne!(alias, env.url, "the URL names neither localhost nor 127.0.0.1");
    let request = ProvisionRequest {
        run_migrations: false,
        ..env.db_request("alias", &alias)
    };
    let dir = env.dir("migrations");
    let (run, _) = typed!(env.registry, p => rustango::tenancy::provision::provision_tenant_recorded(
        &p, &env.url, &dir, &request, None, None, None,
    )
    .await)
    .expect("only warned in this release");
    let events =
        rustango::tenancy::provision_store::events_since(&env.registry, *run.id.get().unwrap(), 0)
            .await
            .unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.message.contains("registry's own database")),
        "{:?}",
        events.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

/// `reseal-tenant-secrets` lists, then seals, a plain password URL.
async fn reseal_seals_a_plain_password_url(pool: &Pool) {
    with_secret_key();
    let mut env = Env::new(pool).await;
    let url = env.tenant_db_url("plain").await;
    let mut org = Org {
        slug: "plain".into(),
        display_name: "plain".into(),
        backend_kind: env.backend().as_str().into(),
        database_url: Some(url.clone()),
        ..rustango::testkit::org()
    };
    org.save_pool(&env.registry).await.expect("insert org");
    let dir = env.dir("migrations");
    let out = env
        .run_out(&dir, &["reseal-tenant-secrets"])
        .await
        .expect("dry run");
    let stored = || async {
        env.org("plain")
            .await
            .expect("row")
            .database_url
            .expect("url")
    };
    assert_eq!(stored().await, url, "a dry run wrote: {out}");
    if env.backend() == BackendKind::Sqlite {
        assert!(
            out.contains("0 to change"),
            "no password, nothing to seal: {out}"
        );
        return;
    }
    assert!(out.contains("plain: would seal"), "{out}");
    env.run_out(&dir, &["reseal-tenant-secrets", "--apply"])
        .await
        .expect("apply");
    let sealed = stored().await;
    assert!(sealed.starts_with("enc:v1:"), "{sealed}");
    let org = env.org("plain").await.expect("row");
    let opened = typed!(env.registry, p => p.resolved_database_url(&org).await.expect("opens"));
    assert_eq!(opened, url);
}

/// `--apply` leaves a sealed URL without a password as it is, not unsealed.
async fn reseal_leaves_a_sealed_url_without_a_password(pool: &Pool) {
    with_secret_key();
    let mut env = Env::new(pool).await;
    let plain = match env.backend() {
        BackendKind::Sqlite => env.tenant_db_url("nopw").await,
        BackendKind::Postgres => "postgres://u@127.0.0.1:1/x".into(),
        BackendKind::MySql => "mysql://u@127.0.0.1:1/x".into(),
    };
    let sealed = rustango::tenancy::seal(&plain).expect("seal");
    let mut org = Org {
        slug: "nopw".into(),
        display_name: "nopw".into(),
        backend_kind: env.backend().as_str().into(),
        database_url: Some(sealed.clone()),
        ..rustango::testkit::org()
    };
    org.save_pool(&env.registry).await.expect("insert org");
    let dir = env.dir("migrations");
    let out = env
        .run_out(&dir, &["reseal-tenant-secrets", "--apply"])
        .await;
    // The server arms point at no server: its SSO secrets can't be reached,
    // which counts as a failure, but the URL is still left as it is.
    // Without `sso` the tenant is never opened.
    match env.backend() {
        BackendKind::Sqlite => drop(out.expect("apply")),
        _ if !cfg!(feature = "sso") => drop(out.expect("apply")),
        _ => {
            assert!(format!("{:?}", out.expect_err("unreachable")).contains("could not be resealed"))
        }
    }
    let stored = env.org("nopw").await.expect("row").database_url;
    assert_eq!(stored.as_deref(), Some(sealed.as_str()));
}

tri_dialect_test! {
    setup: setup,
    sqlite: file,
    scenarios: [
        reseal_leaves_a_sealed_url_without_a_password,
        a_resolved_secret_is_never_echoed,
        migrate_storage_checks_the_target_url,
        reseal_seals_a_plain_password_url,
        provision_resolves_an_env_reference,
        create_tenant_probes_before_writing,
        a_url_of_another_backend_is_refused,
        a_password_url_is_stored_sealed,
        data_verbs_stay_in_their_tenant,
        schema_mode_data_verbs_skip_public,
        create_tenant_returns_a_failed_migration,
        create_tenant_if_missing_finishes_a_failed_create,
        create_tenant_if_missing_leaves_a_suspended_tenant,
        create_tenant_activates_after_migrating,
        create_tenant_migrates_only_the_new_tenant,
        forget_pending_refuses_a_tenant_applied_migration,
        forget_pending_refuses_an_unreadable_tenant,
        an_existing_schema_is_not_adopted,
        a_schema_named_after_a_role_is_refused,
        create_tenant_releases_its_schema_on_a_failed_insert,
        provision_releases_its_schema_on_a_failed_insert,
        concurrent_creates_never_share_a_schema,
        a_duplicate_slug_leaves_no_run,
        the_registry_under_an_alias_is_flagged,
        check_deploy_names_a_role_named_schema,
    ],
}
