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

async fn has_index(pool: &Pool, name: &str) -> bool {
    let n: i64 = match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => rustango::sql::sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_indexes WHERE schemaname = current_schema() AND indexname = $1",
        )
        .bind(name)
        .fetch_one(pg)
        .await
        .unwrap(),
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => rustango::sql::sqlx::query_scalar(
            "SELECT COUNT(DISTINCT index_name) FROM information_schema.statistics \
             WHERE table_schema = DATABASE() AND index_name = ?",
        )
        .bind(name)
        .fetch_one(my)
        .await
        .unwrap(),
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => rustango::sql::sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?",
        )
        .bind(name)
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
    write_step(
        dir,
        "0001_initial",
        None,
        json!({ "tables": [table] }),
        vec![json!({ "CreateTable": name })],
    );
}

/// Write migration `name` with schema ops `ops` to `dir`.
fn write_step(dir: &Path, name: &str, prev: Option<&str>, snapshot: Value, ops: Vec<Value>) {
    let forward: Vec<Value> = ops.into_iter().map(|op| json!({ "schema": op })).collect();
    let mig = json!({
        "name": name,
        "created_at": "2026-01-01T00:00:00Z",
        "prev": prev,
        "snapshot": snapshot,
        "forward": forward,
    });
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(format!("{name}.json")), mig.to_string()).unwrap();
}

/// Write migration `name` with `forward` ops as given to `dir`.
fn write_raw(dir: &Path, name: &str, forward: Vec<Value>) {
    let mig = json!({
        "name": name,
        "created_at": "2026-01-01T00:00:00Z",
        "prev": null,
        "atomic": false,
        "snapshot": { "tables": [] },
        "forward": forward,
    });
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(format!("{name}.json")), mig.to_string()).unwrap();
}

/// Both scopes of today's system chain under `root`.
fn system_chain(root: &Path) {
    for scope in [ModelScope::Registry, ModelScope::Tenant] {
        rustango::migrate::make_migrations_system(root, scope, None).unwrap();
    }
}

/// Finding 1 — a pending project AddColumn of a column the system snapshot
/// also has runs before the framework converges the table.
async fn pending_project_add_column(backend: Backend) {
    let tmp = tempfile::tempdir().unwrap();
    let Some((pool, _)) = fresh(backend, tmp.path(), "addcol").await else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let root = tmp.path().join("app");
    system_chain(&root);
    let (table, column) = ("rustango_admin_users", "sessions_revoked_at");
    let dir = root.join("migrations");
    project_initial(&dir, current_table(table, Some(column)));
    // An older release applied the project chain alone.
    rustango::migrate::migrate_pool(&pool, &dir).await.unwrap();
    assert!(!has_column(&pool, table, column).await);
    write_step(
        &dir,
        "0002_add",
        Some("0001_initial"),
        json!({ "tables": [current_table(table, None)] }),
        vec![json!({ "AddColumn": { "table": table, "column": column } })],
    );
    manage_migrate(&pool, &dir).await.expect("first run");
    assert!(has_column(&pool, table, column).await);
    manage_migrate(&pool, &dir).await.expect("second run");
}

/// Finding 2 — system step N adds a column to a project-owned table and
/// N+1 alters it; the alter must find the column.
async fn alter_after_add_on_owned(backend: Backend) {
    let tmp = tempfile::tempdir().unwrap();
    let Some((pool, _)) = fresh(backend, tmp.path(), "alter").await else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let root = tmp.path().join("app");
    system_chain(&root);
    let table = "rustango_admin_users";
    let sys = root.join("system/migrations");
    let first = read(&tenant_file(&sys));
    let with_field = |field: Value| {
        let mut snap = first["snapshot"].clone();
        let t = snap["tables"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|t| t["name"] == table)
            .unwrap();
        t["fields"].as_array_mut().unwrap().push(field);
        snap
    };
    let nick = |column: &str, len: u32| {
        json!({ "name": column, "column": column, "ty": "string",
                "nullable": true, "primary_key": false, "max_length": len })
    };
    write_step(
        &sys,
        "9001_nick",
        first["name"].as_str(),
        with_field(nick("nick", 50)),
        vec![json!({ "AddColumn": { "table": table, "column": "nick" } })],
    );
    // MySQL has no AlterColumnMaxLength yet, and SQLite renders it as nothing.
    let (after, op) = match backend {
        #[cfg(feature = "postgres")]
        Backend::Postgres => (
            nick("nick", 100),
            json!({ "AlterColumnMaxLength": { "table": table, "column": "nick", "from": 50, "to": 100 } }),
        ),
        #[allow(unreachable_patterns)]
        _ => (
            nick("nick2", 50),
            json!({ "RenameColumn": { "table": table, "old_column": "nick", "new_column": "nick2" } }),
        ),
    };
    write_step(
        &sys,
        "9002_nick",
        Some("9001_nick"),
        with_field(after),
        vec![op],
    );
    let dir = root.join("migrations");
    project_initial(&dir, current_table(table, None));
    manage_migrate(&pool, &dir).await.expect("first run");
    manage_migrate(&pool, &dir).await.expect("second run");
}

/// Take `tables` and `index` out of the first tenant step; today's catch-up
/// step then adds them back. Returns that step's debug-printed ops.
fn age_tenant_chain(root: &Path, tables: &[String], index: Option<&str>) -> String {
    let path = tenant_file(&root.join("system/migrations"));
    let mut mig = read(&path);
    let is_gone = |v: &Value| v.as_str().is_some_and(|t| tables.iter().any(|g| g == t));
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
                    && index.is_none_or(|n| i["name"] != n)
            });
        }
    }
    mig["forward"].as_array_mut().unwrap().retain(|op| {
        let s = &op["schema"];
        !(is_gone(&s["CreateTable"])
            || is_gone(&s["CreateIndex"]["table"])
            || is_gone(&s["CreateM2MTable"]["through"])
            || index.is_some_and(|n| s["CreateIndex"]["name"] == n))
    });
    std::fs::write(&path, mig.to_string()).unwrap();
    let step = rustango::migrate::make_migrations_system(root, ModelScope::Tenant, None)
        .unwrap()
        .expect("the catch-up step");
    format!("{:?}", step.forward)
}

/// A later system step's index on a project-owned table is created.
async fn later_index_on_owned(backend: Backend) {
    let tmp = tempfile::tempdir().unwrap();
    let Some((pool, _)) = fresh(backend, tmp.path(), "index").await else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let root = tmp.path().join("app");
    system_chain(&root);
    let first = read(&tenant_file(&root.join("system/migrations")));
    let idx = first["snapshot"]["indexes"][0].clone();
    let (table, index) = (
        idx["table"].as_str().unwrap(),
        idx["name"].as_str().unwrap(),
    );
    let ops = age_tenant_chain(&root, &[], Some(index));
    assert!(ops.contains(index), "{ops}");
    let dir = root.join("migrations");
    project_initial(&dir, current_table(table, None));
    manage_migrate(&pool, &dir).await.expect("first run");
    assert!(has_index(&pool, index).await, "{table}.{index} missing");
    manage_migrate(&pool, &dir).await.expect("second run");
}

/// A callback that migrates again while the outer migrate holds the lock.
fn nested_migrate(pool: Pool) -> rustango::migrate::callbacks::MigrationCallbackFut {
    Box::pin(async move {
        let dir = tempfile::tempdir().unwrap();
        rustango::migrate::migrate_pool(&pool, dir.path())
            .await
            .map(|_| ())
    })
}

rustango::register_migration_callback!("syschain_nested_migrate", nested_migrate);

/// A nested migrate lock is an error, not a hang.
async fn nested_lock_is_an_error(backend: Backend) {
    let tmp = tempfile::tempdir().unwrap();
    let Some((pool, _)) = fresh(backend, tmp.path(), "nested").await else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let dir = tmp.path().join("migrations");
    write_raw(
        &dir,
        "0001_nested",
        vec![json!({ "callback": { "name": "syschain_nested_migrate" } })],
    );
    let run = rustango::migrate::migrate_pool(&pool, &dir);
    let err = tokio::time::timeout(std::time::Duration::from_secs(60), run)
        .await
        .expect("a nested lock must not hang")
        .expect_err("a nested lock is refused");
    assert!(err.to_string().contains("holds the migrate lock"), "{err}");
}

/// Finding 3 — a framework table the project created and later dropped is
/// the system chain's again, on a fresh database and on one that has it.
async fn owned_table_dropped_later(backend: Backend) {
    let (table, column) = ("rustango_admin_users", "sessions_revoked_at");
    for heal in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let Some((pool, _)) = fresh(backend, tmp.path(), "drop").await else {
            eprintln!("skipping — backend URL unset");
            return;
        };
        let root = tmp.path().join("app");
        system_chain(&root);
        let dir = root.join("migrations");
        project_initial(&dir, current_table(table, Some(column)));
        if heal {
            manage_migrate(&pool, &dir).await.expect("project owns it");
        }
        write_step(
            &dir,
            "0002_drop",
            Some("0001_initial"),
            json!({ "tables": [] }),
            vec![json!({ "DropTable": table })],
        );
        manage_migrate(&pool, &dir).await.expect("first run");
        assert!(has_column(&pool, table, column).await, "heal={heal}");
        manage_migrate(&pool, &dir).await.expect("second run");
    }
}

/// #2066 — an empty project table gets a NOT NULL column with no default.
async fn not_null_column_on_empty_table(backend: Backend) {
    let tmp = tempfile::tempdir().unwrap();
    let Some((pool, _)) = fresh(backend, tmp.path(), "notnull").await else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let table = "rustango_admin_users";
    let full = current_table(table, None);
    let column = full["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["nullable"] == false && f["default"].is_null() && f["primary_key"] == false)
        .expect("a NOT NULL column with no default")["column"]
        .as_str()
        .unwrap()
        .to_owned();
    let root = tmp.path().join("app");
    system_chain(&root);
    let dir = root.join("migrations");
    project_initial(&dir, current_table(table, Some(&column)));
    manage_migrate(&pool, &dir).await.expect("first run");
    assert!(has_column(&pool, table, &column).await, "{column}");
    manage_migrate(&pool, &dir).await.expect("second run");
}

/// Finding 4 — two processes migrate one database at once.
#[cfg(any(feature = "postgres", feature = "mysql"))]
async fn concurrent_migrates(backend: Backend) {
    let tmp = tempfile::tempdir().unwrap();
    let Some((_, url)) = fresh(backend, tmp.path(), "race").await else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let (table, column) = ("rustango_admin_users", "sessions_revoked_at");
    let root = tmp.path().join("app");
    system_chain(&root);
    let dir = root.join("migrations");
    project_initial(&dir, current_table(table, Some(column)));
    let (a, b) = (
        Pool::connect(&url).await.unwrap(),
        Pool::connect(&url).await.unwrap(),
    );
    let both = async { tokio::join!(manage_migrate(&a, &dir), manage_migrate(&b, &dir)) };
    let (ra, rb) = tokio::time::timeout(std::time::Duration::from_secs(120), both)
        .await
        .expect("no deadlock");
    ra.expect("first replica");
    rb.expect("second replica");
    assert!(has_column(&a, table, column).await);
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

/// The signal tests: `post_migrate` receivers are process-global.
#[cfg(feature = "postgres")]
static SIGNALS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// #2052 on a schema-mode tenant: its own `rustango_users` gets the column.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn schema_mode_tenant_converges() {
    let _signals = SIGNALS.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let Some((registry, registry_url)) = fresh(Backend::Postgres, tmp.path(), "schema").await
    else {
        eprintln!("skipping — DATABASE_URL unset");
        return;
    };
    let boot = tmp.path().join("boot/migrations");
    std::fs::create_dir_all(&boot).unwrap();
    rustango::tenancy::migrate_registry_pool(&registry, &boot)
        .await
        .expect("registry tables");
    let mut org = rustango::tenancy::Org {
        slug: "t1".into(),
        display_name: "t1".into(),
        storage_mode: rustango::tenancy::StorageMode::Schema.as_str().into(),
        backend_kind: "postgres".into(),
        schema_name: Some("t1".into()),
        database_url: None,
        ..rustango::testkit::org()
    };
    org.insert_pool(&registry).await.unwrap();
    let (table, column) = ("rustango_users", "password_changed_at");
    let dir = tmp.path().join("app/migrations");
    project_initial(&dir, current_table(table, Some(column)));
    rustango::tenancy::migrate_registry_pool(&registry, &dir)
        .await
        .expect("registry");
    let Pool::Postgres(pg) = &registry else {
        unreachable!()
    };
    let pools = rustango::tenancy::TenantPools::new(pg.clone());
    for run in 0..2 {
        let report = rustango::tenancy::migrate_tenants(&pools, &dir, &registry_url)
            .await
            .expect("tenants");
        assert!(report.all_ok(), "run {run}: {report:?}");
    }
    let n: i64 = rustango::sql::sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.columns \
         WHERE table_schema = 't1' AND table_name = $1 AND column_name = $2",
    )
    .bind(table)
    .bind(column)
    .fetch_one(pg)
    .await
    .unwrap();
    assert_eq!(n, 1, "t1.{table}.{column}");
}

/// A `post_migrate` receiver may migrate again: the signal fires outside the lock.
#[cfg(all(feature = "postgres", feature = "signals"))]
#[tokio::test]
async fn post_migrate_receiver_can_migrate() {
    use std::sync::{Arc, Mutex};
    let _signals = SIGNALS.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let Some((registry, registry_url)) = fresh(Backend::Postgres, tmp.path(), "signal").await
    else {
        eprintln!("skipping — DATABASE_URL unset");
        return;
    };
    let boot = tmp.path().join("boot/migrations");
    std::fs::create_dir_all(&boot).unwrap();
    rustango::tenancy::migrate_registry_pool(&registry, &boot)
        .await
        .expect("registry tables");
    let mut org = rustango::tenancy::Org {
        slug: "t1".into(),
        display_name: "t1".into(),
        storage_mode: rustango::tenancy::StorageMode::Schema.as_str().into(),
        backend_kind: "postgres".into(),
        schema_name: Some("t1".into()),
        database_url: None,
        ..rustango::testkit::org()
    };
    org.insert_pool(&registry).await.unwrap();
    let dir = tmp.path().join("app/migrations");
    std::fs::create_dir_all(&dir).unwrap();
    let seen: Arc<Mutex<Option<Result<(), String>>>> = Arc::default();
    let (pool, boot, out) = (registry.clone(), boot.clone(), seen.clone());
    let id = rustango::signals::migrate::connect_post_migrate(move |_| {
        let (pool, boot, out) = (pool.clone(), boot.clone(), out.clone());
        async move {
            let r = rustango::migrate::migrate_pool(&pool, &boot).await;
            *out.lock().unwrap() = Some(r.map(|_| ()).map_err(|e| e.to_string()));
        }
    });
    let Pool::Postgres(pg) = &registry else {
        unreachable!()
    };
    let pools = rustango::tenancy::TenantPools::new(pg.clone());
    let run = rustango::tenancy::migrate_tenants(&pools, &dir, &registry_url);
    let report = tokio::time::timeout(std::time::Duration::from_secs(60), run).await;
    rustango::signals::migrate::disconnect_post_migrate(id);
    let report = report.expect("no hang").expect("tenants");
    assert!(report.all_ok(), "{report:?}");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen, Some(Ok(())), "the receiver's migrate");
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn concurrent_migrates_postgres() {
    concurrent_migrates(Backend::Postgres).await;
}

#[cfg(feature = "mysql")]
#[tokio::test]
async fn concurrent_migrates_mysql() {
    concurrent_migrates(Backend::Mysql).await;
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
    pending_project_add_column,
    alter_after_add_on_owned,
    owned_table_dropped_later,
    not_null_column_on_empty_table,
    later_index_on_owned,
    nested_lock_is_an_error,
);
