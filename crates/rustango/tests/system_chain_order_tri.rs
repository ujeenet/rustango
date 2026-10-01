//! A project whose own `0001` creates framework tables (a pre-system-chain
//! scaffold) must migrate a fresh database on every backend (#2055, #2052).
//! SQLite does not check FK targets at `CREATE TABLE`, so PG and MySQL matter.

#![cfg(all(feature = "admin", feature = "tenancy", feature = "testkit"))]

use std::path::{Path, PathBuf};

use rustango::core::ModelScope;
use rustango::sql::Pool;
use serde_json::{json, Value};

#[derive(Clone, Copy)]
enum Backend {
    #[cfg(feature = "postgres")]
    Postgres,
    #[cfg(feature = "mysql")]
    Mysql,
    #[cfg(feature = "sqlite")]
    Sqlite,
}

/// A fresh, empty database and its URL; `None` when the backend's URL is unset.
async fn fresh(backend: Backend, tmp: &Path, tag: &str) -> Option<(Pool, String)> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db = format!("rustango_syschain_{tag}_{}_{nanos}", std::process::id());
    let url = match backend {
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => format!("sqlite:{}?mode=rwc", tmp.join(format!("{db}.db")).display()),
        #[cfg(feature = "postgres")]
        Backend::Postgres => create_db(&std::env::var("DATABASE_URL").ok()?, &db).await,
        #[cfg(feature = "mysql")]
        Backend::Mysql => create_db(&std::env::var("MYSQL_TEST_URL").ok()?, &db).await,
    };
    let _ = tmp;
    Some((Pool::connect(&url).await.expect("connect"), url))
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
async fn create_db(admin_url: &str, db: &str) -> String {
    let admin = Pool::connect(admin_url).await.expect("connect admin");
    rustango::sql::raw_execute_pool(&admin, &format!("CREATE DATABASE {db}"), Vec::new())
        .await
        .expect("create database");
    let (base, _) = admin_url.rsplit_once('/').unwrap();
    format!("{base}/{db}")
}

async fn has_column(pool: &Pool, table: &str, column: &str) -> bool {
    let n: i64 = match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => rustango::sql::sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = $1 AND column_name = $2",
        )
        .bind(table)
        .bind(column)
        .fetch_one(pg)
        .await
        .unwrap(),
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => rustango::sql::sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns \
             WHERE table_schema = DATABASE() AND table_name = ? AND column_name = ?",
        )
        .bind(table)
        .bind(column)
        .fetch_one(my)
        .await
        .unwrap(),
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => rustango::sql::sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info(?) WHERE name = ?",
        )
        .bind(table)
        .bind(column)
        .fetch_one(sq)
        .await
        .unwrap(),
    };
    n == 1
}

async fn has_table(pool: &Pool, table: &str) -> bool {
    rustango::sql::raw_execute_pool(
        pool,
        &format!("SELECT 1 FROM {table} WHERE 1 = 0"),
        Vec::new(),
    )
    .await
    .is_ok()
}

async fn manage_migrate(pool: &Pool, dir: &Path) -> Result<(), rustango::migrate::MigrateError> {
    std::fs::create_dir_all(dir).unwrap();
    let mut out = Vec::new();
    rustango::migrate::manage::run_with_writer(pool, dir, ["migrate".to_owned()], &mut out).await
}

/// The tenant-scope system file in `sys` (the one with no `scope` key).
fn tenant_file(sys: &Path) -> PathBuf {
    std::fs::read_dir(sys)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| !std::fs::read_to_string(p).unwrap().contains("\"scope\""))
        .expect("tenant file")
}

fn read(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

/// `table`'s snapshot in today's tenant chain, minus `drop_column`.
fn current_table(table: &str, drop_column: Option<&str>) -> Value {
    let tmp = tempfile::tempdir().unwrap();
    rustango::migrate::make_migrations_system(tmp.path(), ModelScope::Tenant, None).unwrap();
    let mig = read(&tenant_file(&tmp.path().join("system/migrations")));
    let mut t = mig["snapshot"]["tables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == table)
        .unwrap_or_else(|| panic!("{table} in the tenant chain"))
        .clone();
    if let Some(c) = drop_column {
        t["fields"]
            .as_array_mut()
            .unwrap()
            .retain(|f| f["column"] != c);
    }
    t
}

/// A project `0001_initial` that creates `table` with the given snapshot.
fn project_initial(dir: &Path, table: Value) {
    let name = table["name"].clone();
    let mig = json!({
        "name": "0001_initial",
        "created_at": "2026-01-01T00:00:00Z",
        "prev": null,
        "snapshot": { "tables": [table] },
        "forward": [{ "schema": { "CreateTable": name } }],
    });
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("0001_initial.json"), mig.to_string()).unwrap();
}

/// #2055 — a later system step creates tables with an FK to the project's
/// `rustango_users` and adds a column elsewhere; on PG/MySQL it must wait.
async fn fk_to_project_table(backend: Backend) {
    let tmp = tempfile::tempdir().unwrap();
    let Some((pool, _)) = fresh(backend, tmp.path(), "fk").await else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let root = tmp.path().join("app");
    for scope in [ModelScope::Registry, ModelScope::Tenant] {
        rustango::migrate::make_migrations_system(&root, scope, None).unwrap();
    }
    // Age the tenant chain: no users (nor what FKs it), no sessions_revoked_at.
    let (users, admin, column) = (
        "rustango_users",
        "rustango_admin_users",
        "sessions_revoked_at",
    );
    let path = tenant_file(&root.join("system/migrations"));
    let mut mig = read(&path);
    let gone: Vec<String> = mig["snapshot"]["tables"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| {
            t["name"] == users
                || t["fields"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|f| f["fk"]["to"] == users)
        })
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect();
    assert!(gone.len() > 1, "some system table FKs {users}: {gone:?}");
    let is_gone = |v: &Value| v.as_str().is_some_and(|t| gone.iter().any(|g| g == t));
    let snap = &mut mig["snapshot"];
    snap["tables"]
        .as_array_mut()
        .unwrap()
        .retain(|t| !is_gone(&t["name"]));
    for key in ["indexes", "m2m_tables"] {
        if let Some(list) = snap.get_mut(key).and_then(Value::as_array_mut) {
            list.retain(|i| {
                !is_gone(&i["table"])
                    && !is_gone(&i["through"])
                    && !is_gone(&i["src_table"])
                    && !is_gone(&i["dst_table"])
            });
        }
    }
    let admin_t = snap["tables"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|t| t["name"] == admin)
        .unwrap();
    admin_t["fields"]
        .as_array_mut()
        .unwrap()
        .retain(|f| f["column"] != column);
    mig["forward"].as_array_mut().unwrap().retain(|op| {
        let s = &op["schema"];
        !(is_gone(&s["CreateTable"])
            || is_gone(&s["CreateIndex"]["table"])
            || is_gone(&s["CreateM2MTable"]["through"]))
    });
    std::fs::write(&path, mig.to_string()).unwrap();
    let step = rustango::migrate::make_migrations_system(&root, ModelScope::Tenant, None)
        .unwrap()
        .expect("the catch-up step");
    let ops = format!("{:?}", step.forward);
    assert!(ops.contains(users) && ops.contains(column), "{ops}");

    let dir = root.join("migrations");
    project_initial(&dir, current_table(users, None));
    manage_migrate(&pool, &dir)
        .await
        .expect("fresh database migrates");
    for t in &gone {
        assert!(has_table(&pool, t).await, "{t} missing");
    }
    assert!(has_column(&pool, admin, column).await);
    manage_migrate(&pool, &dir).await.expect("second run");
}

/// #2052 — an old scaffold's first `makemigrations` writes `system/` as one
/// step from today's models; the project's older `rustango_admin_users`
/// must still get the new column when another process migrates.
async fn single_step_chain_converges_project_table(backend: Backend) {
    let tmp = tempfile::tempdir().unwrap();
    let Some((pool, _)) = fresh(backend, tmp.path(), "regen").await else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let (table, column) = ("rustango_admin_users", "sessions_revoked_at");
    let root = tmp.path().join("app");
    for scope in [ModelScope::Registry, ModelScope::Tenant] {
        rustango::migrate::make_migrations_system(&root, scope, None).unwrap();
    }
    let dir = root.join("migrations");
    project_initial(&dir, current_table(table, Some(column)));
    manage_migrate(&pool, &dir)
        .await
        .expect("fresh database migrates");
    assert!(has_column(&pool, table, column).await, "{table}.{column}");
    manage_migrate(&pool, &dir).await.expect("second run");
}

/// #2052 on the tenancy runner: a tenant project `0001` that creates an older
/// `rustango_users` must neither collide nor miss the newer column.
async fn tenant_project_table_converges(backend: Backend) {
    let tmp = tempfile::tempdir().unwrap();
    let Some((registry, _)) = fresh(backend, tmp.path(), "reg").await else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let (_tenant, tenant_url) = fresh(backend, tmp.path(), "ten").await.unwrap();
    let boot = tmp.path().join("boot/migrations");
    std::fs::create_dir_all(&boot).unwrap();
    rustango::tenancy::migrate_registry_pool(&registry, &boot)
        .await
        .expect("registry tables");
    let mut org = rustango::tenancy::Org {
        slug: "t1".into(),
        display_name: "t1".into(),
        backend_kind: match backend {
            #[cfg(feature = "postgres")]
            Backend::Postgres => "postgres",
            #[cfg(feature = "mysql")]
            Backend::Mysql => "mysql",
            #[cfg(feature = "sqlite")]
            Backend::Sqlite => "sqlite",
        }
        .into(),
        database_url: Some(tenant_url.clone()),
        ..rustango::testkit::org()
    };
    org.insert_pool(&registry).await.unwrap();

    let (table, column) = ("rustango_users", "password_changed_at");
    let dir = tmp.path().join("app/migrations");
    project_initial(&dir, current_table(table, Some(column)));
    rustango::tenancy::migrate_registry_pool(&registry, &dir)
        .await
        .expect("registry");
    let report = match &registry {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let pools = rustango::tenancy::TenantPools::new(pg.clone());
            rustango::tenancy::migrate_tenants_db(&pools, &dir, "").await
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let pools = rustango::tenancy::TenantPools::new(my.clone());
            rustango::tenancy::migrate_tenants_db(&pools, &dir, "").await
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let pools = rustango::tenancy::TenantPools::new(sq.clone());
            rustango::tenancy::migrate_tenants_db(&pools, &dir, "").await
        }
    }
    .expect("tenants");
    assert!(report.all_ok(), "{report:?}");
    let tenant = Pool::connect(&tenant_url).await.unwrap();
    assert!(has_column(&tenant, table, column).await, "{table}.{column}");
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
    fk_to_project_table,
    single_step_chain_converges_project_table,
    tenant_project_table_converges,
);
