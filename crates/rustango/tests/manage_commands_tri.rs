//! `manage` with only the `DB_*` vars set, and app / crate commands with
//! `--tenant <slug>`, on every registry backend.
//!
//! The scenarios rewrite `DATABASE_URL` and `DB_*`; the matrix's
//! suite-wide `live_lock` runs them one at a time.

#![cfg(all(
    feature = "manage",
    feature = "tenancy",
    feature = "testkit",
    feature = "sqlite"
))]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rustango::manage::{Cli, CommandContext};
use rustango::sql::Pool;
use rustango::tenancy::manage::api::{create_tenant, CreateTenantOpts};
use rustango::tenancy::{Org, StorageMode, TenantPools};
use rustango::tri_dialect_test;

#[cfg(any(feature = "postgres", feature = "mysql"))]
#[path = "support/scratch_db.rs"]
mod scratch_db;

/// What the crate-registered command saw: dialect and arguments.
static REGISTERED: Mutex<Vec<(String, Vec<String>)>> = Mutex::new(Vec::new());

rustango::register_command!(
    "tri-registered",
    "A command another crate registered",
    |ctx: CommandContext, args: Vec<String>| async move {
        rustango::sql::raw_execute_pool(ctx.pool(), "SELECT 1", Vec::new()).await?;
        let dialect = ctx.pool().dialect().name().to_owned();
        REGISTERED.lock().unwrap().push((dialect, args));
        Ok(())
    },
);

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

const DB_VARS: &[&str] = &[
    "DATABASE_URL",
    "DB_DRIVER",
    "DB_HOST",
    "DB_PORT",
    "DB_USER",
    "DB_PASSWORD",
    "DB_PASSWORD_FILE",
    "DB_NAME",
    "DB_PARAMS",
];

/// Puts `DATABASE_URL` and `DB_*` back as they were.
struct EnvRestore(Vec<(&'static str, Option<String>)>);

impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (name, prev) in &self.0 {
            match prev {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
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
            Pool::Sqlite(_) => format!("sqlite:{}", tmp.path().join("reg.db").display()),
        };
        let registry = Pool::connect(&url).await.expect("registry");
        Self {
            url,
            registry,
            tmp,
            guards,
        }
    }

    fn dir(&self) -> PathBuf {
        let dir = self.tmp.path().join("migrations");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Unset `DATABASE_URL` and describe the registry with `DB_*` alone.
    /// The Postgres password goes through `DB_PASSWORD_FILE`.
    fn only_db_vars(&self) -> EnvRestore {
        let restore = EnvRestore(
            DB_VARS
                .iter()
                .map(|n| (*n, std::env::var(n).ok()))
                .collect(),
        );
        for n in DB_VARS {
            std::env::remove_var(n);
        }
        if let Some(path) = self.url.strip_prefix("sqlite:") {
            std::env::set_var("DB_DRIVER", "sqlite");
            std::env::set_var("DB_NAME", path);
            return restore;
        }
        let (scheme, rest) = self.url.split_once("://").unwrap();
        let (userinfo, hostpath) = rest.rsplit_once('@').unwrap();
        let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
        let (hostport, db) = hostpath.split_once('/').unwrap();
        let (host, port) = hostport.rsplit_once(':').unwrap_or((hostport, ""));
        let (db, params) = db.split_once('?').unwrap_or((db, ""));
        std::env::set_var("DB_DRIVER", scheme);
        std::env::set_var("DB_HOST", host);
        std::env::set_var("DB_USER", user);
        std::env::set_var("DB_NAME", db);
        if !port.is_empty() {
            std::env::set_var("DB_PORT", port);
        }
        if !params.is_empty() {
            std::env::set_var("DB_PARAMS", params);
        }
        if !password.is_empty() {
            if scheme.starts_with("postgres") {
                let file = self.tmp.path().join("db_password");
                std::fs::write(&file, format!("{password}\n")).unwrap();
                std::env::set_var("DB_PASSWORD_FILE", file);
            } else {
                std::env::set_var("DB_PASSWORD", password);
            }
        }
        restore
    }

    /// A tenant with its own storage, holding a `cmd_marker` table the
    /// registry does not have.
    async fn tenant(&mut self, slug: &str, mode: StorageMode) -> Org {
        let opts = match mode {
            StorageMode::Schema => CreateTenantOpts {
                mode,
                schema_name: Some(slug.replace('-', "_")),
                no_migrate: true,
                ..CreateTenantOpts::default()
            },
            _ => CreateTenantOpts {
                mode,
                database_url: Some(self.tenant_db_url(slug).await),
                no_migrate: true,
                ..CreateTenantOpts::default()
            },
        };
        let dir = self.dir();
        let org = typed!(self.registry, p => create_tenant(&p, &self.url, &dir, slug, opts).await)
            .expect("create tenant");
        let pool = typed!(self.registry, p => p.scoped_pool_dyn(&org).await.expect("tenant pool"));
        rustango::sql::raw_execute_pool(&pool, "CREATE TABLE cmd_marker (id INT)", Vec::new())
            .await
            .expect("marker table");
        org
    }

    async fn tenant_db_url(&mut self, slug: &str) -> String {
        match &self.registry {
            #[cfg(feature = "postgres")]
            Pool::Postgres(_) => scratch(&mut self.guards, "DATABASE_URL").await,
            #[cfg(feature = "mysql")]
            Pool::Mysql(_) => scratch(&mut self.guards, "MYSQL_TEST_URL").await,
            Pool::Sqlite(_) => format!(
                "sqlite://{}?mode=rwc",
                self.tmp.path().join(format!("{slug}.db")).display()
            ),
        }
    }
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
async fn scratch(guards: &mut Vec<Box<dyn std::any::Any>>, var: &str) -> String {
    let db = scratch_db::ScratchDb::create(&std::env::var(var).unwrap(), "rustango_mc").await;
    let url = db.url().to_owned();
    guards.push(Box::new(db));
    url
}

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

/// Built-in verbs and a registered command reach the database the
/// `DB_*` vars describe, with no `DATABASE_URL`.
async fn manage_connects_with_only_the_db_vars(backend: &Pool) {
    let env = Env::new(backend).await;
    let dialect = env.registry.dialect().name().to_owned();
    let _restore = env.only_db_vars();
    assert!(std::env::var("DATABASE_URL").is_err());

    Cli::new()
        .migrations_dir(env.dir())
        .run_with_args(args(&["migrate"]))
        .await
        .expect("migrate with DB_* only");

    REGISTERED.lock().unwrap().clear();
    Cli::new()
        .migrations_dir(env.dir())
        .run_with_args(args(&["tri-registered", "a", "b"]))
        .await
        .expect("registered command with DB_* only");
    assert_eq!(
        REGISTERED.lock().unwrap().as_slice(),
        &[(dialect, args(&["a", "b"]))]
    );

    // The same database: a table the app command makes is in the registry.
    Cli::new()
        .migrations_dir(env.dir())
        .command("tri-make", "", |ctx: CommandContext, _| async move {
            let sql = "CREATE TABLE cmd_made (id INT)";
            rustango::sql::raw_execute_pool(ctx.pool(), sql, Vec::new()).await?;
            Ok(())
        })
        .run_with_args(args(&["tri-make"]))
        .await
        .expect("app command");
    rustango::sql::raw_execute_pool(&env.registry, "SELECT COUNT(*) FROM cmd_made", Vec::new())
        .await
        .expect("the command wrote to the DB_* database");
}

/// An app model, so `makemigrations` has a project table to write.
#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "mc_check_item")]
#[allow(dead_code)]
pub struct CheckItem {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    #[rustango(max_length = 32, default = "'new'")]
    pub state: String,
}

async fn manage_out(pool: &Pool, dir: &std::path::Path, argv: &[&str]) -> String {
    let mut out = Vec::new();
    rustango::migrate::manage::run_with_writer(pool, dir, args(argv), &mut out)
        .await
        .unwrap_or_else(|e| panic!("{argv:?}: {e}"));
    String::from_utf8(out).unwrap()
}

/// `check` reports migrations the ledger has not applied, and model
/// changes no migration holds; once migrated it reports neither.
async fn check_reports_unapplied_and_unmade_migrations(backend: &Pool) {
    let env = Env::new(backend).await;
    let dir = env.dir();
    manage_out(&env.registry, &dir, &["makemigrations"]).await;
    let out = manage_out(&env.registry, &dir, &["check"]).await;
    assert!(out.contains("not applied — run `manage migrate`"), "{out}");
    assert!(!out.contains("models have changes"), "{out}");

    manage_out(&env.registry, &dir, &["migrate"]).await;
    let out = manage_out(&env.registry, &dir, &["check"]).await;
    assert!(!out.contains("not applied"), "{out}");
    assert!(!out.contains("models have changes"), "{out}");

    // Only an empty file: every model is a change no migration holds.
    let bare = env.tmp.path().join("bare").join("migrations");
    std::fs::create_dir_all(&bare).unwrap();
    manage_out(
        &env.registry,
        &bare,
        &["makemigrations", "--empty", "start"],
    )
    .await;
    let out = manage_out(&env.registry, &bare, &["check"]).await;
    assert!(
        out.contains("models have changes no migration holds"),
        "{out}"
    );
}

/// `migrate <target>` says which way each file went, and CLI errors carry
/// no "invalid migration" prefix or stale version.
async fn migrate_target_names_each_direction(backend: &Pool) {
    let env = Env::new(backend).await;
    let dir = env.tmp.path().join("steps").join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    manage_out(&env.registry, &dir, &["makemigrations", "--empty", "a"]).await;
    manage_out(&env.registry, &dir, &["makemigrations", "--empty", "b"]).await;
    manage_out(&env.registry, &dir, &["migrate"]).await;

    let out = manage_out(&env.registry, &dir, &["migrate", "0001_a"]).await;
    assert_eq!(out.trim(), "unapplied 0002_b", "{out}");
    let out = manage_out(&env.registry, &dir, &["migrate", "0002_b"]).await;
    assert_eq!(out.trim(), "applied 0002_b", "{out}");

    let err = |argv: &'static [&'static str]| {
        let (pool, dir) = (env.registry.clone(), dir.clone());
        async move {
            let mut out = Vec::new();
            rustango::migrate::manage::run_with_writer(&pool, &dir, args(argv), &mut out)
                .await
                .expect_err("refused")
                .to_string()
        }
    };
    let e = err(&["createsuperuser"]).await;
    assert!(
        e.starts_with("unknown subcommand: `createsuperuser`"),
        "{e}"
    );
    let e = err(&["migrate", "0001_a", "--dry-run"]).await;
    assert!(
        !e.contains("invalid migration") && !e.contains("v0."),
        "{e}"
    );
}

/// `--tenant <slug>` opens that tenant's pool, schema or database mode,
/// and stays out of the command's arguments.
async fn tenant_flag_opens_the_tenant_pool(backend: &Pool) {
    let mut env = Env::new(backend).await;
    rustango::testkit::migrate_framework(&env.registry)
        .await
        .expect("framework tables");
    let pid = std::process::id();
    let mut slugs = vec![
        env.tenant(&format!("mcd{pid}"), StorageMode::Database)
            .await
            .slug,
    ];
    // Schema mode is Postgres only.
    if env.registry.dialect().name() == "postgres" {
        slugs.push(
            env.tenant(&format!("mcs{pid}"), StorageMode::Schema)
                .await
                .slug,
        );
    }
    let _restore = env.only_db_vars();

    for slug in slugs {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = Arc::clone(&seen);
        Cli::new()
            .tenancy()
            .migrations_dir(env.dir())
            .command(
                "tri-tenant",
                "",
                move |ctx: CommandContext, rest: Vec<String>| {
                    let log = Arc::clone(&log);
                    async move {
                        let sql = "SELECT COUNT(*) FROM cmd_marker";
                        let tenant = ctx.tenant_pool().ok_or("no tenant pool")?;
                        rustango::sql::raw_execute_pool(tenant, sql, Vec::new()).await?;
                        let on_registry =
                            rustango::sql::raw_execute_pool(ctx.pool(), sql, Vec::new()).await;
                        let mut log = log.lock().unwrap();
                        log.push(ctx.tenant().map(|o| o.slug.clone()).unwrap_or_default());
                        log.push(format!("registry has marker: {}", on_registry.is_ok()));
                        log.extend(rest);
                        Ok(())
                    }
                },
            )
            .run_with_args(args(&["tri-tenant", "--tenant", &slug, "x"]))
            .await
            .unwrap_or_else(|e| panic!("{slug}: {e}"));
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[
                slug.clone(),
                "registry has marker: false".into(),
                "x".into()
            ],
        );

        let err = Cli::new()
            .tenancy()
            .migrations_dir(env.dir())
            .run_with_args(args(&["tri-registered", "--tenant", "no-such-tenant"]))
            .await
            .expect_err("unknown tenant");
        assert!(err.to_string().contains("no-such-tenant"), "{err}");
    }
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        manage_connects_with_only_the_db_vars,
        tenant_flag_opens_the_tenant_pool,
        check_reports_unapplied_and_unmade_migrations,
        migrate_target_names_each_direction,
    ],
}
