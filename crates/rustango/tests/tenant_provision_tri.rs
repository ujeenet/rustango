//! Creating a database-mode tenant on a saved server, on every registry
//! backend: the database, its own login, the sealed URL, the record, and
//! a full rollback when a later step fails.
//!
//! The server arms create databases and logins, so `DATABASE_URL` and
//! `MYSQL_TEST_URL` must be admin logins (MySQL: root).

#![cfg(all(feature = "tenancy", feature = "testkit", feature = "sqlite"))]

use std::path::{Path, PathBuf};
use std::time::Duration;

use rustango::core::Column as _;
use rustango::sql::{DatabaseName, FetcherPool as _, Login, LoginPassword, Pool};
use rustango::tenancy::manage::api::{create_tenant, CreateTenantOpts};
use rustango::tenancy::provision::{self, ProvisionRequest};
use rustango::tenancy::server_admin::{tenant_login_name, ServerAdmin};
use rustango::tenancy::{
    db_server, BackendKind, DbServerSpec, Org, StorageMode, TenancyError, TenantDatabase,
    TenantPools, TlsMode,
};
use rustango::tri_dialect_test;

#[cfg(any(feature = "postgres", feature = "mysql"))]
#[path = "support/scratch_db.rs"]
mod scratch_db;

const KEY: &str = "tenant-provision-tri-key-0123456789abcdef";
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

/// A tenant slug unique to this process and scenario: tenant databases
/// and logins live on the shared server.
fn slug(tag: &str) -> String {
    format!("tp{}-{tag}", std::process::id())
}

fn db_name(slug: &str) -> DatabaseName {
    DatabaseName::new(slug).unwrap()
}

/// `url` pointed at database `db`.
fn on_db(url: &str, db: &str) -> String {
    let (base, rest) = url.rsplit_once('/').unwrap();
    let query = rest.split_once('?').map(|(_, q)| format!("?{q}"));
    format!("{base}/{db}{}", query.unwrap_or_default())
}

/// The password in `url`, if any.
fn password_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let userinfo = rest.rsplit_once('@')?.0;
    userinfo.split_once(':').map(|(_, p)| p.to_owned())
}

/// Run `sql` as text, not prepared: MySQL cannot prepare `CREATE TRIGGER`.
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
        write_migrations(&env.dir("good"), &["0001_notes"]);
        // Two files creating one table: the second fails.
        write_migrations(&env.dir("broken"), &["0001_notes", "0002_again"]);
        // Migrates fine, but seeds a user the first admin then collides with.
        write_migrations(&env.dir("taken"), &["0001_notes"]);
        write_seed_user(&env.dir("taken"), "taken");
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

    /// Save server `name` as the live admin, or a directory on SQLite.
    async fn add_server(&self, name: &str) {
        self.add_server_as(name, None).await;
    }

    /// [`Self::add_server`] logging in as `user` / `password` instead.
    async fn add_server_as(&self, name: &str, login: Option<(&str, &str)>) {
        let base = DbServerSpec {
            name: name.into(),
            backend: self.backend(),
            active: true,
            ..DbServerSpec::default()
        };
        if !self.server_backend() {
            let dir = match login {
                Some(_) => self.tmp.path().join("no-such-dir"),
                None => self.dir("tenants"),
            };
            let spec = DbServerSpec {
                sqlite_dir: Some(dir.display().to_string()),
                ..base
            };
            db_server::add(&self.registry, &spec, None)
                .await
                .expect("add server");
            return;
        }
        let rest = self.url.split_once("://").unwrap().1;
        let (userinfo, rest) = rest.rsplit_once('@').unwrap();
        let (admin, admin_pw) = userinfo.split_once(':').unwrap_or((userinfo, ""));
        let (user, pw) = login.unwrap_or((admin, admin_pw));
        let authority = rest.split(['/', '?']).next().unwrap();
        let (host, port) = authority.rsplit_once(':').unwrap();
        let spec = DbServerSpec {
            host: Some(host.into()),
            port: Some(port.parse().unwrap()),
            admin_user: Some(user.into()),
            // The CI servers have no TLS.
            tls: Some(TlsMode::Disable),
            ..base
        };
        let pw = LoginPassword::new(pw).unwrap();
        db_server::add(&self.registry, &spec, Some(&pw))
            .await
            .expect("add server");
    }

    /// An admin on the live server, or the tenants directory.
    async fn admin(&self) -> ServerAdmin {
        if self.server_backend() {
            ServerAdmin::connect(&self.url, T).await.expect("admin")
        } else {
            ServerAdmin::sqlite_dir(self.dir("tenants"))
        }
    }

    /// Drop a tenant database left by an earlier run of this process id.
    /// Retries while a dropped pool's sessions are still closing.
    async fn clean(&self, slugs: &[&str]) {
        let admin = self.admin().await;
        for s in slugs {
            let name = db_name(s);
            let login = Login::new(tenant_login_name(&name));
            for attempt in 0.. {
                match admin.drop_tenant_database(&name, &login).await {
                    Ok(()) => break,
                    Err(e) if attempt < 50 => {
                        let _ = e;
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
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
            admin
                .drop_tenant_database(&name, &login)
                .await
                .expect("drop again");
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

    async fn provision(&self, dir: &Path, request: &ProvisionRequest) -> Result<(), TenancyError> {
        typed!(self.registry, p => provision::provision_tenant_recorded(
            &p, &self.url, dir, request, None, None, None,
        )
        .await)
        .map(drop)
    }

    async fn run(&self, dir: &Path, args: &[&str]) -> Result<String, TenancyError> {
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        let mut out = Vec::new();
        typed!(self.registry, p => rustango::tenancy::manage::run_with_writer(&p, &self.url, dir, args, &mut out).await)?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// Every stored event message of every run, and every audit change.
    async fn trail(&self) -> String {
        use rustango::tenancy::provision_store::{events_since, ProvisioningRun};
        let mut out = String::new();
        let runs: Vec<ProvisioningRun> = ProvisioningRun::objects()
            .fetch(&self.registry)
            .await
            .unwrap();
        for run in runs {
            out.push_str(&format!("{:?} {:?}\n", run.error, run.database_url));
            let id = run.id.get().copied().unwrap_or_default();
            for e in events_since(&self.registry, id, 0).await.unwrap() {
                out.push_str(&format!("{} {} {:?}\n", e.step, e.status, e.message));
            }
        }
        let audit = rustango::audit::list(&self.registry, &Default::default(), 500, 0)
            .await
            .unwrap();
        out.push_str(&format!(
            "{:?}",
            audit.iter().map(|e| &e.changes).collect::<Vec<_>>()
        ));
        out
    }

    /// The step/status pairs of `slug`'s latest run.
    async fn steps(&self, slug: &str) -> Vec<String> {
        use rustango::tenancy::provision_store::{events_since, ProvisioningRun};
        let run = ProvisioningRun::objects()
            .where_(ProvisioningRun::slug.eq(slug.to_owned()))
            .order_by(&[("id", true)])
            .first(&self.registry)
            .await
            .unwrap()
            .expect("a run");
        let id = run.id.get().copied().unwrap_or_default();
        events_since(&self.registry, id, 0)
            .await
            .unwrap()
            .into_iter()
            .map(|e| format!("{}:{}", e.step, e.status))
            .collect()
    }

    /// Refuse every new `rustango_orgs` row, or allow them again.
    async fn refuse_org_inserts(&self, on: bool) {
        let sql: &[&str] = match (self.backend(), on) {
            (BackendKind::Postgres, true) => &[
                "CREATE FUNCTION tp_refuse() RETURNS trigger LANGUAGE plpgsql AS \
                 $$ BEGIN RAISE EXCEPTION 'injected insert failure'; END $$",
                "CREATE TRIGGER tp_refuse BEFORE INSERT ON rustango_orgs \
                 FOR EACH ROW EXECUTE FUNCTION tp_refuse()",
            ],
            (BackendKind::Postgres, false) => &["DROP TRIGGER tp_refuse ON rustango_orgs"],
            (BackendKind::MySql, true) => {
                &["CREATE TRIGGER tp_refuse BEFORE INSERT ON rustango_orgs \
                 FOR EACH ROW SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'injected insert failure'"]
            }
            (BackendKind::Sqlite, true) => {
                &["CREATE TRIGGER tp_refuse BEFORE INSERT ON rustango_orgs \
                 BEGIN SELECT RAISE(ABORT, 'injected insert failure'); END"]
            }
            (_, false) => &["DROP TRIGGER tp_refuse"],
        };
        for s in sql {
            exec_text(&self.registry, s).await;
        }
    }
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
async fn scratch(guards: &mut Vec<Box<dyn std::any::Any>>, var: &str) -> String {
    let db = scratch_db::ScratchDb::create(&std::env::var(var).unwrap(), "rustango_tprov").await;
    let url = db.url().to_owned();
    guards.push(Box::new(db));
    url
}

/// Tenant migrations each creating table `tp_notes`.
fn write_migrations(dir: &Path, names: &[&str]) {
    for name in names {
        write_table_migration(dir, name, "tp_notes");
    }
}

/// A tenant migration `name` creating `table`.
fn write_table_migration(dir: &Path, name: &str, table: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let mig = rustango::migrate::Migration {
        name: name.to_owned(),
        created_at: "2026-10-25T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: rustango::migrate::MigrationScope::Tenant,
        replaces: Vec::new(),
        merges: Vec::new(),
        snapshot: serde_json::from_value(serde_json::json!({
            "tables": [{
                "name": table, "model": "T",
                "fields": [{"name": "id", "column": "id", "ty": "i64",
                            "nullable": false, "primary_key": true}]
            }]
        }))
        .unwrap(),
        forward: vec![rustango::migrate::Operation::Schema(
            rustango::migrate::SchemaChange::CreateTable(table.into()),
        )],
    };
    rustango::migrate::file::write(&dir.join(format!("{name}.json")), &mig).unwrap();
}

/// A tenant data migration inserting user `username`. Runs after the
/// system chain, which makes `rustango_users`.
fn write_seed_user(dir: &Path, username: &str) {
    let mig = rustango::migrate::Migration {
        name: "0002_seed_user".to_owned(),
        created_at: "2026-10-25T00:00:01Z".into(),
        prev: None,
        atomic: true,
        scope: rustango::migrate::MigrationScope::Tenant,
        replaces: Vec::new(),
        merges: Vec::new(),
        snapshot: serde_json::from_value(serde_json::json!({
            "tables": [{
                "name": "tp_notes", "model": "T",
                "fields": [{"name": "id", "column": "id", "ty": "i64",
                            "nullable": false, "primary_key": true}]
            }]
        }))
        .unwrap(),
        forward: vec![rustango::migrate::Operation::Data(rustango::migrate::DataOp {
            sql: format!(
                "INSERT INTO rustango_users (username, password_hash, is_superuser, active, \
                 created_at, data) VALUES ('{username}', 'x', false, true, CURRENT_TIMESTAMP, '{{}}')"
            ),
            reverse_sql: None,
            reversible: false,
        })],
    };
    rustango::migrate::file::write(&dir.join("0002_seed_user.json"), &mig).unwrap();
}

/// The tenant is active, its URL sealed, its record written, and its own
/// login built its schema. Returns the resolved URL.
async fn assert_provisioned(env: &Env, slug: &str) -> String {
    let org = env.org(slug).await.expect("org row");
    assert!(org.active, "{slug} inactive");
    let stored = org.database_url.clone().expect("a URL");
    let url = env.resolved_url(&org).await;
    let records = env.records(slug).await;
    assert_eq!(records.len(), 1, "{records:?}");
    let r = &records[0];
    assert_eq!(
        (r.server_name.as_str(), r.database_name.as_str()),
        ("main", slug)
    );
    assert_eq!(r.org_id, org.id.get().copied());
    if env.server_backend() {
        assert!(
            stored.starts_with("enc:v1:"),
            "stored in plain: {}",
            &stored[..10]
        );
        let login = tenant_login_name(&db_name(slug));
        assert_eq!(r.login_name.as_deref(), Some(login.as_str()));
        assert!(
            url.contains(&format!("://{login}:")),
            "connects as its own login"
        );
    } else {
        assert_eq!(r.login_name, None);
        assert!(stored.contains(&format!("{slug}.db")), "{stored}");
    }
    let tenant = Pool::connect(&url).await.expect("tenant connects");
    let rows: Vec<(i64,)> =
        rustango::sql::raw_query_pool("SELECT COUNT(*) FROM tp_notes", vec![], &tenant)
            .await
            .expect("its migration ran");
    assert_eq!(rows[0].0, 0);
    tenant.close().await;
    url
}

// ------------------------------------------------------------- console

struct Console {
    app: axum::Router,
    cookie: String,
}

impl Console {
    async fn boot(env: &Env, migrations: &Path) -> Self {
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
        let dir = migrations.to_owned();
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
        body(self.app.clone().oneshot(req).await.unwrap()).await
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

    /// Submit the create form for `slug` on server `main`.
    async fn create(&self, slug: &str) -> axum::response::Response {
        self.post(
            "/orgs/new",
            &format!("slug={slug}&storage_mode=database&database_source=server&server=main"),
        )
        .await
    }
}

async fn body(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

// ----------------------------------------------------------- scenarios

/// The console creates the database and its login, migrates as that
/// login and activates; the password appears nowhere.
async fn the_console_creates_a_tenant_on_a_saved_server(pool: &Pool) {
    let _g = env_lock().lock().await;
    std::env::set_var("RUSTANGO_SECRET_KEY", KEY);
    let env = Env::new(pool).await;
    let a = slug("con");
    env.clean(&[&a]).await;
    env.add_server("main").await;
    let c = Console::boot(&env, &env.dir("good")).await;

    let page = c.get("/orgs/new").await;
    assert!(
        page.contains("Create a new database on a saved server"),
        "{page}"
    );
    assert!(page.contains(r#"<option value="main""#), "{page}");
    assert!(
        page.contains(r#"type="password" id="database_url""#),
        "{page}"
    );
    // A saved server is the default; the shared-login path says so.
    let server_radio = page
        .split(r#"value="server""#)
        .nth(1)
        .unwrap()
        .split('>')
        .next()
        .unwrap();
    assert!(server_radio.contains("checked"), "{page}");
    assert!(page.contains("not isolated"), "{page}");

    let probe = body(
        c.post(
            "/orgs/test-connection",
            "storage_mode=database&database_source=server&server=main",
        )
        .await,
    )
    .await;
    let json: serde_json::Value = serde_json::from_str(&probe).expect("json");
    assert_eq!(json["status"], "ok", "{probe}");
    assert_eq!(json["can_create_database"], true, "{probe}");

    // A refused URL comes back as an error, never as a pre-filled field.
    let foreign = if env.backend() == BackendKind::Postgres {
        "mysql"
    } else {
        "postgres"
    };
    let leak = "Leak-q7Zx-pw";
    let refused = body(
        c.post(
            "/orgs/new",
            &format!(
                "slug={a}&storage_mode=database&database_source=url&database_url={foreign}%3A%2F%2Fu%3A{leak}%40h%2Fx"
            ),
        )
        .await,
    )
    .await;
    assert!(refused.contains("alert-error"), "{refused}");
    assert!(!refused.contains(leak), "the typed URL was sent back");

    let resp = c.create(&a).await;
    assert_eq!(resp.status().as_u16(), 303, "{}", body(resp).await);
    let run_page = resp.headers()["location"].to_str().unwrap().to_owned();
    let url = assert_provisioned(&env, &a).await;
    let steps = env.steps(&a).await;
    let mut want = vec!["create_database:ok"];
    if env.server_backend() {
        want.extend(["create_login:ok", "grant:ok"]);
    } else {
        want.extend(["create_login:skipped", "grant:skipped"]);
    }
    for s in want {
        assert!(steps.iter().any(|x| x == s), "{s} missing: {steps:?}");
    }

    // The dry check now finds the slug taken.
    let probe = body(
        c.post(
            "/orgs/test-connection",
            &format!("slug={a}&storage_mode=database&database_source=server&server=main"),
        )
        .await,
    )
    .await;
    assert!(
        probe.contains(r#""status":"bad""#) && probe.contains("already exists"),
        "{probe}"
    );

    let in_use = db_server::get(&env.registry, "main")
        .await
        .unwrap()
        .used_by(&env.registry, &rustango::tenancy::TenantSecrets::standard())
        .await
        .unwrap();
    assert!(in_use.contains(&a), "{in_use:?}");

    if let Some(pw) = password_of(&url) {
        let shown = format!(
            "{}{}{}",
            c.get(&format!("/orgs/{run_page}")).await,
            c.get(&format!("/orgs/{a}")).await,
            env.trail().await
        );
        assert!(!shown.contains(&pw), "the tenant password was shown");
    }
    // Its tenant pool holds sessions in the database.
    drop(c);
    env.clean(&[&a]).await;
}

/// `create-tenant --server`: two tenants, each login shut out of the
/// other's database.
async fn the_cli_creates_tenants_on_a_saved_server(pool: &Pool) {
    let _g = env_lock().lock().await;
    std::env::set_var("RUSTANGO_SECRET_KEY", KEY);
    let env = Env::new(pool).await;
    let (a, b) = (slug("cli-a"), slug("cli-b"));
    env.clean(&[&a, &b]).await;
    env.add_server("main").await;
    let good = env.dir("good");

    let err = env
        .run(
            &good,
            &[
                "create-tenant",
                &a,
                "--server",
                "main",
                "--database-url",
                "x",
            ],
        )
        .await
        .expect_err("both sources");
    assert!(err.to_string().contains("not both"), "{err}");

    let mut out = String::new();
    for s in [&a, &b] {
        out.push_str(
            &env.run(
                &good,
                &["create-tenant", s, "--mode", "database", "--server", "main"],
            )
            .await
            .expect("create-tenant"),
        );
    }
    assert!(out.contains(&format!("created database `{a}`")), "{out}");
    let url_a = assert_provisioned(&env, &a).await;
    let url_b = assert_provisioned(&env, &b).await;
    if let Some(pw) = password_of(&url_a) {
        assert!(!out.contains(&pw), "{out}");
        assert!(!env.trail().await.contains(&pw));
        // A's login cannot open B's database, and B's still can.
        assert!(
            Pool::connect(&on_db(&url_a, &b)).await.is_err(),
            "a reached b"
        );
        Pool::connect(&url_b)
            .await
            .expect("b connects")
            .close()
            .await;
    }
    env.clean(&[&a, &b]).await;
}

/// A migration failure drops the new database, login, record and row;
/// a retry then starts clean.
async fn a_failed_migration_drops_everything_it_made(pool: &Pool) {
    let _g = env_lock().lock().await;
    std::env::set_var("RUSTANGO_SECRET_KEY", KEY);
    let env = Env::new(pool).await;
    let a = slug("mig");
    env.clean(&[&a]).await;
    env.add_server("main").await;
    let c = Console::boot(&env, &env.dir("broken")).await;

    let resp = c.create(&a).await;
    assert_eq!(resp.status().as_u16(), 200, "the form comes back");
    let page = body(resp).await;
    assert!(
        page.contains("migrations failed") && page.contains("dropped"),
        "{page}"
    );
    assert!(env.org(&a).await.is_none(), "the row survived");
    assert!(env.records(&a).await.is_empty(), "the record survived");
    assert!(!env.database_exists(&a).await, "the database survived");
    assert!(env.nothing_left(&a).await, "a login or database survived");
    let steps = env.steps(&a).await;
    assert!(steps.iter().any(|s| s == "rollback:ok"), "{steps:?}");

    // Nothing left in the way of a retry.
    let req = ProvisionRequest::on_server(&a, "main");
    env.provision(&env.dir("good"), &req).await.expect("retry");
    assert_provisioned(&env, &a).await;
    env.clean(&[&a]).await;
}

/// A first admin that cannot be created rolls the run back like any other
/// step; a good one is a superuser and nothing stores its handover.
async fn a_failed_first_admin_drops_everything_it_made(pool: &Pool) {
    let _g = env_lock().lock().await;
    std::env::set_var("RUSTANGO_SECRET_KEY", KEY);
    let env = Env::new(pool).await;
    let a = slug("adm");
    env.clean(&[&a]).await;
    env.add_server("main").await;
    let admin =
        |name: &str| rustango::tenancy::FirstAdmin::new("ann@acme.test", Some(name)).unwrap();

    let req = ProvisionRequest {
        first_admin: Some(admin("taken")),
        ..ProvisionRequest::on_server(&a, "main")
    };
    let err = env
        .provision(&env.dir("taken"), &req)
        .await
        .expect_err("taken");
    assert!(err.to_string().contains("taken"), "{err}");
    assert!(env.org(&a).await.is_none(), "the row survived");
    assert!(env.records(&a).await.is_empty(), "the record survived");
    assert!(env.nothing_left(&a).await, "a login or database survived");
    let steps = env.steps(&a).await;
    assert!(
        steps.iter().any(|s| s == "create_admin:failed"),
        "{steps:?}"
    );
    assert!(steps.iter().any(|s| s == "rollback:ok"), "{steps:?}");
    assert!(!steps.iter().any(|s| s == "activate:ok"), "{steps:?}");

    let req = ProvisionRequest {
        first_admin: Some(admin("ann")),
        ..ProvisionRequest::on_server(&a, "main")
    };
    if let Err(e) = env.provision(&env.dir("good"), &req).await {
        panic!("retry: {e}: {:?}", env.steps(&a).await);
    }
    let url = assert_provisioned(&env, &a).await;
    let tenant = Pool::connect(&url).await.expect("tenant");
    let users: Vec<rustango::tenancy::User> = rustango::tenancy::User::objects()
        .where_(rustango::tenancy::User::username.eq("ann".to_owned()))
        .fetch(&tenant)
        .await
        .unwrap();
    assert!(users.len() == 1 && users[0].is_superuser, "{users:?}");
    assert!(
        users[0].password_changed_at.is_none(),
        "a set-password link needs it unset"
    );
    tenant.close().await;
    let audit = rustango::audit::list(&env.registry, &Default::default(), 500, 0)
        .await
        .unwrap();
    assert!(
        audit
            .iter()
            .any(|e| e.source == "provision:first_admin" && e.entity_table == "rustango_users"),
        "no audit row"
    );
    env.clean(&[&a]).await;
}

/// A failed insert of the tenant row drops the new database and login.
async fn a_failed_insert_drops_what_it_made(pool: &Pool) {
    let _g = env_lock().lock().await;
    std::env::set_var("RUSTANGO_SECRET_KEY", KEY);
    let env = Env::new(pool).await;
    let a = slug("ins");
    env.clean(&[&a]).await;
    env.add_server("main").await;
    let req = ProvisionRequest::on_server(&a, "main");

    env.refuse_org_inserts(true).await;
    let err = env
        .provision(&env.dir("good"), &req)
        .await
        .expect_err("insert refused");
    env.refuse_org_inserts(false).await;
    assert!(env.org(&a).await.is_none(), "{err}");
    assert!(env.records(&a).await.is_empty(), "the record survived");
    assert!(env.nothing_left(&a).await, "a login or database survived");
    let steps = env.steps(&a).await;
    assert!(steps.iter().any(|s| s == "rollback:ok"), "{steps:?}");

    env.provision(&env.dir("good"), &req).await.expect("retry");
    assert_provisioned(&env, &a).await;
    env.clean(&[&a]).await;
}

/// A database already named as the slug is refused and left alone.
async fn an_existing_database_is_refused_and_kept(pool: &Pool) {
    let _g = env_lock().lock().await;
    std::env::set_var("RUSTANGO_SECRET_KEY", KEY);
    let env = Env::new(pool).await;
    let a = slug("old");
    env.clean(&[&a]).await;
    env.add_server("main").await;
    // Someone else's database, under another login.
    let admin = env.admin().await;
    let name = db_name(&a);
    let other = Login::new(rustango::sql::LoginName::new(format!("{a}-x")).unwrap());
    admin
        .create_tenant_database(&name, &other, &LoginPassword::generate())
        .await
        .expect("pre-existing");

    // The console's dry check sees it first, and creates nothing.
    let c = Console::boot(&env, &env.dir("good")).await;
    let probe = body(
        c.post(
            "/orgs/test-connection",
            &format!("slug={a}&storage_mode=database&database_source=server&server=main"),
        )
        .await,
    )
    .await;
    assert!(
        probe.contains("database") && probe.contains("already exists"),
        "{probe}"
    );
    assert!(env.org(&a).await.is_none());
    drop(c);

    let err = env
        .provision(&env.dir("good"), &ProvisionRequest::on_server(&a, "main"))
        .await
        .expect_err("exists");
    assert!(err.to_string().contains("already exists"), "{err}");
    assert!(
        env.database_exists(&a).await,
        "a database it did not make was dropped"
    );
    assert!(env.org(&a).await.is_none() && env.records(&a).await.is_empty());
    let steps = env.steps(&a).await;
    assert!(
        steps.iter().any(|s| s == "create_database:failed"),
        "{steps:?}"
    );
    assert!(
        !steps.iter().any(|s| s.starts_with("rollback")),
        "{steps:?}"
    );

    admin
        .drop_tenant_database(&name, &other)
        .await
        .expect("cleanup");
    admin.close().await;
}

/// A server whose admin may not provision is refused before any DDL.
async fn a_blocked_server_is_refused_before_any_ddl(pool: &Pool) {
    let _g = env_lock().lock().await;
    std::env::set_var("RUSTANGO_SECRET_KEY", KEY);
    let env = Env::new(pool).await;
    let a = slug("weak");
    env.clean(&[&a]).await;
    let weak = format!("tp{}weak", std::process::id());
    let drop_weak = match env.backend() {
        BackendKind::Postgres => {
            exec_text(&env.registry, &format!("DROP ROLE IF EXISTS \"{weak}\"")).await;
            exec_text(
                &env.registry,
                &format!("CREATE ROLE \"{weak}\" LOGIN PASSWORD 'weakpw' NOCREATEDB NOCREATEROLE"),
            )
            .await;
            Some(format!("DROP ROLE \"{weak}\""))
        }
        BackendKind::MySql => {
            exec_text(&env.registry, &format!("DROP USER IF EXISTS '{weak}'@'%'")).await;
            exec_text(
                &env.registry,
                &format!("CREATE USER '{weak}'@'%' IDENTIFIED BY 'weakpw'"),
            )
            .await;
            Some(format!("DROP USER '{weak}'@'%'"))
        }
        BackendKind::Sqlite => None,
    };
    env.add_server_as("main", Some((&weak, "weakpw"))).await;

    let err = env
        .provision(&env.dir("good"), &ProvisionRequest::on_server(&a, "main"))
        .await
        .expect_err("blocked");
    assert!(
        err.to_string().contains("cannot create tenant databases"),
        "{err}"
    );
    assert!(!env.database_exists(&a).await);
    let steps = env.steps(&a).await;
    assert!(
        steps.iter().any(|s| s == "check_connection:failed"),
        "{steps:?}"
    );
    assert!(
        !steps.iter().any(|s| s.starts_with("create_database")),
        "{steps:?}"
    );
    if let Some(sql) = drop_weak {
        exec_text(&env.registry, &sql).await;
    }
}

/// `api::create_tenant` takes the server path too; a backend other than
/// the registry's is refused.
async fn the_api_creates_on_a_saved_server(pool: &Pool) {
    let _g = env_lock().lock().await;
    std::env::set_var("RUSTANGO_SECRET_KEY", KEY);
    let env = Env::new(pool).await;
    let a = slug("api");
    env.clean(&[&a]).await;
    env.add_server("main").await;
    let good = env.dir("good");
    let opts = || CreateTenantOpts {
        mode: StorageMode::Database,
        database_server: Some("main".into()),
        ..CreateTenantOpts::default()
    };
    let both = CreateTenantOpts {
        database_url: Some("x".into()),
        ..opts()
    };
    let err = typed!(env.registry, p => create_tenant(&p, &env.url, &good, &a, both).await)
        .expect_err("both sources");
    assert!(err.to_string().contains("not both"), "{err}");
    let other = if env.server_backend() {
        BackendKind::Sqlite
    } else {
        BackendKind::Postgres
    };
    let wrong = CreateTenantOpts {
        backend: Some(other),
        ..opts()
    };
    let err = typed!(env.registry, p => create_tenant(&p, &env.url, &good, &a, wrong).await)
        .expect_err("another backend");
    assert!(err.to_string().contains("registry runs on"), "{err}");

    typed!(env.registry, p => create_tenant(&p, &env.url, &good, &a, opts()).await)
        .expect("created");
    assert_provisioned(&env, &a).await;
    env.clean(&[&a]).await;

    // A recorded database keeps its server in use, tenant row or not.
    let mut ghost = TenantDatabase {
        id: rustango::sql::Auto::default(),
        org_slug: "ghost".into(),
        org_id: None,
        server_name: "main".into(),
        database_name: "ghost".into(),
        login_name: None,
        created: false,
        created_at: rustango::sql::Auto::default(),
    };
    ghost.insert_pool(&env.registry).await.unwrap();
    // Only the record is left to say so.
    let orgs = Org::objects()
        .where_(Org::slug.eq(a.clone()))
        .compile_delete()
        .unwrap();
    rustango::sql::delete_pool(&env.registry, &orgs)
        .await
        .unwrap();
    let records = TenantDatabase::objects()
        .where_(TenantDatabase::org_slug.eq(a.clone()))
        .compile_delete()
        .unwrap();
    rustango::sql::delete_pool(&env.registry, &records)
        .await
        .unwrap();
    let err = env
        .run(&good, &["remove-db-server", "main"])
        .await
        .expect_err("in use");
    assert!(err.to_string().contains("ghost"), "{err}");
}

/// The new login signs in before anything is registered; a login the
/// server will not admit is rolled back. MySQL: a more specific account of
/// the same name shadows it.
async fn a_login_that_cannot_sign_in_is_rolled_back(pool: &Pool) {
    let _g = env_lock().lock().await;
    std::env::set_var("RUSTANGO_SECRET_KEY", KEY);
    let env = Env::new(pool).await;
    let a = slug("shadow");
    env.clean(&[&a]).await;
    env.add_server("main").await;
    let req = ProvisionRequest::on_server(&a, "main");
    if env.backend() != BackendKind::MySql {
        env.provision(&env.dir("good"), &req)
            .await
            .expect("created");
        let want = if env.server_backend() {
            "check_login:ok"
        } else {
            "check_login:skipped"
        };
        assert!(
            env.steps(&a).await.iter().any(|s| s == want),
            "{:?}",
            env.steps(&a).await
        );
        env.clean(&[&a]).await;
        return;
    }
    // This client's address, as the server sees it.
    let rows: Vec<(String,)> = rustango::sql::raw_query_pool(
        "SELECT CAST(SUBSTRING_INDEX(USER(), '@', -1) AS CHAR)",
        vec![],
        &env.registry,
    )
    .await
    .unwrap();
    let shadow = format!("'{a}'@'{}'", rows[0].0);
    exec_text(&env.registry, &format!("DROP USER IF EXISTS {shadow}")).await;
    exec_text(
        &env.registry,
        &format!("CREATE USER {shadow} IDENTIFIED BY 'other-pw'"),
    )
    .await;

    let err = env
        .provision(&env.dir("good"), &req)
        .await
        .expect_err("shadowed");
    exec_text(&env.registry, &format!("DROP USER {shadow}")).await;
    let msg = err.to_string();
    assert!(
        msg.contains("cannot sign in") && msg.contains("shadow"),
        "{msg}"
    );
    assert!(
        env.org(&a).await.is_none() && env.records(&a).await.is_empty(),
        "{msg}"
    );
    assert!(env.nothing_left(&a).await, "the database or login survived");
    let steps = env.steps(&a).await;
    assert!(steps.iter().any(|s| s == "check_login:failed"), "{steps:?}");
    assert!(steps.iter().any(|s| s == "rollback:ok"), "{steps:?}");
}

/// MySQL: the tenant database takes the registry's collation, not
/// utf8mb4's default, which ignores case.
async fn the_tenant_database_takes_the_registry_collation(pool: &Pool) {
    let _g = env_lock().lock().await;
    std::env::set_var("RUSTANGO_SECRET_KEY", KEY);
    let env = Env::new(pool).await;
    if env.backend() != BackendKind::MySql {
        return;
    }
    let a = slug("coll");
    env.clean(&[&a]).await;
    env.add_server("main").await;
    exec_text(&env.registry, "ALTER DATABASE COLLATE utf8mb4_0900_as_cs").await;
    env.provision(&env.dir("good"), &ProvisionRequest::on_server(&a, "main"))
        .await
        .expect("created");
    let rows: Vec<(String,)> = rustango::sql::raw_query_pool(
        "SELECT CAST(DEFAULT_COLLATION_NAME AS CHAR) FROM information_schema.SCHEMATA \
         WHERE SCHEMA_NAME = ?",
        vec![a.clone().into()],
        &env.registry,
    )
    .await
    .unwrap();
    assert_eq!(rows[0].0, "utf8mb4_0900_as_cs");
    env.clean(&[&a]).await;
}

/// A failure at the login check or at activate drops everything the run
/// made, and the error names what was undone.
async fn late_failures_roll_back(pool: &Pool) {
    let _g = env_lock().lock().await;
    std::env::set_var("RUSTANGO_SECRET_KEY", KEY);
    let env = Env::new(pool).await;
    env.add_server("main").await;
    for step in ["check_login", "activate"] {
        // A SQLite tenant has no login, so nothing to check.
        if step == "check_login" && !env.server_backend() {
            continue;
        }
        let a = slug(&format!("late-{}", &step[..3]));
        env.clean(&[&a]).await;
        std::env::set_var("RUSTANGO_PROVISION_FAIL_AT", step);
        let err = env
            .provision(&env.dir("good"), &ProvisionRequest::on_server(&a, "main"))
            .await
            .expect_err("injected");
        std::env::remove_var("RUSTANGO_PROVISION_FAIL_AT");
        let msg = err.to_string();
        assert!(msg.contains("dropped"), "{step}: {msg}");
        assert!(env.org(&a).await.is_none(), "{step}: the row survived");
        assert!(
            env.records(&a).await.is_empty(),
            "{step}: the record survived"
        );
        assert!(
            env.nothing_left(&a).await,
            "{step}: the database or login survived"
        );
        let steps = env.steps(&a).await;
        assert!(
            steps.iter().any(|s| s == "rollback:ok"),
            "{step}: {steps:?}"
        );
    }
}

/// A record an interrupted run left is reclaimed when it says the database
/// is rustango's; a fresh one means a run is in progress and is refused.
async fn an_interrupted_run_is_reclaimed(pool: &Pool) {
    use rustango::sql::UpdaterPool as _;
    let _g = env_lock().lock().await;
    std::env::set_var("RUSTANGO_SECRET_KEY", KEY);
    let env = Env::new(pool).await;
    let a = slug("intr");
    env.clean(&[&a]).await;
    env.add_server("main").await;
    // What a run killed after its CREATE leaves behind.
    let admin = env.admin().await;
    let name = db_name(&a);
    admin
        .create_tenant_database(
            &name,
            &Login::new(tenant_login_name(&name)),
            &LoginPassword::generate(),
        )
        .await
        .expect("leftover");
    admin.close().await;
    let mut record = TenantDatabase {
        id: rustango::sql::Auto::default(),
        org_slug: a.clone(),
        org_id: None,
        server_name: "main".into(),
        database_name: a.clone(),
        login_name: None,
        created: true,
        created_at: rustango::sql::Auto::default(),
    };
    record.insert_pool(&env.registry).await.unwrap();
    let req = ProvisionRequest::on_server(&a, "main");

    let err = env
        .provision(&env.dir("good"), &req)
        .await
        .expect_err("in progress");
    assert!(err.to_string().contains("another run"), "{err}");
    assert!(
        env.database_exists(&a).await,
        "a run in progress lost its database"
    );

    TenantDatabase::objects()
        .where_(TenantDatabase::org_slug.eq(a.clone()))
        .update()
        .set(
            "created_at",
            chrono::Utc::now() - chrono::Duration::hours(1),
        )
        .execute_pool(&env.registry)
        .await
        .unwrap();
    env.provision(&env.dir("good"), &req)
        .await
        .expect("reclaimed and created");
    assert_provisioned(&env, &a).await;
    let steps = env.steps(&a).await;
    assert!(steps.iter().any(|s| s == "rollback:ok"), "{steps:?}");
    env.clean(&[&a]).await;
}

/// A project root with an app's migrations: `migrations/` and `blog/migrations/`.
fn project_root(env: &Env) -> PathBuf {
    let root = env.dir("project");
    write_migrations(&root.join("migrations"), &["0001_notes"]);
    write_table_migration(
        &root.join("blog").join("migrations"),
        "0001_posts",
        "tp_posts",
    );
    root
}

/// The tenant at `url` has the app's table too.
async fn assert_app_migrated(url: &str) {
    let tenant = Pool::connect(url).await.expect("tenant connects");
    rustango::sql::raw_query_pool::<(i64,)>("SELECT COUNT(*) FROM tp_posts", vec![], &tenant)
        .await
        .expect("the app's migration ran");
    tenant.close().await;
}

/// The engine takes a project root and migrates every app dir in it.
async fn the_engine_migrates_every_app_dir(pool: &Pool) {
    let _g = env_lock().lock().await;
    std::env::set_var("RUSTANGO_SECRET_KEY", KEY);
    let env = Env::new(pool).await;
    let a = slug("root");
    env.clean(&[&a]).await;
    env.add_server("main").await;
    env.provision(
        &project_root(&env),
        &ProvisionRequest::on_server(&a, "main"),
    )
    .await
    .expect("created");
    let url = assert_provisioned(&env, &a).await;
    assert_app_migrated(&url).await;
    env.clean(&[&a]).await;
}

/// `api::create_tenant` runs the engine: its run records every step.
async fn the_api_runs_the_engine_steps(pool: &Pool) {
    let _g = env_lock().lock().await;
    std::env::set_var("RUSTANGO_SECRET_KEY", KEY);
    let env = Env::new(pool).await;
    let a = slug("apisteps");
    env.clean(&[&a]).await;
    env.add_server("main").await;
    let opts = CreateTenantOpts {
        mode: StorageMode::Database,
        database_server: Some("main".into()),
        ..CreateTenantOpts::default()
    };
    let root = project_root(&env);
    typed!(env.registry, p => create_tenant(&p, &env.url, &root, &a, opts).await).expect("created");
    let url = assert_provisioned(&env, &a).await;
    assert_app_migrated(&url).await;
    let steps = env.steps(&a).await;
    for step in [
        "validate:ok",
        "check_connection:ok",
        "register_org:ok",
        "migrate:ok",
        "activate:ok",
    ] {
        assert!(steps.iter().any(|s| s == step), "{step}: {steps:?}");
    }
    env.clean(&[&a]).await;
}

/// Two creates of one slug at once: one tenant, one clear refusal.
async fn concurrent_creates_make_one_tenant(pool: &Pool) {
    let _g = env_lock().lock().await;
    std::env::set_var("RUSTANGO_SECRET_KEY", KEY);
    let env = Env::new(pool).await;
    let a = slug("race");
    env.clean(&[&a]).await;
    env.add_server("main").await;
    let req = ProvisionRequest::on_server(&a, "main");
    let good = env.dir("good");
    let (x, y) = tokio::join!(env.provision(&good, &req), env.provision(&good, &req));
    assert_eq!(x.is_ok() as u8 + y.is_ok() as u8, 1, "{x:?} {y:?}");
    assert_provisioned(&env, &a).await;
    env.clean(&[&a]).await;
}

tri_dialect_test! {
    setup: setup,
    sqlite: file,
    scenarios: [
        the_console_creates_a_tenant_on_a_saved_server,
        the_cli_creates_tenants_on_a_saved_server,
        a_failed_migration_drops_everything_it_made,
        a_failed_first_admin_drops_everything_it_made,
        a_failed_insert_drops_what_it_made,
        an_existing_database_is_refused_and_kept,
        a_blocked_server_is_refused_before_any_ddl,
        the_api_creates_on_a_saved_server,
        a_login_that_cannot_sign_in_is_rolled_back,
        the_tenant_database_takes_the_registry_collation,
        late_failures_roll_back,
        an_interrupted_run_is_reclaimed,
        concurrent_creates_make_one_tenant,
        the_engine_migrates_every_app_dir,
        the_api_runs_the_engine_steps,
    ],
}
