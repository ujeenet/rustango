//! An engine v1 release meeting engine v2 migrations: every v1 entry point
//! refuses a v2 or mixed project before it writes a file or a table, and a
//! v1 migrate refuses a database switched to v2 (boot warns and serves).

#![cfg(all(feature = "tenancy", feature = "testkit"))]

use std::path::{Path, PathBuf};

use rustango::migrate::{MigrateError, ProjectEngine};
use rustango::sql::Pool;

#[derive(Clone, Copy, Debug)]
enum Backend {
    #[cfg(feature = "postgres")]
    Postgres,
    #[cfg(feature = "mysql")]
    Mysql,
    #[cfg(feature = "sqlite")]
    Sqlite,
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
#[path = "support/scratch_db.rs"]
mod scratch_db;

#[cfg(any(feature = "postgres", feature = "mysql"))]
type DbGuard = Option<scratch_db::ScratchDb>;
#[cfg(not(any(feature = "postgres", feature = "mysql")))]
type DbGuard = Option<()>;

/// A fresh, empty database, its URL and its guard; `None` when the
/// backend's URL is unset.
async fn fresh(backend: Backend, tmp: &Path) -> Option<(Pool, String, DbGuard)> {
    let (url, guard): (String, DbGuard) = match backend {
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => {
            let db = tmp.join("engine_v2.db");
            (format!("sqlite:{}?mode=rwc", db.display()), None)
        }
        #[cfg(feature = "postgres")]
        Backend::Postgres => scratch(&std::env::var("DATABASE_URL").ok()?).await,
        #[cfg(feature = "mysql")]
        Backend::Mysql => scratch(&std::env::var("MYSQL_TEST_URL").ok()?).await,
    };
    let _ = tmp;
    Some((Pool::connect(&url).await.expect("connect"), url, guard))
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
async fn scratch(admin_url: &str) -> (String, DbGuard) {
    let db = scratch_db::ScratchDb::create(admin_url, "rustango_engine_v2").await;
    (db.url().to_owned(), Some(db))
}

/// Base tables in the database's own schema.
async fn tables(pool: &Pool) -> Vec<String> {
    let rows: Vec<(String,)> = match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => rustango::sql::sqlx::query_as(
            "SELECT table_name::text FROM information_schema.tables \
             WHERE table_schema = current_schema() ORDER BY 1",
        )
        .fetch_all(pg)
        .await
        .unwrap(),
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => rustango::sql::sqlx::query_as(
            "SELECT CAST(table_name AS CHAR) FROM information_schema.tables \
             WHERE table_schema = DATABASE() ORDER BY 1",
        )
        .fetch_all(my)
        .await
        .unwrap(),
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => rustango::sql::sqlx::query_as(
            "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY 1",
        )
        .fetch_all(sq)
        .await
        .unwrap(),
    };
    rows.into_iter().map(|(t,)| t).collect()
}

async fn exec(pool: &Pool, sql: &str) {
    rustango::sql::raw_execute_pool(pool, sql, Vec::new())
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

const V2: &str = r#"{"engine": 2, "format": 1, "id": "0001_initial", "app": "project",
  "target": "tenant", "operations": []}"#;

fn v1(name: &str) -> String {
    format!(
        r#"{{"name": "{name}", "created_at": "", "snapshot": {{"tables": []}}, "forward": []}}"#
    )
}

/// The project layouts a v1 release must refuse.
#[derive(Clone, Copy, Debug)]
enum Shape {
    /// `migrations/` holds a v2 file.
    V2,
    /// A v1 `system/` next to a v2 `migrations/`.
    MixedSystem,
    /// A v1 `migrations/` next to a v2 `blog/migrations/`.
    MixedApps,
    /// One folder with a v1 and a v2 file.
    MixedFolder,
}

/// Every shape: what a project-level entry must refuse.
const PROJECT: &[Shape] = &[
    Shape::V2,
    Shape::MixedSystem,
    Shape::MixedApps,
    Shape::MixedFolder,
];
/// What a runner over one folder sees: the per-folder guard.
const FOLDER: &[Shape] = &[Shape::V2, Shape::MixedFolder];

fn put(dir: &Path, name: &str, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(format!("{name}.json")), body).unwrap();
}

/// Lay `shape` out under `root`; returns its `migrations/` dir.
fn lay_out(root: &Path, shape: Shape) -> PathBuf {
    let dir = root.join("migrations");
    match shape {
        Shape::V2 => put(&dir, "0001_initial", V2),
        Shape::MixedSystem => {
            put(
                &root.join("system/migrations"),
                "0001_system",
                &v1("0001_system"),
            );
            put(&dir, "0001_initial", V2);
        }
        Shape::MixedApps => {
            put(&dir, "0001_initial", &v1("0001_initial"));
            put(&root.join("blog/migrations"), "0001_blog", V2);
        }
        Shape::MixedFolder => {
            put(&dir, "0001_initial", &v1("0001_initial"));
            put(&dir, "0002_v2", &V2.replace("0001_initial", "0002_v2"));
        }
    }
    dir
}

/// Every file under `root`, with its bytes, except the SQLite database.
fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                out.push((p.clone(), Vec::new()));
                walk(&p, out);
            } else if !p.to_string_lossy().contains("engine_v2.db") {
                out.push((p.clone(), std::fs::read(&p).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

/// What an entry gets: the pool, its URL and the project's dirs.
struct Ctx {
    pool: Pool,
    url: String,
    root: PathBuf,
    dir: PathBuf,
}

type Fut<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + 'a>>;
type Call = Box<dyn Fn(&Ctx) -> Fut<'_>>;

/// Each `(label, call)` on each of `shapes`, on one fresh database: every
/// call is refused with the engine error and changes no file and no table.
async fn refuses(backend: Backend, shapes: &[Shape], entries: Vec<(&str, Call)>) {
    let tmp = tempfile::tempdir().unwrap();
    let Some((pool, url, _guard)) = fresh(backend, tmp.path()).await else {
        eprintln!("skipping {backend:?}: URL unset");
        return;
    };
    for &shape in shapes {
        for (label, call) in &entries {
            let root = tmp
                .path()
                .join(format!("{shape:?}_{}", label.replace(' ', "_")));
            std::fs::create_dir_all(&root).unwrap();
            let dir = lay_out(&root, shape);
            let before = snapshot(&root);
            let ctx = Ctx {
                pool: pool.clone(),
                url: url.clone(),
                root: root.clone(),
                dir,
            };
            let err = call(&ctx)
                .await
                .err()
                .unwrap_or_else(|| panic!("{label} on {shape:?}: not refused"));
            assert!(
                err.contains("holds engine v2 migrations")
                    || err.contains("mixes engine v1 and engine v2"),
                "{label} on {shape:?}: {err}"
            );
            let after = snapshot(&root);
            let names = |s: &[(PathBuf, Vec<u8>)]| -> Vec<PathBuf> {
                s.iter().map(|(p, _)| p.clone()).collect()
            };
            assert_eq!(
                names(&after),
                names(&before),
                "{label} on {shape:?}: files written"
            );
            assert!(after == before, "{label} on {shape:?}: a file changed");
            assert_eq!(
                tables(&pool).await,
                Vec::<String>::new(),
                "{label} on {shape:?}: database written"
            );
        }
    }
}

fn call<F>(f: F) -> Call
where
    F: for<'a> Fn(&'a Ctx) -> Fut<'a> + 'static,
{
    Box::new(f)
}

/// The single-database `manage` verb `args`.
fn cli(args: &'static [&'static str]) -> Call {
    call(move |c| {
        Box::pin(async move {
            let args = args.iter().map(|a| (*a).to_owned());
            rustango::migrate::manage::run_with_writer(&c.pool, &c.dir, args, &mut Vec::new())
                .await
                .map_err(|e| e.to_string())
        })
    })
}

/// `$body` with `$pools` bound to the typed `TenantPools` of `$pool`.
macro_rules! with_pools {
    ($pool:expr, |$pools:ident| $body:expr) => {
        match $pool {
            #[cfg(feature = "postgres")]
            Pool::Postgres(p) => {
                let $pools = rustango::tenancy::TenantPools::new(p.clone());
                $body.await.map(drop).map_err(|e| e.to_string())
            }
            #[cfg(feature = "mysql")]
            Pool::Mysql(p) => {
                let $pools = rustango::tenancy::TenantPools::new(p.clone());
                $body.await.map(drop).map_err(|e| e.to_string())
            }
            #[cfg(feature = "sqlite")]
            Pool::Sqlite(p) => {
                let $pools = rustango::tenancy::TenantPools::new(p.clone());
                $body.await.map(drop).map_err(|e| e.to_string())
            }
        }
    };
}

/// The tenancy `manage` verb `args`.
fn tenancy_cli(args: &'static [&'static str]) -> Call {
    call(move |c| {
        Box::pin(async move {
            let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
            with_pools!(&c.pool, |pools| rustango::tenancy::manage::run_with_writer(
                &pools,
                &c.url,
                &c.dir,
                args,
                &mut Vec::new()
            ))
        })
    })
}

fn err_string<T>(r: Result<T, MigrateError>) -> Result<(), String> {
    r.map(drop).map_err(|e| e.to_string())
}

async fn v1_refuses_v2_cli_migrate(backend: Backend) {
    refuses(
        backend,
        PROJECT,
        vec![
            ("migrate", cli(&["migrate"])),
            ("migrate target", cli(&["migrate", "0001_initial"])),
            ("migrate zero", cli(&["migrate", "zero"])),
            ("migrate dry-run", cli(&["migrate", "--dry-run"])),
            ("migrate squash", cli(&["migrate", "--squash"])),
        ],
    )
    .await;
}

async fn v1_refuses_v2_cli_downgrade(backend: Backend) {
    refuses(backend, PROJECT, vec![("downgrade", cli(&["downgrade"]))]).await;
}

async fn v1_refuses_v2_cli_makemigrations(backend: Backend) {
    refuses(
        backend,
        PROJECT,
        vec![
            ("makemigrations", cli(&["makemigrations"])),
            ("empty", cli(&["makemigrations", "--empty", "seed"])),
            ("merge", cli(&["makemigrations", "--merge"])),
            ("app", cli(&["makemigrations", "--app", "blog"])),
            ("scope", cli(&["makemigrations", "--scope", "tenant"])),
        ],
    )
    .await;
}

async fn v1_refuses_v2_cli_showmigrations(backend: Backend) {
    refuses(
        backend,
        PROJECT,
        vec![
            ("showmigrations", cli(&["showmigrations"])),
            ("status", cli(&["status"])),
        ],
    )
    .await;
}

async fn v1_refuses_v2_cli_sqlmigrate(backend: Backend) {
    refuses(
        backend,
        PROJECT,
        vec![("sqlmigrate", cli(&["sqlmigrate", "0001_initial"]))],
    )
    .await;
}

async fn v1_refuses_v2_cli_check(backend: Backend) {
    refuses(backend, PROJECT, vec![("check", cli(&["check"]))]).await;
}

async fn v1_refuses_v2_cli_forget_pending(backend: Backend) {
    refuses(
        backend,
        PROJECT,
        vec![("forget-pending", cli(&["forget-pending", "0001_initial"]))],
    )
    .await;
}

async fn v1_refuses_v2_cli_add_data_op(backend: Backend) {
    refuses(
        backend,
        PROJECT,
        vec![
            (
                "add-data-op to",
                cli(&["add-data-op", "--to", "0001_initial", "--sql", "SELECT 1"]),
            ),
            (
                "add-data-op name",
                cli(&["add-data-op", "--name", "seed", "--sql", "SELECT 1"]),
            ),
        ],
    )
    .await;
}

async fn v1_refuses_v2_tenancy_migrate(backend: Backend) {
    refuses(
        backend,
        PROJECT,
        vec![
            ("migrate", tenancy_cli(&["migrate"])),
            ("fake", tenancy_cli(&["migrate", "--fake", "0001_initial"])),
            ("system", tenancy_cli(&["migrate", "--system"])),
            ("all-tenants", tenancy_cli(&["migrate", "--all-tenants"])),
            ("migrate-tenants", tenancy_cli(&["migrate-tenants"])),
            ("migrate-registry", tenancy_cli(&["migrate-registry"])),
        ],
    )
    .await;
}

async fn v1_refuses_v2_tenancy_verbs(backend: Backend) {
    refuses(
        backend,
        PROJECT,
        vec![
            ("downgrade", tenancy_cli(&["downgrade"])),
            ("check", tenancy_cli(&["check"])),
            (
                "forget-pending",
                tenancy_cli(&["forget-pending", "0001_initial"]),
            ),
            ("showmigrations", tenancy_cli(&["showmigrations"])),
            ("status", tenancy_cli(&["status"])),
            ("init-tenancy", tenancy_cli(&["init-tenancy"])),
            ("makemigrations", tenancy_cli(&["makemigrations"])),
            ("sqlmigrate", tenancy_cli(&["sqlmigrate", "0001_initial"])),
        ],
    )
    .await;
}

async fn v1_refuses_v2_provisioning(backend: Backend) {
    refuses(
        backend,
        PROJECT,
        vec![
            (
                "migrate_one_tenant",
                call(|c| {
                    Box::pin(async move {
                        let org = rustango::testkit::org_with(|o| o.slug = "t1".into());
                        with_pools!(&c.pool, |pools| rustango::tenancy::migrate_one_tenant(
                            &pools, &org, &c.dir, &c.url, None
                        ))
                    })
                }),
            ),
            (
                "migrate_in_run",
                call(|c| {
                    Box::pin(async move {
                        with_pools!(&c.pool, |pools| {
                            rustango::tenancy::migrate_run::migrate_in_run(
                                &pools, &c.dir, &c.url, 1, None,
                            )
                        })
                    })
                }),
            ),
        ],
    )
    .await;
}

async fn v1_refuses_v2_tenancy_pub_apis(backend: Backend) {
    refuses(
        backend,
        PROJECT,
        vec![
            (
                "migrate_registry",
                call(|c| {
                    Box::pin(async move {
                        with_pools!(&c.pool, |pools| rustango::tenancy::migrate_registry(
                            &pools, &c.dir
                        ))
                    })
                }),
            ),
            (
                "migrate_registry_pool",
                call(|c| {
                    Box::pin(async move {
                        rustango::tenancy::migrate_registry_pool(&c.pool, &c.dir)
                            .await
                            .map(drop)
                            .map_err(|e| e.to_string())
                    })
                }),
            ),
            (
                "migrate_tenants_dyn",
                call(|c| {
                    Box::pin(async move {
                        with_pools!(&c.pool, |pools| rustango::tenancy::migrate_tenants_dyn(
                            &pools, &c.dir, &c.url
                        ))
                    })
                }),
            ),
            (
                "migrate_tenants_db",
                call(|c| {
                    Box::pin(async move {
                        with_pools!(&c.pool, |pools| rustango::tenancy::migrate_tenants_db(
                            &pools, &c.dir, &c.url
                        ))
                    })
                }),
            ),
        ],
    )
    .await;
}

/// The tenancy server's boot: `Builder::migrate(project_root)`.
async fn v1_refuses_v2_builder_migrate(backend: Backend) {
    refuses(
        backend,
        PROJECT,
        vec![(
            "Builder::migrate",
            call(|c| Box::pin(async move { builder_migrate(&c.pool, &c.url, &c.root).await })),
        )],
    )
    .await;
}

async fn builder_migrate(pool: &Pool, url: &str, root: &Path) -> Result<(), String> {
    macro_rules! run {
        ($p:expr) => {
            rustango::server::Builder::from_pool($p.clone(), url, "localhost")
                .migrate(root)
                .await
                .map(drop)
                .map_err(|e| e.to_string())
        };
    }
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(p) => run!(p),
        #[cfg(feature = "mysql")]
        Pool::Mysql(p) => run!(p),
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(p) => run!(p),
    }
}

async fn v1_refuses_v2_make_apis(backend: Backend) {
    use rustango::migrate::manage::{make_empty, make_empty_in_scope};
    refuses(
        backend,
        PROJECT,
        vec![
            (
                "make_migrations",
                call(|c| {
                    Box::pin(
                        async move { err_string(rustango::migrate::make_migrations(&c.dir, None)) },
                    )
                }),
            ),
            (
                "make_migrations_for_scope",
                call(|c| {
                    Box::pin(async move {
                        err_string(rustango::migrate::make::make_migrations_for_scope(
                            &c.dir,
                            rustango::core::ModelScope::Tenant,
                            None,
                        ))
                    })
                }),
            ),
            (
                "make_migrations_for_app",
                call(|c| {
                    Box::pin(async move {
                        err_string(rustango::migrate::make_migrations_for_app(
                            &c.root, "shop", None,
                        ))
                    })
                }),
            ),
            (
                "make_merge_migration",
                call(|c| {
                    Box::pin(async move {
                        err_string(rustango::migrate::make::make_merge_migration(&c.dir))
                    })
                }),
            ),
            (
                "make_migrations_system",
                call(|c| {
                    Box::pin(async move {
                        err_string(rustango::migrate::make_migrations_system(
                            &c.root,
                            rustango::core::ModelScope::Tenant,
                            None,
                        ))
                    })
                }),
            ),
            (
                "make_empty",
                call(|c| Box::pin(async move { err_string(make_empty(&c.dir, "seed")) })),
            ),
            (
                "make_empty_in_scope",
                call(|c| {
                    Box::pin(async move {
                        err_string(make_empty_in_scope(
                            &c.dir,
                            "seed",
                            rustango::migrate::MigrationScope::Tenant,
                        ))
                    })
                }),
            ),
            (
                "sqlmigrate_one",
                call(|c| {
                    Box::pin(async move {
                        err_string(rustango::migrate::sqlmigrate_one(
                            &c.dir,
                            "0001_initial",
                            c.pool.dialect(),
                        ))
                    })
                }),
            ),
        ],
    )
    .await;
}

async fn v1_refuses_v2_data_op_apis(backend: Backend) {
    refuses(
        backend,
        PROJECT,
        vec![
            (
                "make_data_migration",
                call(|c| {
                    Box::pin(async move {
                        err_string(rustango::migrate::manage::make_data_migration(
                            &c.dir, "seed", "SELECT 1", None,
                        ))
                    })
                }),
            ),
            (
                "append_data_op",
                call(|c| {
                    Box::pin(async move {
                        err_string(rustango::migrate::manage::append_data_op(
                            &c.dir,
                            "0001_initial",
                            "SELECT 1",
                            None,
                        ))
                    })
                }),
            ),
        ],
    )
    .await;
}

/// The runner over one folder relies on the per-folder guard.
async fn v1_refuses_v2_runner_apis(backend: Backend) {
    use rustango::migrate as m;
    let b = || m::Builder::new().ledger("ev2_ledger");
    refuses(
        backend,
        FOLDER,
        vec![
            (
                "migrate_pool",
                call(|c| {
                    Box::pin(async move { err_string(m::migrate_pool(&c.pool, &c.dir).await) })
                }),
            ),
            (
                "migrate_pool_with_ledger",
                call(|c| {
                    Box::pin(async move {
                        err_string(m::migrate_pool_with_ledger(&c.pool, &c.dir, "ev2").await)
                    })
                }),
            ),
            (
                "migrate_to_pool",
                call(|c| {
                    Box::pin(async move {
                        err_string(m::migrate_to_pool(&c.pool, &c.dir, "0001_initial").await)
                    })
                }),
            ),
            (
                "downgrade_pool",
                call(|c| {
                    Box::pin(async move { err_string(m::downgrade_pool(&c.pool, &c.dir, 1).await) })
                }),
            ),
            (
                "unapply_pool",
                call(|c| {
                    Box::pin(async move {
                        err_string(m::unapply_pool(&c.pool, &c.dir, "0001_initial").await)
                    })
                }),
            ),
            (
                "unapply_force_pool",
                call(|c| {
                    Box::pin(async move {
                        err_string(m::unapply_force_pool(&c.pool, &c.dir, "0001_initial").await)
                    })
                }),
            ),
            (
                "migrate_dry_run_pool",
                call(|c| {
                    Box::pin(
                        async move { err_string(m::migrate_dry_run_pool(&c.pool, &c.dir).await) },
                    )
                }),
            ),
            (
                "migrate_embedded_pool",
                call(|c| {
                    Box::pin(async move {
                        let entries: Vec<(String, String)> = std::fs::read_dir(&c.dir)
                            .unwrap()
                            .map(|e| {
                                let p = e.unwrap().path();
                                let stem = p.file_stem().unwrap().to_string_lossy().into_owned();
                                (stem, std::fs::read_to_string(&p).unwrap())
                            })
                            .collect();
                        let slice: Vec<(&str, &str)> = entries
                            .iter()
                            .map(|(a, b)| (a.as_str(), b.as_str()))
                            .collect();
                        err_string(m::migrate_embedded_pool(&c.pool, &slice).await)
                    })
                }),
            ),
            (
                "Builder::migrate",
                call(move |c| {
                    Box::pin(async move { err_string(b().migrate(&c.pool, &c.dir).await) })
                }),
            ),
            (
                "Builder::migrate_to",
                call(move |c| {
                    Box::pin(async move {
                        err_string(b().migrate_to(&c.pool, &c.dir, "0001_initial").await)
                    })
                }),
            ),
            (
                "Builder::migrate_dry_run",
                call(move |c| {
                    Box::pin(async move { err_string(b().migrate_dry_run(&c.pool, &c.dir).await) })
                }),
            ),
            (
                "Builder::downgrade",
                call(move |c| {
                    Box::pin(async move { err_string(b().downgrade(&c.pool, &c.dir, 1).await) })
                }),
            ),
            (
                "Builder::unapply",
                call(move |c| {
                    Box::pin(async move {
                        err_string(b().unapply(&c.pool, &c.dir, "0001_initial").await)
                    })
                }),
            ),
            (
                "Builder::unapply_force",
                call(move |c| {
                    Box::pin(async move {
                        err_string(b().unapply_force(&c.pool, &c.dir, "0001_initial").await)
                    })
                }),
            ),
        ],
    )
    .await;
}

/// `detect` sees what a single folder cannot: a v2 app folder beside v1 ones.
#[test]
fn detect_sees_across_folders() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = lay_out(tmp.path(), Shape::MixedApps);
    let blog = tmp.path().join("blog/migrations");
    assert!(rustango::migrate::file::list_dir(&dir).is_ok());
    let found = ProjectEngine::detect([&dir, &blog]).unwrap();
    assert!(matches!(found, ProjectEngine::Mixed { .. }), "{found:?}");
    assert!(matches!(
        found.require_v1(),
        Err(MigrateError::MixedEngine { .. })
    ));
    assert_eq!(ProjectEngine::detect([&dir]).unwrap(), ProjectEngine::V1);
}

// ---- a database switched to engine v2 ----

/// CLI `migrate` refuses a database holding the v2 ledger and writes no v1 ledger.
async fn v1_migrate_refuses_switched_db(backend: Backend) {
    let tmp = tempfile::tempdir().unwrap();
    let Some((pool, _, _guard)) = fresh(backend, tmp.path()).await else {
        return;
    };
    exec(
        &pool,
        "CREATE TABLE __rustango_migrations_v2 (id VARCHAR(64))",
    )
    .await;
    let dir = tmp.path().join("migrations");
    put(&dir, "0001_initial", &v1("0001_initial"));
    let err = rustango::migrate::manage::run_with_writer(
        &pool,
        &dir,
        ["migrate".to_owned()],
        &mut Vec::new(),
    )
    .await
    .expect_err("switched database");
    assert!(matches!(err, MigrateError::SwitchedToV2 { .. }), "{err}");
    assert!(
        err.to_string().contains("__rustango_migrations_v2"),
        "{err}"
    );
    let t = tables(&pool).await;
    assert!(
        !t.iter().any(|t| t == rustango::migrate::LEDGER_TABLE),
        "{t:?}"
    );

    // Every v1 ledger write starts at the same check, custom ledgers included.
    exec(&pool, "CREATE TABLE ev2_custom_v2 (id VARCHAR(64))").await;
    let err = rustango::migrate::migrate_pool_with_ledger(&pool, &dir, "ev2_custom")
        .await
        .expect_err("custom ledger");
    assert!(matches!(err, MigrateError::SwitchedToV2 { .. }), "{err}");
    let err = rustango::migrate::ensure_ledger_pool(&pool)
        .await
        .expect_err("ensure_ledger_pool");
    assert!(matches!(err, MigrateError::SwitchedToV2 { .. }), "{err}");
    let err = rustango::migrate::ensure_ledger_pool_with_ledger(&pool, "ev2_custom")
        .await
        .expect_err("ensure_ledger_pool_with_ledger");
    assert!(matches!(err, MigrateError::SwitchedToV2 { .. }), "{err}");
    // A ledger without a v2 twin still works.
    rustango::migrate::ensure_ledger_pool_with_ledger(&pool, "ev2_other")
        .await
        .expect("unswitched ledger");
}

/// Tenancy boot on a registry switched to v2 warns and serves.
async fn v1_boot_serves_on_switched_registry(backend: Backend) {
    let tmp = tempfile::tempdir().unwrap();
    let Some((pool, url, _guard)) = fresh(backend, tmp.path()).await else {
        return;
    };
    exec(
        &pool,
        "CREATE TABLE __rustango_migrations_v2 (id VARCHAR(64))",
    )
    .await;
    let root = tmp.path().join("project");
    put(
        &root.join("migrations"),
        "0001_initial",
        &v1("0001_initial"),
    );
    builder_migrate(&pool, &url, &root)
        .await
        .expect("boot serves");
    let t = tables(&pool).await;
    assert!(
        !t.iter().any(|t| t == rustango::migrate::LEDGER_TABLE),
        "{t:?}"
    );
}

macro_rules! per_backend {
    ($($name:ident),* $(,)?) => {
        #[cfg(feature = "postgres")]
        mod postgres {
            $(#[tokio::test] async fn $name() { super::$name(super::Backend::Postgres).await })*
        }
        #[cfg(feature = "mysql")]
        mod mysql {
            $(#[tokio::test] async fn $name() { super::$name(super::Backend::Mysql).await })*
        }
        #[cfg(feature = "sqlite")]
        mod sqlite {
            $(#[tokio::test] async fn $name() { super::$name(super::Backend::Sqlite).await })*
        }
    };
}

per_backend!(
    v1_refuses_v2_cli_migrate,
    v1_refuses_v2_cli_downgrade,
    v1_refuses_v2_cli_makemigrations,
    v1_refuses_v2_cli_showmigrations,
    v1_refuses_v2_cli_sqlmigrate,
    v1_refuses_v2_cli_check,
    v1_refuses_v2_cli_forget_pending,
    v1_refuses_v2_cli_add_data_op,
    v1_refuses_v2_tenancy_migrate,
    v1_refuses_v2_tenancy_verbs,
    v1_refuses_v2_provisioning,
    v1_refuses_v2_tenancy_pub_apis,
    v1_refuses_v2_builder_migrate,
    v1_refuses_v2_make_apis,
    v1_refuses_v2_data_op_apis,
    v1_refuses_v2_runner_apis,
    v1_migrate_refuses_switched_db,
    v1_boot_serves_on_switched_registry,
);
