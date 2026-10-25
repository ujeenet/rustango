//! Backing test for `docs/tenant-databases.md` — purging and rotating a
//! tenant whose database rustango made on a saved server, on every
//! registry backend.
//!
//! The server arms create databases and logins, so `DATABASE_URL` and
//! `MYSQL_TEST_URL` must be admin logins (MySQL: root).

#![cfg(all(feature = "tenancy", feature = "testkit", feature = "sqlite"))]

use std::path::{Path, PathBuf};
use std::time::Duration;

use rustango::core::Column as _;
use rustango::sql::{DatabaseName, FetcherPool as _, Login, LoginPassword, Pool, UpdaterPool as _};
use rustango::tenancy::provision::{self, ProvisionRequest};
use rustango::tenancy::provision_store::{self as store, ProvisioningRun, RunKind, RunState};
use rustango::tenancy::server_admin::{tenant_login_name, ServerAdmin};
use rustango::tenancy::{
    db_server, BackendKind, DbServerSpec, Org, TenancyError, TenantDatabase, TenantPools, TlsMode,
};
use rustango::tri_dialect_test;

#[cfg(any(feature = "postgres", feature = "mysql"))]
#[path = "support/scratch_db.rs"]
mod scratch_db;

const KEY: &str = "server-tenant-tri-key-0123456789abcdefgh";
const T: Duration = Duration::from_secs(10);

/// The scenarios set `RUSTANGO_SECRET_KEY`.
fn env_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

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

async fn setup(_: &Pool) {}

/// Key set, and pods converge fast so a MySQL rotation waits two seconds.
fn prepare() {
    std::env::set_var("RUSTANGO_SECRET_KEY", KEY);
    std::env::set_var("RUSTANGO_HOST_GEN_CHECK_SECS", "1");
}

fn slug(tag: &str) -> String {
    format!("st{}-{tag}", std::process::id())
}

fn db_name(slug: &str) -> DatabaseName {
    DatabaseName::new(slug).unwrap()
}

fn password_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let userinfo = rest.rsplit_once('@')?.0;
    userinfo.split_once(':').map(|(_, p)| p.to_owned())
}

async fn exec_text(pool: &Pool, sql: &str) {
    use rustango::sql::sqlx;
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(p) => drop(sqlx::raw_sql(sql).execute(p).await.expect(sql)),
        #[cfg(feature = "mysql")]
        Pool::Mysql(p) => drop(sqlx::raw_sql(sql).execute(p).await.expect(sql)),
        Pool::Sqlite(p) => drop(sqlx::raw_sql(sql).execute(p).await.expect(sql)),
    }
}

struct Env {
    url: String,
    registry: Pool,
    tmp: tempfile::TempDir,
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
        let env = Self {
            url,
            registry,
            tmp,
            guards,
        };
        write_migrations(&env.dir("good"));
        env
    }

    fn backend(&self) -> BackendKind {
        BackendKind::of_pool(&self.registry)
    }

    fn server_backend(&self) -> bool {
        self.backend() != BackendKind::Sqlite
    }

    fn dir(&self, name: &str) -> PathBuf {
        let dir = self.tmp.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The admin URL's user and password, host and port.
    fn admin_parts(&self) -> (String, String, String, u16) {
        let rest = self.url.split_once("://").unwrap().1;
        let (userinfo, rest) = rest.rsplit_once('@').unwrap();
        let (user, pw) = userinfo.split_once(':').unwrap_or((userinfo, ""));
        let authority = rest.split(['/', '?']).next().unwrap();
        let (host, port) = authority.rsplit_once(':').unwrap();
        (user.into(), pw.into(), host.into(), port.parse().unwrap())
    }

    /// Save server `name` as `login`, else the live admin; a directory on SQLite.
    async fn add_server_as(&self, name: &str, login: Option<(&str, &str)>) {
        let base = DbServerSpec {
            name: name.into(),
            backend: self.backend(),
            active: true,
            ..DbServerSpec::default()
        };
        if !self.server_backend() {
            let spec = DbServerSpec {
                sqlite_dir: Some(self.dir("tenants").display().to_string()),
                ..base
            };
            db_server::add(&self.registry, &spec, None).await.expect("add server");
            return;
        }
        let (admin, admin_pw, host, port) = self.admin_parts();
        let (user, pw) = login.unwrap_or((&admin, &admin_pw));
        let spec = DbServerSpec {
            host: Some(host),
            port: Some(port),
            admin_user: Some(user.into()),
            tls: Some(TlsMode::Disable),
            ..base
        };
        let pw = LoginPassword::new(pw).unwrap();
        db_server::add(&self.registry, &spec, Some(&pw)).await.expect("add server");
    }

    async fn admin(&self) -> ServerAdmin {
        if self.server_backend() {
            ServerAdmin::connect(&self.url, T).await.expect("admin")
        } else {
            ServerAdmin::sqlite_dir(self.dir("tenants"))
        }
    }

    async fn clean(&self, slugs: &[&str]) {
        let admin = self.admin().await;
        for s in slugs {
            let name = db_name(s);
            let login = Login::new(tenant_login_name(&name));
            for attempt in 0.. {
                match admin.drop_tenant_database(&name, &login).await {
                    Ok(()) => break,
                    Err(_) if attempt < 50 => tokio::time::sleep(Duration::from_millis(100)).await,
                    Err(e) => panic!("clean {s}: {e}"),
                }
            }
        }
        admin.close().await;
    }

    async fn database_exists(&self, slug: &str) -> bool {
        if !self.server_backend() {
            return self.dir("tenants").join(format!("{slug}.db")).exists();
        }
        match Pool::connect_admin(&self.url, Some(slug), T).await {
            Ok(p) => {
                p.close().await;
                true
            }
            Err(_) => false,
        }
    }

    /// Neither the database nor its login exists: making both again works.
    async fn nothing_left(&self, slug: &str) -> bool {
        let admin = self.admin().await;
        let name = db_name(slug);
        let login = Login::new(tenant_login_name(&name));
        let made = admin
            .create_tenant_database(&name, &login, &LoginPassword::generate())
            .await;
        let ok = made.is_ok();
        if ok {
            admin.drop_tenant_database(&name, &login).await.expect("drop again");
        }
        admin.close().await;
        ok
    }

    async fn org(&self, slug: &str) -> Option<Org> {
        Org::objects()
            .where_(Org::slug.eq(slug.to_owned()))
            .first(&self.registry)
            .await
            .expect("fetch org")
    }

    async fn records(&self, slug: &str) -> Vec<TenantDatabase> {
        TenantDatabase::objects()
            .where_(TenantDatabase::org_slug.eq(slug.to_owned()))
            .fetch(&self.registry)
            .await
            .expect("fetch records")
    }

    async fn resolved_url(&self, org: &Org) -> String {
        typed!(self.registry, p => p.resolved_database_url(org).await.expect("opens"))
    }

    async fn provision(&self, request: &ProvisionRequest) {
        let dir = self.dir("good");
        typed!(self.registry, p => provision::provision_tenant_recorded(
            &p, &self.url, &dir, request, None, None, None,
        )
        .await)
        .expect("provision");
    }

    async fn run(&self, args: &[&str]) -> Result<String, TenancyError> {
        let dir = self.dir("good");
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        let mut out = Vec::new();
        typed!(self.registry, p => rustango::tenancy::manage::run_with_writer(&p, &self.url, &dir, args, &mut out).await)
            .map(|()| String::from_utf8_lossy(&out).into_owned())
    }

    async fn purge(&self, slug: &str) -> Result<String, TenancyError> {
        self.run(&["purge-tenant", slug, "--confirm", slug, "--purge-database"])
            .await
    }

    /// The latest run of `kind` for `slug`, and its step/status pairs.
    async fn last_run(&self, slug: &str, kind: RunKind) -> (ProvisioningRun, Vec<String>) {
        let run = ProvisioningRun::objects()
            .where_(ProvisioningRun::slug.eq(slug.to_owned()))
            .where_(ProvisioningRun::kind.eq(kind.as_str().to_owned()))
            .order_by(&[("id", true)])
            .first(&self.registry)
            .await
            .unwrap()
            .expect("a run");
        let id = run.id.get().copied().unwrap_or_default();
        let steps = store::events_since(&self.registry, id, 0)
            .await
            .unwrap()
            .into_iter()
            .map(|e| format!("{}:{}", e.step, e.status))
            .collect();
        (run, steps)
    }

    /// Every stored run error and event message, and every audit change.
    async fn trail(&self) -> String {
        let mut out = String::new();
        let runs: Vec<ProvisioningRun> = ProvisioningRun::objects().fetch(&self.registry).await.unwrap();
        for run in runs {
            out.push_str(&format!("{:?}\n", run.error));
            let id = run.id.get().copied().unwrap_or_default();
            for e in store::events_since(&self.registry, id, 0).await.unwrap() {
                out.push_str(&format!("{} {} {:?}\n", e.step, e.status, e.message));
            }
        }
        let audit = rustango::audit::list(&self.registry, &Default::default(), 500, 0)
            .await
            .unwrap();
        out.push_str(&format!("{:?}", audit.iter().map(|e| &e.changes).collect::<Vec<_>>()));
        out
    }

    /// A tenant made on server `main`, with its resolved URL.
    async fn server_tenant(&self, slug: &str) -> String {
        self.provision(&ProvisionRequest::on_server(slug, "main")).await;
        let org = self.org(slug).await.expect("org");
        self.resolved_url(&org).await
    }
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
async fn scratch(guards: &mut Vec<Box<dyn std::any::Any>>, var: &str) -> String {
    let db = scratch_db::ScratchDb::create(&std::env::var(var).unwrap(), "rustango_stenant").await;
    let url = db.url().to_owned();
    guards.push(Box::new(db));
    url
}

/// One tenant migration creating table `st_notes`.
fn write_migrations(dir: &Path) {
    let mig = rustango::migrate::Migration {
        name: "0001_notes".to_owned(),
        created_at: "2026-10-25T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: rustango::migrate::MigrationScope::Tenant,
        replaces: Vec::new(),
        merges: Vec::new(),
        snapshot: serde_json::from_value(serde_json::json!({
            "tables": [{
                "name": "st_notes", "model": "T",
                "fields": [{"name": "id", "column": "id", "ty": "i64",
                            "nullable": false, "primary_key": true}]
            }]
        }))
        .unwrap(),
        forward: vec![rustango::migrate::Operation::Schema(
            rustango::migrate::SchemaChange::CreateTable("st_notes".into()),
        )],
    };
    rustango::migrate::file::write(&dir.join("0001_notes.json"), &mig).unwrap();
}

// ----------------------------------------------------------- scenarios

/// The server admin drops the database and the login, then the record
/// and the row; the CLI prints every step and the run records them.
async fn purge_drops_a_server_tenant_as_the_admin(pool: &Pool) {
    let _g = env_lock().lock().await;
    prepare();
    let env = Env::new(pool).await;
    let a = slug("purge");
    env.clean(&[&a]).await;
    env.add_server_as("main", None).await;
    let url = env.server_tenant(&a).await;

    let out = env.purge(&a).await.expect("purge");
    for step in ["check", "deactivate", "drop_database", "delete_record", "remove_org"] {
        assert!(out.contains(&format!("  {step} ... ok")), "{step}: {out}");
    }
    assert!(env.org(&a).await.is_none(), "the row survived");
    assert!(env.records(&a).await.is_empty(), "the record survived");
    assert!(!env.database_exists(&a).await, "the database survived");
    assert!(env.nothing_left(&a).await, "the login survived");
    let (run, steps) = env.last_run(&a, RunKind::Purge).await;
    assert_eq!(RunState::parse(&run.state), RunState::Succeeded, "{steps:?}");
    let login_step = if env.server_backend() { "drop_login:ok" } else { "drop_login:skipped" };
    assert!(steps.iter().any(|s| s == login_step), "{steps:?}");
    if let Some(pw) = password_of(&url) {
        assert!(!out.contains(&pw), "{out}");
        assert!(!env.trail().await.contains(&pw));
    }
}

/// The tenant's own sessions, opened by another pod's pools, are ended.
async fn purge_ends_the_tenants_sessions_on_other_pods(pool: &Pool) {
    let _g = env_lock().lock().await;
    prepare();
    let env = Env::new(pool).await;
    let a = slug("pods");
    env.clean(&[&a]).await;
    env.add_server_as("main", None).await;
    env.server_tenant(&a).await;
    let org = env.org(&a).await.unwrap();

    typed!(env.registry, other_pod => {
        let held = other_pod.database_acquire(&org).await.expect("tenant session");
        let out = env.purge(&a).await.expect("purge with a session open");
        assert!(out.contains("  drop_database ... ok"), "{out}");
        drop(held);
    });
    assert!(!env.database_exists(&a).await, "the database survived");
}

/// A tenant whose URL was moved off the recorded database is refused
/// before anything changes, by purge and by rotation.
async fn a_moved_tenant_is_refused_before_anything_changes(pool: &Pool) {
    let _g = env_lock().lock().await;
    prepare();
    let env = Env::new(pool).await;
    let (a, b) = (slug("moved"), slug("target"));
    env.clean(&[&a, &b]).await;
    env.add_server_as("main", None).await;
    env.server_tenant(&a).await;
    // Another database the tenant now points at, as after migrate-tenant-storage.
    let admin = env.admin().await;
    let other = admin
        .create_tenant_database(&db_name(&b), &Login::new(tenant_login_name(&db_name(&b))), &LoginPassword::generate())
        .await
        .expect("other database");
    let moved = rustango::tenancy::seal(other.url()).expect("seal");
    admin.close().await;
    let org = env.org(&a).await.unwrap();
    Org::objects()
        .where_(Org::id.eq(org.id.get().copied().unwrap()))
        .update()
        .set("database_url", Some(moved))
        .execute_pool(&env.registry)
        .await
        .unwrap();

    let err = env.purge(&a).await.expect_err("moved");
    assert!(err.to_string().contains("no longer names the database"), "{err}");
    if env.server_backend() {
        let err = env.run(&["rotate-tenant-password", &a]).await.expect_err("moved");
        assert!(err.to_string().contains("no longer names the database"), "{err}");
    }
    let org = env.org(&a).await.expect("the row stays");
    assert!(org.active, "a refused purge deactivated it");
    assert_eq!(env.records(&a).await.len(), 1, "the record stays");
    assert!(env.database_exists(&a).await && env.database_exists(&b).await);
    let (_, steps) = env.last_run(&a, RunKind::Purge).await;
    assert_eq!(steps, ["check:started", "check:failed"], "{steps:?}");
    env.clean(&[&a, &b]).await;
}

/// A database the operator brought has no record, so a purge never drops
/// it through a server: off Postgres it stays, as before.
async fn a_brought_database_is_left_off_postgres(pool: &Pool) {
    let _g = env_lock().lock().await;
    prepare();
    let env = Env::new(pool).await;
    if env.backend() == BackendKind::Postgres {
        return;
    }
    let a = slug("brought");
    env.clean(&[&a]).await;
    let admin = env.admin().await;
    let made = admin
        .create_tenant_database(&db_name(&a), &Login::new(tenant_login_name(&db_name(&a))), &LoginPassword::generate())
        .await
        .expect("brought database");
    admin.close().await;
    env.provision(&ProvisionRequest::database(&a, made.url())).await;
    assert!(env.records(&a).await.is_empty());

    let out = env.purge(&a).await.expect("purge");
    assert!(out.contains("drop_database ... skipped"), "{out}");
    assert!(env.org(&a).await.is_none());
    assert!(env.database_exists(&a).await, "a brought database was dropped");
    env.clean(&[&a]).await;
}

/// A new password is set, sealed and stored; the tenant connects with it
/// and the old one stops working. Neither appears anywhere.
async fn rotation_gives_the_login_a_new_password(pool: &Pool) {
    let _g = env_lock().lock().await;
    prepare();
    let env = Env::new(pool).await;
    let a = slug("rot");
    env.clean(&[&a]).await;
    env.add_server_as("main", None).await;
    let old_url = env.server_tenant(&a).await;

    let rotated = env.run(&["rotate-tenant-password", &a]).await;
    if !env.server_backend() {
        let err = rotated.expect_err("SQLite has no password");
        assert!(err.to_string().contains("no password"), "{err}");
        return;
    }
    let out = rotated.expect("rotate");
    for step in ["check", "alter_password", "store_password", "invalidate_pools"] {
        assert!(out.contains(&format!("  {step} ... ok")), "{step}: {out}");
    }
    let org = env.org(&a).await.unwrap();
    assert!(org.database_url.as_deref().unwrap().starts_with("enc:v1:"));
    let new_url = env.resolved_url(&org).await;
    assert_ne!(new_url, old_url);
    Pool::connect(&new_url).await.expect("new password works").close().await;
    assert!(Pool::connect(&old_url).await.is_err(), "the old password still works");
    let (run, steps) = env.last_run(&a, RunKind::RotatePassword).await;
    assert_eq!(RunState::parse(&run.state), RunState::Succeeded, "{steps:?}");
    let trail = env.trail().await;
    for pw in [password_of(&old_url).unwrap(), password_of(&new_url).unwrap()] {
        assert!(!out.contains(&pw) && !trail.contains(&pw), "a password leaked");
    }
    env.clean(&[&a]).await;
}

/// A database rustango did not make keeps its login's password.
async fn rotation_refuses_a_brought_database(pool: &Pool) {
    let _g = env_lock().lock().await;
    prepare();
    let env = Env::new(pool).await;
    if !env.server_backend() {
        return;
    }
    let a = slug("rotb");
    env.clean(&[&a]).await;
    let admin = env.admin().await;
    let made = admin
        .create_tenant_database(&db_name(&a), &Login::new(tenant_login_name(&db_name(&a))), &LoginPassword::generate())
        .await
        .expect("brought database");
    admin.close().await;
    env.provision(&ProvisionRequest::database(&a, made.url())).await;
    let before = env.org(&a).await.unwrap().database_url;

    let err = env.run(&["rotate-tenant-password", &a]).await.expect_err("brought");
    assert!(err.to_string().contains("not made by rustango"), "{err}");
    assert_eq!(env.org(&a).await.unwrap().database_url, before);
    env.clean(&[&a]).await;
}

/// A PG 16+ admin that did not create the login is refused with what to grant.
async fn a_replaced_admin_is_told_what_it_lacks(pool: &Pool) {
    let _g = env_lock().lock().await;
    prepare();
    let env = Env::new(pool).await;
    if env.backend() != BackendKind::Postgres {
        return;
    }
    let version: Vec<(i32,)> = rustango::sql::raw_query_pool(
        "SELECT current_setting('server_version_num')::int",
        Vec::new(),
        &env.registry,
    )
    .await
    .unwrap();
    if version[0].0 < 160_000 {
        return;
    }
    let a = slug("adm");
    env.clean(&[&a]).await;
    env.add_server_as("main", None).await;
    env.server_tenant(&a).await;
    let second = format!("st{}second", std::process::id());
    exec_text(&env.registry, &format!("DROP ROLE IF EXISTS \"{second}\"")).await;
    exec_text(
        &env.registry,
        &format!("CREATE ROLE \"{second}\" LOGIN CREATEDB CREATEROLE PASSWORD 'secondpw'"),
    )
    .await;
    env.add_server_as("second", Some((&second, "secondpw"))).await;
    TenantDatabase::objects()
        .where_(TenantDatabase::org_slug.eq(a.clone()))
        .update()
        .set("server_name", "second")
        .execute_pool(&env.registry)
        .await
        .unwrap();

    let err = env.run(&["rotate-tenant-password", &a]).await.expect_err("no ADMIN OPTION");
    assert!(err.to_string().contains("ADMIN OPTION"), "{err}");
    let err = env.purge(&a).await.expect_err("does not own it");
    assert!(err.to_string().contains("does not own database"), "{err}");
    assert!(env.org(&a).await.unwrap().active);

    TenantDatabase::objects()
        .where_(TenantDatabase::org_slug.eq(a.clone()))
        .update()
        .set("server_name", "main")
        .execute_pool(&env.registry)
        .await
        .unwrap();
    env.clean(&[&a]).await;
    exec_text(&env.registry, &format!("DROP ROLE \"{second}\"")).await;
}

// ------------------------------------------------------------- console

struct Console {
    app: axum::Router,
    cookie: String,
}

impl Console {
    async fn boot(env: &Env) -> Self {
        use rustango::tenancy::operator_console::{router_with_provisioning, SessionSecret};
        let mut op = rustango::tenancy::Operator {
            id: rustango::sql::Auto::default(),
            username: "op".into(),
            password_hash: rustango::tenancy::password::hash("letmein").unwrap(),
            active: true,
            created_at: chrono::Utc::now(),
            password_changed_at: None,
            sessions_revoked_at: None,
        };
        op.insert_pool(&env.registry).await.expect("seed operator");
        let dir = env.dir("good");
        let app = typed!(env.registry, p => {
            let pools = std::sync::Arc::new(p);
            let provisioner = provision::Provisioner::new(pools.clone(), env.url.clone(), dir).erased();
            router_with_provisioning(env.registry.clone(), pools, provisioner, SessionSecret::from_env_or_random())
        });
        let mut c = Self {
            app,
            cookie: String::new(),
        };
        let login = c.post("/login", "username=op&password=letmein").await;
        c.cookie = login.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        c
    }

    async fn get(&self, uri: &str) -> String {
        use tower::ServiceExt as _;
        let req = axum::http::Request::builder()
            .uri(uri)
            .header("cookie", &self.cookie)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    async fn post(&self, uri: &str, form: &str) -> axum::response::Response {
        use tower::ServiceExt as _;
        let req = axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("cookie", "rustango_csrf=t")
            .header("x-csrf-token", "t")
            .header("cookie", &self.cookie)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(axum::body::Body::from(form.to_owned()))
            .unwrap();
        self.app.clone().oneshot(req).await.unwrap()
    }

    /// Post `form` to `uri`, follow the redirect to a run, wait for it to end.
    async fn run(&self, env: &Env, uri: &str, form: &str) -> (ProvisioningRun, String) {
        let resp = self.post(uri, form).await;
        assert_eq!(resp.status().as_u16(), 303, "{uri}");
        let to = resp.headers()["location"].to_str().unwrap().to_owned();
        let id: i64 = to.rsplit('/').next().unwrap().parse().expect("a run id");
        for _ in 0..600 {
            let run = store::run_by_id(&env.registry, id).await.unwrap().expect("run");
            if RunState::parse(&run.state).is_terminal() {
                return (run, self.get(&to).await);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("run {id} never finished");
    }
}

/// The tenant page offers rotation for a server-made login; rotation and
/// purge each run on the run page, step by step.
async fn the_console_rotates_and_purges_through_the_run_page(pool: &Pool) {
    let _g = env_lock().lock().await;
    prepare();
    let env = Env::new(pool).await;
    let a = slug("con");
    env.clean(&[&a]).await;
    env.add_server_as("main", None).await;
    env.server_tenant(&a).await;
    let c = Console::boot(&env).await;

    let page = c.get(&format!("/orgs/{a}/edit")).await;
    assert_eq!(page.contains("rotate-password"), env.server_backend(), "{page}");
    if env.server_backend() {
        let (run, page) = c.run(&env, &format!("/orgs/{a}/rotate-password"), "").await;
        assert_eq!(RunState::parse(&run.state), RunState::Succeeded, "{page}");
        assert!(page.contains("Rotating the database password of"), "{page}");
        assert!(page.contains("store_password"), "{page}");
    }
    let (run, page) = c
        .run(&env, &format!("/orgs/{a}/purge"), &format!("confirm={a}&purge_database=on"))
        .await;
    assert_eq!(RunState::parse(&run.state), RunState::Succeeded, "{page}");
    assert!(page.contains("Purging") && page.contains("drop_database"), "{page}");
    assert!(env.org(&a).await.is_none());
    assert!(!env.database_exists(&a).await);
}

tri_dialect_test! {
    setup: setup,
    sqlite: file,
    scenarios: [
        purge_drops_a_server_tenant_as_the_admin,
        purge_ends_the_tenants_sessions_on_other_pods,
        a_moved_tenant_is_refused_before_anything_changes,
        a_brought_database_is_left_off_postgres,
        rotation_gives_the_login_a_new_password,
        rotation_refuses_a_brought_database,
        a_replaced_admin_is_told_what_it_lacks,
        the_console_rotates_and_purges_through_the_run_page,
    ],
}
