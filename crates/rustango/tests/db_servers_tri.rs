//! Saved database servers: storage, sealing, probing and the CLI verbs,
//! on every registry backend. Each scenario gets a scratch registry.

#![cfg(all(feature = "tenancy", feature = "testkit", feature = "sqlite"))]

use std::path::PathBuf;
use std::time::Duration;

use rustango::sql::{LoginPassword, Pool};
use rustango::tenancy::db_server::{self, PasswordChange};
use rustango::tenancy::{
    BackendKind, DbServerError, DbServerSpec, Org, TenancyError, TenantPools, TlsMode,
};
use rustango::tri_dialect_test;

#[cfg(any(feature = "postgres", feature = "mysql"))]
#[path = "support/scratch_db.rs"]
mod scratch_db;

const KEY: &str = "db-servers-tri-key-0123456789abcdef0123";
/// Distinctive, so any echo of it is found.
const SECRET: &str = "Hunter2-Leak-q7Zx";

/// The scenarios set and unset `RUSTANGO_SECRET_KEY`; the SQLite arm takes no lock.
fn env_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn with_key() {
    std::env::set_var("RUSTANGO_SECRET_KEY", KEY);
    std::env::remove_var("RUSTANGO_SECRET_KEY_PREVIOUS");
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

/// The admin parts of a test server URL: user, password, host, port.
struct Server {
    user: String,
    password: String,
    host: String,
    port: u16,
}

impl Server {
    fn of(url: &str) -> Self {
        let rest = url.split_once("://").unwrap().1;
        let (userinfo, rest) = rest.rsplit_once('@').unwrap();
        let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
        let authority = rest.split(['/', '?']).next().unwrap();
        let (host, port) = authority.rsplit_once(':').unwrap();
        Self {
            user: user.into(),
            password: password.into(),
            host: host.into(),
            port: port.parse().unwrap(),
        }
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
        Self {
            url,
            registry,
            tmp,
            guards,
        }
    }

    fn backend(&self) -> BackendKind {
        BackendKind::parse(self.registry.dialect().name()).unwrap()
    }

    fn dir(&self) -> PathBuf {
        self.tmp.path().to_owned()
    }

    /// The live server under test, for PG and MySQL.
    fn server(&self) -> Server {
        Server::of(&self.url)
    }

    /// A spec for the live server, or the temp directory on SQLite.
    fn spec(&self, name: &str) -> DbServerSpec {
        let base = DbServerSpec {
            name: name.into(),
            backend: self.backend(),
            active: true,
            ..DbServerSpec::default()
        };
        if self.backend() == BackendKind::Sqlite {
            return DbServerSpec {
                sqlite_dir: Some(self.dir().display().to_string()),
                ..base
            };
        }
        let s = self.server();
        DbServerSpec {
            host: Some(s.host),
            port: Some(s.port),
            admin_user: Some(s.user),
            // The CI Postgres has no TLS; new servers default to `require`.
            tls: Some(TlsMode::Disable),
            ..base
        }
    }

    async fn run(&self, args: &[&str]) -> Result<String, TenancyError> {
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        let mut out = Vec::new();
        let dir = self.dir();
        typed!(self.registry, p => rustango::tenancy::manage::run_with_writer(&p, &self.url, &dir, args, &mut out).await)?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// The stored column, read past the model on purpose.
    async fn stored_password(&self, name: &str) -> Option<String> {
        let sql = format!("SELECT admin_password FROM rustango_db_servers WHERE name = '{name}'");
        let rows: Vec<(Option<String>,)> = rustango::sql::raw_query_pool(&sql, Vec::new(), &self.registry)
            .await
            .unwrap();
        rows.into_iter().next().expect("row").0
    }
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
async fn scratch(guards: &mut Vec<Box<dyn std::any::Any>>, var: &str) -> String {
    let db = scratch_db::ScratchDb::create(&std::env::var(var).unwrap(), "rustango_dbs").await;
    let url = db.url().to_owned();
    guards.push(Box::new(db));
    url
}

fn pw(s: &str) -> LoginPassword {
    LoginPassword::new(s).unwrap()
}

/// The password is stored sealed and appears in no output, error or Debug.
async fn the_password_is_sealed_and_never_shown(pool: &Pool) {
    let _g = env_lock().lock().await;
    with_key();
    let env = Env::new(pool).await;
    let var = format!("RUSTANGO_DBS_PW_{}", std::process::id());
    std::env::set_var(&var, SECRET);
    if env.backend() == BackendKind::Sqlite {
        let dir = env.dir().display().to_string();
        let err = env
            .run(&["add-db-server", "files", "--sqlite-dir", &dir, "--password-env", &var])
            .await
            .expect_err("a directory has no login")
            .to_string();
        assert!(!err.contains(SECRET), "{err}");
        return;
    }
    let s = env.server();
    let port = s.port.to_string();
    let out = env
        .run(&[
            "add-db-server", "main", "--host", &s.host, "--port", &port, "--user", &s.user,
            "--tls", "disable", "--password-env", &var,
        ])
        .await
        .expect("add");
    let stored = env.stored_password("main").await.expect("a password");
    assert!(stored.starts_with("enc:v1:") && !stored.contains(SECRET), "{stored}");

    let mut seen = vec![out];
    seen.push(env.run(&["list-db-servers"]).await.expect("list"));
    // A wrong password: the login is refused, and the report says so.
    let err = env.run(&["test-db-server", "main"]).await.expect_err("refused");
    seen.push(err.to_string());
    let row = db_server::get(&env.registry, "main").await.unwrap();
    let report = row.probe(Duration::from_secs(10)).await.expect("a report");
    assert!(!report.can_login, "{report:?}");
    seen.push(format!("{row:?} {report:?}"));
    seen.push(format!("{:?}", row.admin_url().await.unwrap()));
    seen.push(env.run(&["edit-db-server", "main", "--port", "1"]).await.expect("edit"));
    let unreachable = db_server::get(&env.registry, "main").await.unwrap();
    seen.push(format!("{:?}", unreachable.probe(Duration::from_secs(3)).await));
    seen.push(format!("{:?}", unreachable.connect(Duration::from_secs(3)).await.err()));
    for text in &seen {
        assert!(!text.contains(SECRET), "{text}");
    }
    assert!(seen[1].contains("main") && seen[1].contains("saved"), "{}", seen[1]);

    // The driver names the login it refused; the diagnosis must not.
    const USER: &str = "leakuser-q7";
    let ghost = DbServerSpec {
        admin_user: Some(USER.into()),
        ..env.spec("ghost")
    };
    db_server::add(&env.registry, &ghost, Some(&pw(SECRET))).await.unwrap();
    let row = db_server::get(&env.registry, "ghost").await.unwrap();
    let report = row.probe(Duration::from_secs(10)).await.expect("a report");
    let diagnosis = report.login_error.as_ref().expect("refused").to_string();
    let err = env.run(&["test-db-server", "ghost"]).await.expect_err("refused").to_string();
    for text in [format!("{report:?}"), diagnosis, err] {
        assert!(!text.contains(USER) && !text.contains(SECRET), "{text}");
    }
}

/// Without a key, a password is refused, not stored in plain text.
async fn without_a_key_a_password_is_refused(pool: &Pool) {
    let _g = env_lock().lock().await;
    let env = Env::new(pool).await;
    if env.backend() == BackendKind::Sqlite {
        return;
    }
    std::env::remove_var("RUSTANGO_SECRET_KEY");
    let err = db_server::add(&env.registry, &env.spec("nokey"), Some(&pw(SECRET)))
        .await
        .expect_err("no key");
    with_key();
    assert!(matches!(err, DbServerError::Seal(_)), "{err:?}");
    assert!(err.to_string().contains("RUSTANGO_SECRET_KEY") && !err.to_string().contains(SECRET), "{err}");
    let gone = db_server::get(&env.registry, "nokey").await.expect_err("not stored");
    assert!(matches!(gone, DbServerError::NotFound(_)), "{gone:?}");
    // A blank key would seal under SHA-256 of nothing.
    std::env::set_var("RUSTANGO_SECRET_KEY", "");
    let err = db_server::add(&env.registry, &env.spec("blankkey"), Some(&pw(SECRET)))
        .await
        .expect_err("blank key");
    with_key();
    assert!(matches!(err, DbServerError::Seal(_)), "{err:?}");
}

/// The live login and its rights; a maintenance database is honoured.
async fn a_probe_reports_the_admin_rights(pool: &Pool) {
    let _g = env_lock().lock().await;
    with_key();
    let env = Env::new(pool).await;
    let real = (env.backend() != BackendKind::Sqlite).then(|| pw(&env.server().password));
    let row = db_server::add(&env.registry, &env.spec("live"), real.as_ref())
        .await
        .expect("add");
    let report = row.probe(Duration::from_secs(10)).await.expect("probe");
    assert!(report.can_login, "{report:?}");
    match env.backend() {
        BackendKind::Postgres => {
            assert!(report.pg_major.is_some_and(|m| m >= 15), "{report:?}");
            // The CI role is a superuser.
            assert!(report.can_provision(), "{:?}", report.blocker());
            let out = env.run(&["test-db-server", "live"]).await.expect("ready");
            assert!(out.contains("ready") && out.contains("server version"), "{out}");
            let mut spec = row.spec().unwrap();
            spec.maintenance_db = Some("no_such_db_dbs".into());
            let (row, changed) = db_server::update(&env.registry, "live", &spec, PasswordChange::Keep, None)
                .await
                .unwrap();
            assert_eq!(changed, vec!["maintenance_db"]);
            let err = row.probe(Duration::from_secs(10)).await.expect_err("no such database");
            assert!(err.to_string().contains("no_such_db_dbs"), "{err}");
        }
        BackendKind::MySql => assert!(report.server_version.is_some(), "{report:?}"),
        BackendKind::Sqlite => assert!(report.can_provision(), "{:?}", report.blocker()),
    }
}

/// Keep leaves the sealed value alone; Set replaces it; Clear removes it.
async fn an_edit_keeps_the_password_unless_told(pool: &Pool) {
    let _g = env_lock().lock().await;
    with_key();
    let env = Env::new(pool).await;
    if env.backend() == BackendKind::Sqlite {
        let row = db_server::add(&env.registry, &env.spec("files"), None).await.unwrap();
        let mut spec = row.spec().unwrap();
        spec.active = false;
        let (_, changed) = db_server::update(&env.registry, "files", &spec, PasswordChange::Keep, None)
            .await
            .unwrap();
        assert_eq!(changed, vec!["active"]);
        return;
    }
    let row = db_server::add(&env.registry, &env.spec("e"), Some(&pw(SECRET))).await.unwrap();
    let first = env.stored_password("e").await.unwrap();
    let mut spec = row.spec().unwrap();
    spec.port = Some(1);
    let (_, changed) = db_server::update(&env.registry, "e", &spec, PasswordChange::Keep, None)
        .await
        .unwrap();
    assert_eq!(changed, vec!["port"]);
    assert_eq!(env.stored_password("e").await.as_deref(), Some(first.as_str()));
    let (_, changed) = db_server::update(&env.registry, "e", &spec, PasswordChange::Set(pw("other")), None)
        .await
        .unwrap();
    assert_eq!(changed, vec!["admin_password"]);
    let second = env.stored_password("e").await.unwrap();
    assert!(second != first && second.starts_with("enc:v1:"));
    let (row, _) = db_server::update(&env.registry, "e", &spec, PasswordChange::Clear, None)
        .await
        .unwrap();
    assert!(!row.has_password() && env.stored_password("e").await.is_none());
    spec.name = "renamed".into();
    assert!(db_server::update(&env.registry, "e", &spec, PasswordChange::Keep, None).await.is_err());
}

/// A server of another backend, a duplicate and a bad name are refused.
async fn bad_servers_are_refused(pool: &Pool) {
    let _g = env_lock().lock().await;
    with_key();
    let env = Env::new(pool).await;
    let other = if env.backend() == BackendKind::Sqlite {
        BackendKind::Postgres
    } else {
        BackendKind::Sqlite
    };
    let foreign = DbServerSpec {
        backend: other,
        ..env.spec("foreign")
    };
    let err = db_server::add(&env.registry, &foreign, None).await.expect_err("other backend");
    assert!(matches!(err, DbServerError::BackendMismatch { .. }), "{err:?}");
    db_server::add(&env.registry, &env.spec("dup"), None).await.unwrap();
    let err = db_server::add(&env.registry, &env.spec("dup"), None).await.expect_err("taken");
    assert!(matches!(err, DbServerError::Taken(_)), "{err:?}");
    let err = db_server::add(&env.registry, &env.spec("Bad_Name"), None).await.expect_err("name");
    assert!(matches!(err, DbServerError::Invalid(_)), "{err:?}");
}

/// Removal is refused while a tenant's database is on the server.
async fn a_server_in_use_is_not_removed(pool: &Pool) {
    let _g = env_lock().lock().await;
    with_key();
    let env = Env::new(pool).await;
    db_server::add(&env.registry, &env.spec("used"), None).await.unwrap();
    let tenant_url = match env.backend() {
        BackendKind::Sqlite => format!("sqlite://{}?mode=rwc", env.dir().join("acme.db").display()),
        _ => env.url.clone(),
    };
    let mut org = Org {
        slug: "acme".into(),
        display_name: "acme".into(),
        backend_kind: env.backend().as_str().into(),
        database_url: Some(tenant_url),
        ..rustango::testkit::org()
    };
    org.save_pool(&env.registry).await.expect("insert org");
    let err = env.run(&["remove-db-server", "used"]).await.expect_err("in use").to_string();
    assert!(err.contains("acme"), "{err}");
    assert!(db_server::get(&env.registry, "used").await.is_ok());

    let elsewhere = match env.backend() {
        BackendKind::Sqlite => DbServerSpec {
            sqlite_dir: Some(env.dir().join("other").display().to_string()),
            ..env.spec("free")
        },
        _ => DbServerSpec {
            host: Some("db.invalid".into()),
            ..env.spec("free")
        },
    };
    db_server::add(&env.registry, &elsewhere, None).await.unwrap();
    let out = env.run(&["remove-db-server", "free"]).await.expect("removed");
    assert!(out.contains("removed"), "{out}");
    assert!(db_server::get(&env.registry, "free").await.is_err());
}

/// A tenant whose URL is missing or does not resolve may be on the
/// server, so removal is refused and says why.
async fn an_unknown_tenant_url_blocks_removal(pool: &Pool) {
    let _g = env_lock().lock().await;
    with_key();
    let env = Env::new(pool).await;
    db_server::add(&env.registry, &env.spec("maybe"), None).await.unwrap();
    for (slug, url) in [
        ("lost", Some("env://RUSTANGO_DBS_UNSET_VAR_q7".to_owned())),
        ("blank", None),
    ] {
        let mut org = Org {
            slug: slug.into(),
            display_name: slug.into(),
            backend_kind: env.backend().as_str().into(),
            database_url: url,
            ..rustango::testkit::org()
        };
        org.save_pool(&env.registry).await.expect("insert org");
    }
    let err = env.run(&["remove-db-server", "maybe"]).await.expect_err("unknown").to_string();
    assert!(err.contains("lost (its database URL does not resolve here)"), "{err}");
    assert!(err.contains("blank (it has no database URL)"), "{err}");
    assert!(db_server::get(&env.registry, "maybe").await.is_ok());
}

/// An edit built from an older read is refused, not written over the newer one.
async fn a_stale_edit_is_refused(pool: &Pool) {
    let _g = env_lock().lock().await;
    with_key();
    let env = Env::new(pool).await;
    let row = db_server::add(&env.registry, &env.spec("cas"), None).await.unwrap();
    let first = db_server::get(&env.registry, "cas").await.unwrap();
    let seen = first.updated_at.get().copied();
    assert!(seen.is_some(), "{row:?}");
    let mut spec = first.spec().unwrap();
    spec.active = false;
    db_server::update(&env.registry, "cas", &spec, PasswordChange::Keep, seen)
        .await
        .expect("fresh");
    spec.active = true;
    let err = db_server::update(&env.registry, "cas", &spec, PasswordChange::Keep, seen)
        .await
        .expect_err("stale");
    assert!(matches!(err, DbServerError::Stale(_)), "{err:?}");
    assert!(!db_server::get(&env.registry, "cas").await.unwrap().active);
}

/// New servers get TLS `require` unless told otherwise.
async fn new_servers_require_tls(pool: &Pool) {
    let _g = env_lock().lock().await;
    with_key();
    let env = Env::new(pool).await;
    let spec = DbServerSpec {
        tls: None,
        ..env.spec("tls")
    };
    let row = db_server::add(&env.registry, &spec, None).await.unwrap();
    if env.backend() == BackendKind::Sqlite {
        assert_eq!(row.tls_mode, None);
        return;
    }
    assert_eq!(row.tls_mode.as_deref(), Some("require"));
    assert!(row.tls_warning().is_none());
    let mut spec = row.spec().unwrap();
    spec.tls = None;
    let (row, _) = db_server::update(&env.registry, "tls", &spec, PasswordChange::Keep, None)
        .await
        .unwrap();
    assert!(row.tls_warning().is_some(), "an old row with no mode warns");
}

/// `reseal-tenant-secrets` re-seals server passwords under the new key.
async fn reseal_covers_server_passwords(pool: &Pool) {
    let _g = env_lock().lock().await;
    let env = Env::new(pool).await;
    if env.backend() == BackendKind::Sqlite {
        return;
    }
    std::env::set_var("RUSTANGO_SECRET_KEY", "old-dbs-key-0123456789abcdef0123456789");
    db_server::add(&env.registry, &env.spec("rot"), Some(&pw(SECRET))).await.unwrap();
    let old = env.stored_password("rot").await.unwrap();
    std::env::set_var("RUSTANGO_SECRET_KEY_PREVIOUS", "old-dbs-key-0123456789abcdef0123456789");
    std::env::set_var("RUSTANGO_SECRET_KEY", KEY);
    let dry = env.run(&["reseal-tenant-secrets"]).await.expect("dry run");
    assert!(dry.contains("server rot: would re-seal"), "{dry}");
    assert_eq!(env.stored_password("rot").await.as_deref(), Some(old.as_str()));
    let out = env.run(&["reseal-tenant-secrets", "--apply"]).await.expect("apply");
    assert!(out.contains("server rot: re-sealed"), "{out}");
    let new = env.stored_password("rot").await.unwrap();
    assert!(new != old && new.starts_with("enc:v1:") && !out.contains(SECRET), "{out}");
    // Opens without the old key now.
    std::env::remove_var("RUSTANGO_SECRET_KEY_PREVIOUS");
    let row = db_server::get(&env.registry, "rot").await.unwrap();
    assert!(row.admin_url().await.is_ok());
    with_key();
}

// ------------------------------------------------------------- console

struct Console {
    app: axum::Router,
    cookie: String,
}

enum Mode {
    ReadOnly,
    Full,
    /// A provisioner but no tenant pools.
    NoPools,
}

async fn seed_operator(env: &Env) {
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
}

/// A console POST without the CSRF token is refused and writes nothing.
async fn a_console_post_needs_csrf(pool: &Pool) {
    use tower::ServiceExt as _;
    let _g = env_lock().lock().await;
    with_key();
    let env = Env::new(pool).await;
    seed_operator(&env).await;
    let c = Console::boot(&env, true).await;
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/db-servers")
        .header("cookie", &c.cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(axum::body::Body::from("name=nocsrf&sqlite_dir=/tmp&active=on"))
        .unwrap();
    let resp = c.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status().as_u16(), 403);
    assert!(db_server::get(&env.registry, "nocsrf").await.is_err());
}

/// Without tenant pools the console cannot tell who uses a server, so it
/// does not delete it.
async fn a_console_without_pools_does_not_delete(pool: &Pool) {
    let _g = env_lock().lock().await;
    with_key();
    let env = Env::new(pool).await;
    seed_operator(&env).await;
    db_server::add(&env.registry, &env.spec("kept"), None).await.unwrap();
    let c = Console::boot_with(&env, Mode::NoPools).await;
    let resp = c.post("/db-servers/kept/delete", "").await;
    assert_eq!(resp.status().as_u16(), 303);
    let location = resp.headers()["location"].to_str().unwrap().to_owned();
    assert!(location.contains("remove-db-server"), "{location}");
    assert!(db_server::get(&env.registry, "kept").await.is_ok());
}

impl Console {
    /// Signed in, with provisioning (`full`) or read-only.
    async fn boot(env: &Env, full: bool) -> Self {
        Self::boot_with(env, if full { Mode::Full } else { Mode::ReadOnly }).await
    }

    async fn boot_with(env: &Env, mode: Mode) -> Self {
        use rustango::tenancy::operator_console::{
            router, router_full, router_with_provisioning, SessionSecret,
        };
        let dir = env.dir();
        let app = match mode {
            Mode::Full => typed!(env.registry, p => {
                let pools = std::sync::Arc::new(p);
                let provisioner = rustango::tenancy::provision::Provisioner::new(
                    pools.clone(), env.url.clone(), dir).erased();
                router_with_provisioning(env.registry.clone(), pools, provisioner, SessionSecret::from_env_or_random())
            }),
            Mode::NoPools => typed!(env.registry, p => {
                let provisioner = rustango::tenancy::provision::Provisioner::new(
                    std::sync::Arc::new(p), env.url.clone(), dir).erased();
                router_full(
                    env.registry.clone(),
                    None,
                    Some(provisioner),
                    SessionSecret::from_env_or_random(),
                    rustango::tenancy::branding::default_brand_storage(),
                    None,
                    "/_impersonation_handoff".into(),
                )
            }),
            Mode::ReadOnly => router(env.registry.clone(), SessionSecret::from_env_or_random()),
        };
        let mut c = Self {
            app,
            cookie: String::new(),
        };
        let login = c.post("/login", "username=op&password=letmein").await;
        c.cookie = login
            .headers()
            .get("set-cookie")
            .expect("session cookie")
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        c
    }

    async fn get(&self, uri: &str) -> (u16, String) {
        use tower::ServiceExt as _;
        let req = axum::http::Request::builder()
            .uri(uri)
            .header("cookie", &self.cookie)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        (status, body(resp).await)
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
}

async fn body(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", rustango::url_codec::url_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The console pages, probe and audit rows never carry the password.
async fn the_console_never_shows_the_password(pool: &Pool) {
    let _g = env_lock().lock().await;
    with_key();
    let env = Env::new(pool).await;
    seed_operator(&env).await;
    let read_only = Console::boot(&env, false).await;
    assert_eq!(read_only.get("/db-servers").await.0, 404, "mounted without provisioning");
    let c = Console::boot(&env, true).await;
    let (status, page) = c.get("/db-servers").await;
    assert_eq!(status, 200, "{page}");
    assert!(page.contains("Add a database server"), "{page}");

    let sqlite = env.backend() == BackendKind::Sqlite;
    let (port, fields) = if sqlite {
        (String::new(), vec![("sqlite_dir", env.dir().display().to_string())])
    } else {
        let s = env.server();
        (
            s.port.to_string(),
            vec![("host", s.host), ("admin_user", s.user), ("tls_mode", "disable".into())],
        )
    };
    let mut pairs: Vec<(&str, &str)> = vec![("name", "web"), ("active", "on")];
    pairs.extend(fields.iter().map(|(k, v)| (*k, v.as_str())));
    if !sqlite {
        pairs.push(("port", &port));
    }
    let mut with_pw = pairs.clone();
    with_pw.push(("admin_password", SECRET));
    let mut seen = Vec::new();

    let resp = c.post("/db-servers", &form(&with_pw)).await;
    if sqlite {
        // No login on SQLite: refused, and the form does not echo it.
        let page = body(resp).await;
        assert!(page.contains("no password"), "{page}");
        seen.push(page);
        let resp = c.post("/db-servers", &form(&pairs)).await;
        assert_eq!(resp.status().as_u16(), 303, "{}", body(resp).await);
    } else {
        assert_eq!(resp.status().as_u16(), 303, "{}", body(resp).await);
    }
    let stored = env.stored_password("web").await;
    assert_eq!(stored.is_some(), !sqlite);

    let (_, list) = c.get("/db-servers").await;
    assert!(list.contains("web"), "{list}");
    seen.push(list);
    let (_, edit) = c.get("/db-servers/web/edit").await;
    if !sqlite {
        assert!(edit.contains(r#"type="password" autocomplete="new-password">"#), "{edit}");
    }

    // The version the form was built from rides along.
    let version = edit
        .split(r#"name="version" value=""#)
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("a version field")
        .to_owned();
    seen.push(edit);

    // Blank password keeps it; a bad port re-renders without echoing a typed one.
    let mut edited = pairs.clone();
    edited.retain(|(k, _)| *k != "active");
    let mut fresh = edited.clone();
    fresh.push(("version", &version));
    let resp = c.post("/db-servers/web/edit", &form(&fresh)).await;
    assert_eq!(resp.status().as_u16(), 303, "{}", body(resp).await);
    assert_eq!(env.stored_password("web").await, stored, "a blank field kept it");
    assert!(!db_server::get(&env.registry, "web").await.unwrap().active);
    // Same form again: its version is now old, so nothing is written.
    let mut stale = fresh.clone();
    stale.retain(|(k, _)| *k != "active");
    stale.push(("active", "on"));
    let resp = c.post("/db-servers/web/edit", &form(&stale)).await;
    assert_eq!(resp.status().as_u16(), 303);
    let location = resp.headers()["location"].to_str().unwrap().to_owned();
    let (_, page) = c.get(&location).await;
    assert!(page.contains("changed meanwhile"), "{page}");
    assert!(!db_server::get(&env.registry, "web").await.unwrap().active);
    if !sqlite {
        let mut bad = edited.clone();
        bad.retain(|(k, _)| *k != "port");
        bad.push(("port", "eighty"));
        bad.push(("admin_password", SECRET));
        let page = body(c.post("/db-servers/web/edit", &form(&bad)).await).await;
        assert!(page.contains("Port must be"), "{page}");
        seen.push(page);
    }

    let probe = body(c.post("/db-servers/web/test", "").await).await;
    let json: serde_json::Value = serde_json::from_str(&probe).expect("json");
    assert_eq!(json["status"], if sqlite { "ok" } else { "bad" }, "{probe}");
    seen.push(probe);

    let entries = rustango::audit::list(
        &env.registry,
        &rustango::audit::AuditFilter {
            entity_table: Some("rustango_db_servers".into()),
            ..Default::default()
        },
        50,
        0,
    )
    .await
    .unwrap();
    let audit = format!("{:?}", entries.iter().map(|e| &e.changes).collect::<Vec<_>>());
    assert!(entries.len() >= 2, "{audit}");
    if !sqlite {
        assert!(audit.contains("admin_password"), "names the field: {audit}");
    }
    assert!(!audit.contains("enc:v1"), "{audit}");
    let sources: Vec<&str> = entries.iter().map(|e| e.source.as_str()).collect();
    assert!(sources.iter().any(|s| s.ends_with(":db_server_test")), "{sources:?}");
    seen.push(audit);

    for text in &seen {
        assert!(!text.contains(SECRET), "{text}");
    }
    let resp = c.post("/db-servers/web/delete", "").await;
    assert_eq!(resp.status().as_u16(), 303);
    assert!(db_server::get(&env.registry, "web").await.is_err());
}

/// `reseal-tenant-secrets` also re-seals SSO client secrets, opened with
/// the previous key, so the old key can then go.
#[cfg(feature = "admin-sso")]
#[tokio::test]
async fn reseal_covers_sso_client_secrets() {
    use rustango::casts::{CastValue as _, EncryptedString};
    let _g = env_lock().lock().await;
    let pool = rustango::testkit::matrix::sqlite_file_pool().await;
    let env = Env::new(&pool).await;
    let old = "old-sso-key-0123456789abcdef0123456789";
    std::env::set_var("RUSTANGO_SECRET_KEY", old);
    let sealed = EncryptedString::to_db(&"client-secret".to_owned());
    let insert = format!(
        "INSERT INTO rustango_shared_sso_providers (slug, label, kind, client_id, client_secret, \
         enabled, sort_order, allow_email_link, created_at, updated_at) VALUES ('g', 'G', \
         'google', 'cid', '{sealed}', 1, 0, 0, '2026-01-01 00:00:00', '2026-01-01 00:00:00')"
    );
    rustango::sql::raw_execute_pool(&env.registry, &insert, Vec::new()).await.unwrap();
    std::env::set_var("RUSTANGO_SECRET_KEY_PREVIOUS", old);
    std::env::set_var("RUSTANGO_SECRET_KEY", KEY);
    let out = env.run(&["reseal-tenant-secrets", "--apply"]).await.expect("apply");
    assert!(out.contains("sso shared/g: re-sealed"), "{out}");
    std::env::remove_var("RUSTANGO_SECRET_KEY_PREVIOUS");
    let rows: Vec<(String,)> = rustango::sql::raw_query_pool(
        "SELECT client_secret FROM rustango_shared_sso_providers",
        Vec::new(),
        &env.registry,
    )
    .await
    .unwrap();
    assert_eq!(EncryptedString::from_db(&rows[0].0).unwrap(), "client-secret");
}

/// The pages warn while the key is missing or short, without showing it.
async fn the_console_warns_without_a_good_key(pool: &Pool) {
    let _g = env_lock().lock().await;
    with_key();
    let env = Env::new(pool).await;
    seed_operator(&env).await;
    db_server::add(&env.registry, &env.spec("warned"), None).await.unwrap();
    let c = Console::boot(&env, true).await;
    const SHORT: &str = "short-key-q7Zx";
    for (key, warned) in [
        (None, true),
        (Some(""), true),
        (Some(SHORT), true),
        (Some(KEY), false),
    ] {
        match key {
            Some(k) => std::env::set_var("RUSTANGO_SECRET_KEY", k),
            None => std::env::remove_var("RUSTANGO_SECRET_KEY"),
        }
        for uri in ["/db-servers", "/db-servers/warned/edit", "/orgs/new"] {
            let (status, page) = c.get(uri).await;
            assert_eq!(status, 200, "{page}");
            assert_eq!(page.contains("key-warning"), warned, "{key:?} {uri}: {page}");
            assert!(!page.contains(SHORT) && !page.contains(KEY), "{uri}: {page}");
        }
    }
    with_key();
}

tri_dialect_test! {
    setup: setup,
    sqlite: file,
    scenarios: [
        the_console_warns_without_a_good_key,
        an_unknown_tenant_url_blocks_removal,
        a_stale_edit_is_refused,
        new_servers_require_tls,
        reseal_covers_server_passwords,
        a_console_post_needs_csrf,
        a_console_without_pools_does_not_delete,
        the_console_never_shows_the_password,
        the_password_is_sealed_and_never_shown,
        without_a_key_a_password_is_refused,
        a_probe_reports_the_admin_rights,
        an_edit_keeps_the_password_unless_told,
        bad_servers_are_refused,
        a_server_in_use_is_not_removed,
    ],
}
