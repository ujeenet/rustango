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

/// Run `sql` on a new pool: a pooled SQLite connection keeps a stale schema
/// after another connection's DDL, and then refuses `CREATE INDEX`.
async fn ddl(url: &str, sql: &str) -> bool {
    let pool = Pool::connect(url).await.expect("connect");
    let ok = rustango::sql::raw_execute_pool(&pool, sql, Vec::new())
        .await
        .is_ok();
    pool.close().await;
    ok
}

/// Migrate `tmp/a`, then drop a live system index not on `id`; returns it.
async fn drop_a_system_index(
    backend: Backend,
    url: &str,
    tmp: &Path,
) -> rustango::migrate::IndexSnapshot {
    let pool = Pool::connect(url).await.expect("connect");
    manage_migrate(&pool, &tmp.join("a/migrations"))
        .await
        .expect("first run");
    let sys = rustango::migrate::file::list_dir(&tmp.join("a/system/migrations")).unwrap();
    // Not audit's: its `ensure_table_pool` recreates its own indexes. MySQL
    // refuses to drop an index an FK needs, so take the first that drops.
    for idx in sys.iter().flat_map(|m| &m.snapshot.indexes) {
        if idx.table == "rustango_audit_log"
            || idx.columns == ["id"]
            || !has_index(&pool, &idx.name).await
        {
            continue;
        }
        let sql = match backend {
            #[cfg(feature = "mysql")]
            Backend::Mysql => format!("DROP INDEX {} ON {}", idx.name, idx.table),
            #[allow(unreachable_patterns)]
            _ => format!("DROP INDEX {}", idx.name),
        };
        if ddl(url, &sql).await {
            pool.close().await;
            return idx.clone();
        }
    }
    panic!("no live system index to drop");
}

/// #2016 — a second dir's regenerated system chain restores a dropped index.
async fn dropped_index_is_restored(backend: Backend) {
    let tmp = tempfile::tempdir().unwrap();
    let Some((_, url)) = fresh(backend, tmp.path(), "idxgone").await else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let idx = drop_a_system_index(backend, &url, tmp.path()).await;
    let pool = Pool::connect(&url).await.expect("connect");
    assert!(
        !has_index(&pool, &idx.name).await,
        "{} was not dropped",
        idx.name
    );
    manage_migrate(&pool, &tmp.path().join("b/migrations"))
        .await
        .expect("second dir");
    assert!(
        has_index(&pool, &idx.name).await,
        "{} was not restored",
        idx.name
    );
}

/// #2016 — a converged index whose name is taken by one on other columns is reported.
async fn clashing_index_is_reported(backend: Backend) {
    let tmp = tempfile::tempdir().unwrap();
    let Some((_, url)) = fresh(backend, tmp.path(), "idxclash").await else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let idx = drop_a_system_index(backend, &url, tmp.path()).await;
    let (table, index) = (&idx.table, &idx.name);
    assert!(ddl(&url, &format!("CREATE INDEX {index} ON {table} (id)")).await);
    let buf = rustango::testkit::CaptureWriter::default();
    let writer = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .with_max_level(tracing::Level::WARN)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let pool = Pool::connect(&url).await.expect("connect");
    manage_migrate(&pool, &tmp.path().join("b/migrations"))
        .await
        .expect("second dir");
    let logged = buf.contents();
    assert!(
        logged.contains(&format!("index `{index}` is on `{table}` (id)")),
        "{logged}"
    );
}

/// A system step that only FK-references an owned table runs without first
/// adding that table's unrelated columns; `finish` reports what it can't add.
async fn fk_only_step_adds_only_targets(backend: Backend) {
    let tmp = tempfile::tempdir().unwrap();
    let Some((pool, _)) = fresh(backend, tmp.path(), "fkonly").await else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let table = "rustango_users";
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
    let first = read(&tenant_file(&root.join("system/migrations")));
    let refs: Vec<String> = first["snapshot"]["tables"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| {
            t["fields"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["fk"]["to"] == table)
        })
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect();
    assert!(!refs.is_empty(), "some system table FKs {table}");
    age_tenant_chain(&root, &refs, None);

    let dir = root.join("migrations");
    let mut older = current_table(table, Some(&column));
    for f in older["fields"].as_array_mut().unwrap() {
        if f["primary_key"] == false {
            f["nullable"] = json!(true);
        }
    }
    project_initial(&dir, older);
    rustango::migrate::migrate_pool(&pool, &dir).await.unwrap();
    let insert = match backend {
        #[cfg(feature = "mysql")]
        Backend::Mysql => format!("INSERT INTO {table} () VALUES ()"),
        #[allow(unreachable_patterns)]
        _ => format!("INSERT INTO {table} DEFAULT VALUES"),
    };
    rustango::sql::raw_execute_pool(&pool, &insert, Vec::new())
        .await
        .unwrap();
    let err = manage_migrate(&pool, &dir)
        .await
        .expect_err("a NOT NULL column can't fill rows");
    assert!(err.to_string().contains(&column), "{err}");
    for t in &refs {
        assert!(has_table(&pool, t).await, "{t} waited on {column}");
    }
}

/// The first tenant-chain table nothing references and that references
/// nothing, with its primary key field.
fn a_leaf_table(first: &Value, not: &str) -> (String, Value) {
    let tables = first["snapshot"]["tables"].as_array().unwrap();
    let has_fk = |t: &Value| {
        t["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| !f["fk"].is_null())
    };
    let referenced = |name: &Value| {
        tables.iter().any(|t| {
            t["fields"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["fk"]["to"] == *name)
        })
    };
    let t = tables
        .iter()
        // Not audit's: its `ensure_table_pool` creates it on its own.
        .find(|t| {
            t["name"] != not
                && t["name"] != "rustango_audit_log"
                && !has_fk(t)
                && t.get("composite_fks").is_none()
                && !referenced(&t["name"])
        })
        .expect("a leaf table");
    let pk = t["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["primary_key"] == true)
        .unwrap();
    (t["name"].as_str().unwrap().to_owned(), pk.clone())
}

/// #2053 / #2083 — a pending project FK to a system table created in, or
/// after, a system step that waits for the project chain. SQLite does not
/// check FK targets at `CREATE TABLE`.
async fn project_fk_to_waiting_system_table(backend: Backend, same_step: bool) {
    let tmp = tempfile::tempdir().unwrap();
    let Some((pool, _)) = fresh(backend, tmp.path(), "fkwait").await else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let root = tmp.path().join("app");
    system_chain(&root);
    let (table, column) = ("rustango_admin_users", "sessions_revoked_at");
    let first = read(&tenant_file(&root.join("system/migrations")));
    let (u, pk) = a_leaf_table(&first, table);
    // Today's catch-up step adds `column` to `table` and creates `u`.
    let path = tenant_file(&root.join("system/migrations"));
    let mut mig = read(&path);
    let tables = mig["snapshot"]["tables"].as_array_mut().unwrap();
    tables.retain(|t| t["name"] != u.as_str());
    let t = tables.iter_mut().find(|t| t["name"] == table).unwrap();
    t["fields"]
        .as_array_mut()
        .unwrap()
        .retain(|f| f["column"] != column);
    if let Some(list) = mig["snapshot"]["indexes"].as_array_mut() {
        list.retain(|i| i["table"] != u.as_str());
    }
    mig["forward"].as_array_mut().unwrap().retain(|op| {
        let s = &op["schema"];
        s["CreateTable"] != u.as_str() && s["CreateIndex"]["table"] != u.as_str()
    });
    std::fs::write(&path, mig.to_string()).unwrap();
    if !same_step {
        // A first step adds only the column; `u` comes after it.
        let mut snap = mig["snapshot"].clone();
        let t = snap["tables"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|t| t["name"] == table)
            .unwrap();
        t["fields"] = current_table(table, None)["fields"].clone();
        write_step(
            &root.join("system/migrations"),
            "9001_column",
            mig["name"].as_str(),
            snap,
            vec![json!({ "AddColumn": { "table": table, "column": column } })],
        );
    }
    let step = rustango::migrate::make_migrations_system(&root, ModelScope::Tenant, None)
        .unwrap()
        .expect("the catch-up step");
    let ops = format!("{:?}", step.forward);
    assert!(ops.contains(&u), "{ops}");
    assert_eq!(ops.contains(column), same_step, "{ops}");

    let dir = root.join("migrations");
    project_initial(&dir, current_table(table, Some(column)));
    // An older release applied the project chain alone.
    rustango::migrate::migrate_pool(&pool, &dir).await.unwrap();
    // B references `u` and comes before A, which writes the owned table.
    let mut u_id = pk.clone();
    for (k, v) in [
        ("name", json!("u_id")),
        ("column", json!("u_id")),
        ("nullable", json!(true)),
        ("primary_key", json!(false)),
        ("auto", json!(false)),
        ("fk", json!({ "kind": "fk", "to": u, "on": pk["column"] })),
    ] {
        u_id[k] = v;
    }
    let b = json!({ "name": "syschain_b", "model": "B", "fields": [
        { "name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true },
        u_id,
    ] });
    let older = current_table(table, Some(column));
    write_step(
        &dir,
        "0002_b",
        Some("0001_initial"),
        json!({ "tables": [older, b.clone()] }),
        vec![json!({ "CreateTable": "syschain_b" })],
    );
    write_step(
        &dir,
        "0003_a",
        Some("0002_b"),
        json!({ "tables": [current_table(table, None), b] }),
        vec![json!({ "AddColumn": { "table": table, "column": column } })],
    );
    manage_migrate(&pool, &dir).await.expect("first run");
    assert!(has_table(&pool, "syschain_b").await);
    assert!(has_column(&pool, table, column).await);
    manage_migrate(&pool, &dir).await.expect("second run");
}

async fn fk_to_a_later_system_step(backend: Backend) {
    project_fk_to_waiting_system_table(backend, false).await;
}

async fn fk_to_a_waiting_system_step(backend: Backend) {
    project_fk_to_waiting_system_table(backend, true).await;
}

/// #2094 — a later system step drops an index the project's table never got.
async fn drop_index_on_owned(backend: Backend) {
    let tmp = tempfile::tempdir().unwrap();
    let Some((pool, _)) = fresh(backend, tmp.path(), "dropidx").await else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let root = tmp.path().join("app");
    system_chain(&root);
    let sys = root.join("system/migrations");
    let first = read(&tenant_file(&sys));
    let idx = first["snapshot"]["indexes"][0].clone();
    let (table, index) = (
        idx["table"].as_str().unwrap(),
        idx["name"].as_str().unwrap(),
    );
    let mut snap = first["snapshot"].clone();
    snap["indexes"]
        .as_array_mut()
        .unwrap()
        .retain(|i| i["name"] != index);
    write_step(
        &sys,
        "9001_drop_index",
        first["name"].as_str(),
        snap,
        vec![json!({ "DropIndex": { "name": index, "table": table } })],
    );
    let dir = root.join("migrations");
    project_initial(&dir, current_table(table, None));
    // MySQL failed here: `DROP INDEX` of an index that was never made.
    manage_migrate(&pool, &dir).await.expect("first run");
    manage_migrate(&pool, &dir).await.expect("second run");
}

/// #2139 — an index a later step drops and re-creates is there on an owned table.
async fn recreated_index_on_owned(backend: Backend) {
    let tmp = tempfile::tempdir().unwrap();
    let Some((pool, _)) = fresh(backend, tmp.path(), "reidx").await else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let root = tmp.path().join("app");
    system_chain(&root);
    let sys = root.join("system/migrations");
    let first = read(&tenant_file(&sys));
    let idx = first["snapshot"]["indexes"][0].clone();
    let (table, index) = (
        idx["table"].as_str().unwrap(),
        idx["name"].as_str().unwrap(),
    );
    write_step(
        &sys,
        "9001_reindex",
        first["name"].as_str(),
        first["snapshot"].clone(),
        vec![
            json!({ "DropIndex": { "name": index, "table": table } }),
            json!({ "CreateIndex": idx }),
        ],
    );
    let dir = root.join("migrations");
    project_initial(&dir, current_table(table, None));
    manage_migrate(&pool, &dir).await.expect("first run");
    assert!(has_index(&pool, index).await, "{table}.{index} missing");
    manage_migrate(&pool, &dir).await.expect("second run");
}

/// A table a waiting step made early still gets a later step's new index.
async fn early_table_gets_a_later_index(backend: Backend) {
    let tmp = tempfile::tempdir().unwrap();
    let Some((pool, _)) = fresh(backend, tmp.path(), "earlyidx").await else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let root = tmp.path().join("app");
    system_chain(&root);
    let first = read(&tenant_file(&root.join("system/migrations")));
    let users = "rustango_users";
    let tables = first["snapshot"]["tables"].as_array().unwrap();
    let no_fk = |name: &Value| {
        tables.iter().any(|t| {
            t["name"] == *name
                && t["name"] != "rustango_audit_log"
                && t["fields"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|f| f["fk"].is_null())
        })
    };
    let idx = first["snapshot"]["indexes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["table"] != users && no_fk(&i["table"]))
        .expect("an index on a table with no FK")
        .clone();
    let index = idx["name"].as_str().unwrap();
    let ops = age_tenant_chain(&root, &[], Some(index));
    assert!(ops.contains(index), "{ops}");
    let dir = root.join("migrations");
    // The project's `users` waits the first step; its other tables come early.
    project_initial(&dir, current_table(users, None));
    manage_migrate(&pool, &dir).await.expect("first run");
    assert!(has_index(&pool, index).await, "{index} was not created");
    manage_migrate(&pool, &dir).await.expect("second run");
}

/// A later system step that only references a waiting step's table waits
/// with it: 9002's FK to 9001's table would fail before 9001 ran.
async fn later_step_waits_on_a_held_table(backend: Backend) {
    let tmp = tempfile::tempdir().unwrap();
    let Some((pool, _)) = fresh(backend, tmp.path(), "held").await else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let root = tmp.path().join("app");
    system_chain(&root);
    let sys = root.join("system/migrations");
    let first = read(&tenant_file(&sys));
    let owned = "rustango_admin_users";
    let admin = current_table(owned, None);
    let pk = admin["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["primary_key"] == true)
        .unwrap()
        .clone();
    let fk = |name: &str, to: &str, on: &Value| {
        let mut f = on.clone();
        for (k, v) in [
            ("name", json!(name)),
            ("column", json!(name)),
            ("nullable", json!(true)),
            ("primary_key", json!(false)),
            ("auto", json!(false)),
            ("fk", json!({ "kind": "fk", "to": to, "on": on["column"] })),
        ] {
            f[k] = v;
        }
        f
    };
    let id = json!({ "name": "id", "column": "id", "ty": "i64", "nullable": false,
                     "primary_key": true });
    let p = json!({ "name": "syschain_p", "model": "P", "fields": [id.clone(), fk("admin_id", owned, &pk)] });
    let q = json!({ "name": "syschain_q", "model": "Q", "fields": [id.clone(), fk("p_id", "syschain_p", &id)] });
    let mut snap = first["snapshot"].clone();
    snap["tables"].as_array_mut().unwrap().push(p);
    write_step(
        &sys,
        "9001_p",
        first["name"].as_str(),
        snap.clone(),
        vec![json!({ "CreateTable": "syschain_p" })],
    );
    snap["tables"].as_array_mut().unwrap().push(q);
    write_step(
        &sys,
        "9002_q",
        Some("9001_p"),
        snap,
        vec![json!({ "CreateTable": "syschain_q" })],
    );
    let dir = root.join("migrations");
    // The project's `owned` is missing until its chain runs, so 9001 waits.
    project_initial(&dir, admin);
    manage_migrate(&pool, &dir).await.expect("first run");
    manage_migrate(&pool, &dir).await.expect("second run");
}

/// Whether `table` has an FK to `to`.
async fn has_fk(pool: &Pool, table: &str, to: &str) -> bool {
    let n: i64 = match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => rustango::sql::sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_constraint \
             WHERE contype = 'f' AND conrelid = to_regclass($1) AND confrelid = to_regclass($2)",
        )
        .bind(table)
        .bind(to)
        .fetch_one(pg)
        .await
        .unwrap(),
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => rustango::sql::sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.key_column_usage \
             WHERE table_schema = DATABASE() AND table_name = ? AND referenced_table_name = ?",
        )
        .bind(table)
        .bind(to)
        .fetch_one(my)
        .await
        .unwrap(),
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => rustango::sql::sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_foreign_key_list(?) WHERE \"table\" = ?",
        )
        .bind(table)
        .bind(to)
        .fetch_one(sq)
        .await
        .unwrap(),
    };
    n > 0
}

/// #2084 — a framework table the project drops comes back with the FKs
/// other tables had to it: PG's `DROP TABLE … CASCADE` takes them.
async fn dropped_table_gets_its_fks_back(backend: Backend) {
    #[allow(unreachable_patterns)]
    match backend {
        #[cfg(feature = "mysql")]
        Backend::Mysql => {
            eprintln!("skipping — MySQL refuses to drop a referenced table (3730)");
            return;
        }
        _ => {}
    }
    let tmp = tempfile::tempdir().unwrap();
    let Some((pool, _)) = fresh(backend, tmp.path(), "remade").await else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let root = tmp.path().join("app");
    system_chain(&root);
    let first = read(&tenant_file(&root.join("system/migrations")));
    let tables = first["snapshot"]["tables"].as_array().unwrap();
    let fks = |t: &Value| {
        t["fields"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| f["fk"]["to"].as_str().map(str::to_owned))
            .collect::<Vec<_>>()
    };
    // A table that references nothing, and one table that references it.
    let (parent, child) = tables
        .iter()
        .filter(|p| fks(p).is_empty())
        .find_map(|p| {
            let name = p["name"].as_str().unwrap();
            let c = tables.iter().find(|c| fks(c) == [name])?;
            Some((name.to_owned(), c["name"].as_str().unwrap().to_owned()))
        })
        .expect("a referenced leaf table");
    let dir = root.join("migrations");
    project_initial(&dir, current_table(&parent, None));
    manage_migrate(&pool, &dir).await.expect("project owns it");
    assert!(has_fk(&pool, &child, &parent).await);
    write_step(
        &dir,
        "0002_drop",
        Some("0001_initial"),
        json!({ "tables": [] }),
        vec![json!({ "DropTable": parent })],
    );
    manage_migrate(&pool, &dir).await.expect("first run");
    assert!(has_table(&pool, &parent).await);
    assert!(
        has_fk(&pool, &child, &parent).await,
        "{child} lost its FK to {parent}"
    );
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

/// A cancelled migrate leaves no session holding the lock in its pool.
#[cfg(any(feature = "postgres", feature = "mysql"))]
async fn cancelled_run_releases_lock(backend: Backend) {
    let tmp = tempfile::tempdir().unwrap();
    let Some((a, url)) = fresh(backend, tmp.path(), "cancel").await else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let sleep = match backend {
        #[cfg(feature = "postgres")]
        Backend::Postgres => "SELECT pg_sleep(30)",
        #[cfg(feature = "mysql")]
        Backend::Mysql => "SELECT SLEEP(30)",
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => unreachable!(),
    };
    let dir = tmp.path().join("slow");
    write_raw(
        &dir,
        "0001_slow",
        vec![json!({ "data": { "sql": sleep, "reversible": false } })],
    );
    let slow = rustango::migrate::migrate_pool(&a, &dir);
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(3), slow)
            .await
            .is_err(),
        "the slow migrate was cancelled"
    );
    let b = Pool::connect(&url).await.unwrap();
    let empty = tmp.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let next = rustango::migrate::migrate_pool(&b, &empty);
    tokio::time::timeout(std::time::Duration::from_secs(15), next)
        .await
        .expect("the cancelled run still holds the lock")
        .expect("migrate");
    drop(a);
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn cancelled_run_releases_lock_postgres() {
    cancelled_run_releases_lock(Backend::Postgres).await;
}

#[cfg(feature = "mysql")]
#[tokio::test]
async fn cancelled_run_releases_lock_mysql() {
    cancelled_run_releases_lock(Backend::Mysql).await;
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

/// #1718 — a tenant FK to a table its schema lacks must not bind to `public`'s;
/// one to a registry table must.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn schema_mode_fk_stays_in_the_tenant_schema() {
    let _signals = SIGNALS.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let Some((registry, registry_url)) = fresh(Backend::Postgres, tmp.path(), "fkq").await else {
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
    let Pool::Postgres(pg) = &registry else {
        unreachable!()
    };
    rustango::sql::sqlx::query("CREATE TABLE fkq_parent (id BIGINT PRIMARY KEY)")
        .execute(pg)
        .await
        .unwrap();
    let field = |name: &str, fk: Option<Value>| {
        json!({ "name": name, "column": name, "ty": "i64", "nullable": fk.is_some(),
                "primary_key": fk.is_none(), "fk": fk })
    };
    let parent = json!({ "name": "fkq_parent", "model": "P", "fields": [field("id", None)] });
    let child = json!({ "name": "fkq_child", "model": "C", "fields": [
        field("id", None),
        field("parent_id", Some(json!({ "kind": "fk", "to": "fkq_parent", "on": "id" }))),
    ] });
    let orgs = json!({ "name": "rustango_orgs", "model": "Org", "fields": [field("id", None)] });
    let reg = json!({ "name": "fkq_reg", "model": "R", "fields": [
        field("id", None),
        field("org_id", Some(json!({ "kind": "fk", "to": "rustango_orgs", "on": "id" }))),
    ] });
    let dir = tmp.path().join("app/migrations");
    write_step(
        &dir,
        "0001_reg",
        None,
        json!({ "tables": [orgs.clone(), reg.clone()] }),
        vec![json!({ "CreateTable": "fkq_reg" })],
    );
    write_step(
        &dir,
        "0002_child",
        Some("0001_reg"),
        json!({ "tables": [orgs, reg, parent, child] }),
        vec![json!({ "CreateTable": "fkq_child" })],
    );
    let pools = rustango::tenancy::TenantPools::new(pg.clone());
    let report = rustango::tenancy::migrate_tenants(&pools, &dir, &registry_url)
        .await
        .expect("tenants");
    let to_public: i64 = rustango::sql::sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_constraint c JOIN pg_class r ON r.oid = c.confrelid \
         JOIN pg_namespace n ON n.oid = r.relnamespace \
         WHERE c.contype = 'f' AND n.nspname = 'public' AND r.relname = 'fkq_parent'",
    )
    .fetch_one(pg)
    .await
    .unwrap();
    assert_eq!(to_public, 0, "t1.fkq_child references public.fkq_parent");
    // A registry model's table is shared: its FK still reaches `public`.
    let to_registry: i64 = rustango::sql::sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_constraint c JOIN pg_class r ON r.oid = c.confrelid \
         JOIN pg_namespace n ON n.oid = r.relnamespace JOIN pg_class t ON t.oid = c.conrelid \
         WHERE c.contype = 'f' AND n.nspname = 'public' AND r.relname = 'rustango_orgs' \
         AND t.relname = 'fkq_reg'",
    )
    .fetch_one(pg)
    .await
    .unwrap();
    assert_eq!(
        to_registry, 1,
        "t1.fkq_reg must reference public.rustango_orgs"
    );
    assert!(
        !report.all_ok(),
        "the missing tenant parent is reported: {report:?}"
    );
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

/// #2027 — migrates waiting on the lock must not hold the pool the holder needs.
#[cfg(any(feature = "postgres", feature = "mysql"))]
async fn waiters_leave_the_pool_free(backend: Backend) {
    use rustango::sql::sqlx;
    #[cfg(feature = "postgres")]
    const KEY: i64 = 0x5255_5354_4d49_4754;
    const N: u32 = 3;
    let tmp = tempfile::tempdir().unwrap();
    let Some((_, url)) = fresh(backend, tmp.path(), "smallpool").await else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let wait = std::time::Duration::from_secs(5);
    // Another process holds the lock while N migrates queue on a pool of N.
    let (pool, outside) = match backend {
        #[cfg(feature = "postgres")]
        Backend::Postgres => {
            use sqlx::Connection as _;
            let opts = sqlx::postgres::PgPoolOptions::new().max_connections(N);
            let pool = opts.acquire_timeout(wait).connect(&url).await.unwrap();
            let mut outside = sqlx::PgConnection::connect(&url).await.unwrap();
            sqlx::query("SELECT pg_advisory_lock($1)")
                .bind(KEY)
                .execute(&mut outside)
                .await
                .unwrap();
            (
                Pool::Postgres(pool),
                Box::new(outside) as Box<dyn std::any::Any>,
            )
        }
        #[cfg(feature = "mysql")]
        Backend::Mysql => {
            use sqlx::Connection as _;
            let opts = sqlx::mysql::MySqlPoolOptions::new().max_connections(N);
            let pool = opts.acquire_timeout(wait).connect(&url).await.unwrap();
            let mut outside = sqlx::MySqlConnection::connect(&url).await.unwrap();
            sqlx::query(MY_HOLD_LOCK)
                .execute(&mut outside)
                .await
                .unwrap();
            (
                Pool::Mysql(pool),
                Box::new(outside) as Box<dyn std::any::Any>,
            )
        }
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => unreachable!(),
    };
    let empty = tmp.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let runs: Vec<_> = (0..N)
        .map(|_| {
            let (pool, dir) = (pool.clone(), empty.clone());
            tokio::spawn(async move { rustango::migrate::migrate_pool(&pool, &dir).await })
        })
        .collect();
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    drop(outside);
    for run in runs {
        tokio::time::timeout(std::time::Duration::from_secs(60), run)
            .await
            .expect("no hang")
            .unwrap()
            .expect("migrate on a shared small pool");
    }
}

/// The runner's per-database MySQL lock, taken from outside.
#[cfg(feature = "mysql")]
const MY_HOLD_LOCK: &str = "SELECT GET_LOCK(CONCAT('rustango_migrate_', SHA1(DATABASE())), -1)";

/// Whether the database at `url` has its migrate lock taken.
#[cfg(any(feature = "postgres", feature = "mysql"))]
async fn migrate_lock_held(backend: Backend, url: &str) -> bool {
    use rustango::sql::sqlx::{self, Connection as _};
    match backend {
        #[cfg(feature = "postgres")]
        Backend::Postgres => {
            let mut c = sqlx::PgConnection::connect(url).await.unwrap();
            sqlx::query(
                "SELECT 1 FROM pg_locks l JOIN pg_database d ON d.oid = l.database \
                 WHERE l.locktype = 'advisory' AND l.granted AND d.datname = current_database()",
            )
            .fetch_optional(&mut c)
            .await
            .unwrap()
            .is_some()
        }
        #[cfg(feature = "mysql")]
        Backend::Mysql => {
            let mut c = sqlx::MySqlConnection::connect(url).await.unwrap();
            let held: (Option<u64>,) = sqlx::query_as(
                "SELECT IS_USED_LOCK(CONCAT('rustango_migrate_', SHA1(DATABASE())))",
            )
            .fetch_one(&mut c)
            .await
            .unwrap();
            held.0.is_some()
        }
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => unreachable!(),
    }
}

/// A slow migrate in one database doesn't block a migrate in another (#1991).
#[cfg(any(feature = "postgres", feature = "mysql"))]
async fn lock_is_per_database(backend: Backend) {
    let tmp = tempfile::tempdir().unwrap();
    let (Some((a, a_url)), Some((b, _))) = (
        fresh(backend, tmp.path(), "lockdba").await,
        fresh(backend, tmp.path(), "lockdbb").await,
    ) else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let sleep = match backend {
        #[cfg(feature = "postgres")]
        Backend::Postgres => "SELECT pg_sleep(4)",
        #[cfg(feature = "mysql")]
        Backend::Mysql => "SELECT SLEEP(4)",
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => unreachable!(),
    };
    let slow_dir = tmp.path().join("slow");
    write_raw(
        &slow_dir,
        "0001_slow",
        vec![json!({ "data": { "sql": sleep, "reversible": false } })],
    );
    let slow = tokio::spawn(async move { rustango::migrate::migrate_pool(&a, &slow_dir).await });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !migrate_lock_held(backend, &a_url).await {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the slow migrate holds its database's lock");
    let empty = tmp.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let limit = std::time::Duration::from_secs(1);
    rustango::migrate::with_lock_timeout(limit, rustango::migrate::migrate_pool(&b, &empty))
        .await
        .expect("another database's migrate must not hold this one's lock");
    slow.await.unwrap().expect("slow migrate");
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn lock_is_per_database_postgres() {
    lock_is_per_database(Backend::Postgres).await;
}

#[cfg(feature = "mysql")]
#[tokio::test]
async fn lock_is_per_database_mysql() {
    lock_is_per_database(Backend::Mysql).await;
}

/// A migrate under `with_lock_timeout` gives up on a held lock with a clear error.
#[cfg(any(feature = "postgres", feature = "mysql"))]
async fn lock_timeout_gives_up(backend: Backend) {
    use rustango::sql::sqlx::{self, Connection as _};
    #[cfg(feature = "postgres")]
    const KEY: i64 = 0x5255_5354_4d49_4754;
    let tmp = tempfile::tempdir().unwrap();
    let Some((pool, url)) = fresh(backend, tmp.path(), "locktimeout").await else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let _outside: Box<dyn std::any::Any> = match backend {
        #[cfg(feature = "postgres")]
        Backend::Postgres => {
            let mut c = sqlx::PgConnection::connect(&url).await.unwrap();
            sqlx::query("SELECT pg_advisory_lock($1)")
                .bind(KEY)
                .execute(&mut c)
                .await
                .unwrap();
            Box::new(c)
        }
        #[cfg(feature = "mysql")]
        Backend::Mysql => {
            let mut c = sqlx::MySqlConnection::connect(&url).await.unwrap();
            sqlx::query(MY_HOLD_LOCK).execute(&mut c).await.unwrap();
            Box::new(c)
        }
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => unreachable!(),
    };
    let dir = tmp.path().join("empty");
    std::fs::create_dir_all(&dir).unwrap();
    let limit = std::time::Duration::from_secs(1);
    let run =
        rustango::migrate::with_lock_timeout(limit, rustango::migrate::migrate_pool(&pool, &dir));
    let err = tokio::time::timeout(std::time::Duration::from_secs(20), run)
        .await
        .expect("the wait is bounded")
        .expect_err("the lock is held");
    assert!(
        matches!(err, rustango::migrate::MigrateError::LockTimeout(d) if d == limit),
        "{err}"
    );
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn lock_timeout_gives_up_postgres() {
    lock_timeout_gives_up(Backend::Postgres).await;
}

#[cfg(feature = "mysql")]
#[tokio::test]
async fn lock_timeout_gives_up_mysql() {
    lock_timeout_gives_up(Backend::Mysql).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn waiters_leave_the_pool_free_postgres() {
    waiters_leave_the_pool_free(Backend::Postgres).await;
}

#[cfg(feature = "mysql")]
#[tokio::test]
async fn waiters_leave_the_pool_free_mysql() {
    waiters_leave_the_pool_free(Backend::Mysql).await;
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
    clashing_index_is_reported,
    dropped_index_is_restored,
    fk_only_step_adds_only_targets,
    nested_lock_is_an_error,
    fk_to_a_later_system_step,
    fk_to_a_waiting_system_step,
    drop_index_on_owned,
    recreated_index_on_owned,
    dropped_table_gets_its_fks_back,
    later_step_waits_on_a_held_table,
    early_table_gets_a_later_index,
);
