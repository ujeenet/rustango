//! The v2 runner on PG, MySQL and SQLite: apply, roll back, re-apply;
//! edited files; crash and resume; lock exclusion; adopt and the
//! per-database switch. Each test gets a database of its own.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use super::*;
use crate::sql::{raw_execute_pool, Pool};

/// A database of the test's own; `Drop` removes a server database.
struct Db {
    pool: Pool,
    drop: Option<(String, String)>,
    _dir: tempfile::TempDir,
}

impl Drop for Db {
    fn drop(&mut self) {
        let Some((admin_url, name)) = self.drop.take() else {
            return;
        };
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let Ok(admin) = Pool::connect(&admin_url).await else {
                    return;
                };
                let force = if admin.dialect().kind() == crate::sql::DialectKind::Postgres {
                    " WITH (FORCE)"
                } else {
                    ""
                };
                let sql = format!(
                    "DROP DATABASE IF EXISTS {}{force}",
                    admin.dialect().quote_ident(&name)
                );
                let _ = raw_execute_pool(&admin, &sql, Vec::new()).await;
                admin.close().await;
            });
        })
        .join();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum B {
    Pg,
    My,
    Lite,
}

/// A fresh database on `b`, or `None` when its URL is unset.
async fn fresh(b: B) -> Option<Db> {
    let dir = tempfile::tempdir().unwrap();
    let server = |var: &str, backend: &str| crate::testkit::matrix::live_url(var, backend);
    let admin = match b {
        B::Pg => server("DATABASE_URL", "postgres")?,
        B::My => server("MYSQL_TEST_URL", "mysql")?,
        B::Lite => {
            let url = format!("sqlite:{}?mode=rwc", dir.path().join("v2.db").display());
            let pool = Pool::connect(&url).await.unwrap();
            return Some(Db {
                pool,
                drop: None,
                _dir: dir,
            });
        }
    };
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = format!(
        "rustango_v2run_{}_{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let a = Pool::connect(&admin).await.expect("admin");
    raw_execute_pool(
        &a,
        &format!("CREATE DATABASE {}", a.dialect().quote_ident(&name)),
        Vec::new(),
    )
    .await
    .unwrap();
    a.close().await;
    let (base, _) = admin.rsplit_once('/').unwrap();
    let pool = Pool::connect(&format!("{base}/{name}")).await.unwrap();
    Some(Db {
        pool,
        drop: Some((admin, name)),
        _dir: dir,
    })
}

fn put(dir: &Path, name: &str, v: &Value) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join(format!("{name}.json")),
        serde_json::to_string_pretty(v).unwrap(),
    )
    .unwrap();
}

fn v2(id: &str, parents: &[&str], ops: Value) -> Value {
    json!({"engine": 2, "format": 1, "id": id, "app": "project", "parents": parents,
           "target": "tenant", "operations": ops})
}

fn author() -> Value {
    json!({"op": "create_table", "v": 1, "table": {"name": "rv_author", "scope": "tenant", "fields": [
        {"column": "id", "ty": "i64", "primary_key": true, "auto": true, "nullable": false},
        {"column": "name", "ty": "string", "max_length": 50, "nullable": true}]}})
}

fn add_bio() -> Value {
    json!([{"op": "add_column", "v": 1, "table": "rv_author",
            "column": {"column": "bio", "ty": "string", "max_length": 80, "nullable": true}},
           {"op": "add_index", "v": 1, "table": "rv_author", "name": "rv_author_name_idx",
            "columns": ["name"]}])
}

/// `migrations/` with 0001 (create) and 0002 (add a column and an index).
fn project() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    put(
        &dir,
        "0001_author",
        &v2("0001_author", &[], json!([author()])),
    );
    put(
        &dir,
        "0002_bio",
        &v2("0002_bio", &["0001_author"], add_bio()),
    );
    (root, dir)
}

async fn columns(pool: &Pool, t: &str) -> Vec<String> {
    super::super::catalog::read_table(pool, t)
        .await
        .unwrap()
        .map(|l| l.columns.into_iter().map(|c| c.name).collect())
        .unwrap_or_default()
}

async fn rows(pool: &Pool) -> Vec<(String, RowState)> {
    Ledger::default_names()
        .rows(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.id, r.state))
        .collect()
}

fn code(r: Result<Report, RunError>) -> Code {
    match r {
        Ok(rep) => panic!("expected a refusal, got {rep:?}"),
        Err(e) => e
            .code()
            .unwrap_or_else(|| panic!("not a coded refusal: {e}")),
    }
}

fn opts() -> Options {
    Options::default()
}

async fn apply_rollback_reapply(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let (_root, dir) = project();
    let rep = migrate(pool, &dir, &opts()).await.unwrap();
    assert_eq!(rep.applied, ["0001_author", "0002_bio"]);
    assert!(!rep.system.is_empty(), "the system chain ran first");
    assert_eq!(columns(pool, "rv_author").await, ["id", "name", "bio"]);
    // Nothing left: a second run applies nothing.
    assert!(migrate(pool, &dir, &opts())
        .await
        .unwrap()
        .applied
        .is_empty());
    let back = downgrade(pool, &dir, 1, &opts()).await.unwrap();
    assert_eq!(back.rolled_back, ["0002_bio"]);
    assert_eq!(columns(pool, "rv_author").await, ["id", "name"]);
    assert_eq!(
        rows(pool).await,
        [("0001_author".to_owned(), RowState::Applied)]
    );
    assert_eq!(
        migrate(pool, &dir, &opts()).await.unwrap().applied,
        ["0002_bio"]
    );
    downgrade(pool, &dir, 2, &opts()).await.unwrap();
    assert!(columns(pool, "rv_author").await.is_empty(), "table dropped");
    assert!(rows(pool).await.is_empty());
    let kinds = Ledger::default_names()
        .event_kinds(pool, ("project", "0002_bio"))
        .await
        .unwrap();
    assert_eq!(kinds, ["applied", "rolled_back", "applied", "rolled_back"]);
    migrate(pool, &dir, &opts()).await.unwrap();
    assert_eq!(columns(pool, "rv_author").await, ["id", "name", "bio"]);
}

/// An applied file edited on disk is refused; a rollback runs what ran,
/// from the stored body, even with the file gone.
async fn edited_file_and_stored_body(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let (_root, dir) = project();
    migrate(pool, &dir, &opts()).await.unwrap();
    let mut edited = v2("0002_bio", &["0001_author"], add_bio());
    edited["operations"][0]["column"]["max_length"] = json!(99);
    put(&dir, "0002_bio", &edited);
    assert_eq!(code(migrate(pool, &dir, &opts()).await), Code::Edited);
    std::fs::remove_file(dir.join("0002_bio.json")).unwrap();
    downgrade(pool, &dir, 1, &opts()).await.unwrap();
    assert_eq!(columns(pool, "rv_author").await, ["id", "name"]);
}

/// A file with an autocommit unit, on every backend: a raw index with its
/// declared state as the probe.
fn raw_index() -> Value {
    json!([{"op": "raw_sql", "v": 1, "id": "name_ix", "tx": "autocommit", "probe": "state",
            "up": {"any": "CREATE INDEX rv_raw_ix ON {{t:rv_author}} ({{c:name}})"},
            "down": {"any": "DROP INDEX rv_raw_ix", "mysql": "DROP INDEX rv_raw_ix ON rv_author"},
            "state": [{"op": "add_index", "v": 1, "table": "rv_author", "name": "rv_raw_ix",
                       "columns": ["name"]}]}])
}

/// Killed mid-run: the next run resumes from the ledger, never re-running
/// a committed unit.
async fn crash_and_resume(b: B) {
    for crash in [
        Crash::BeforeUnit(1),
        Crash::AfterStatement(1),
        Crash::AfterStart(1),
    ] {
        let Some(db) = fresh(b).await else { return };
        let pool = &db.pool;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("migrations");
        put(
            &dir,
            "0001_author",
            &v2("0001_author", &[], json!([author()])),
        );
        put(
            &dir,
            "0002_ix",
            &v2("0002_ix", &["0001_author"], raw_index()),
        );
        let crashing = Options {
            crash: Some(crash),
            ..Options::default()
        };
        migrate(pool, &dir, &crashing).await.expect_err("crashed");
        let left = rows(pool).await;
        assert_eq!(
            left.last().unwrap().1,
            RowState::Failed,
            "{crash:?}: {left:?}"
        );
        // Fencing: a run that does not own the row cannot move it.
        let other = Ledger::default_names()
            .start_unit(pool, ("project", "0002_ix"), "not-the-owner", 0)
            .await;
        assert!(
            other.unwrap_err().to_string().contains("lock lost"),
            "{crash:?}"
        );
        let rep = migrate(pool, &dir, &opts())
            .await
            .unwrap_or_else(|e| panic!("{crash:?}: {e}"));
        assert_eq!(rep.applied, ["0002_ix"], "{crash:?}");
        let ix = super::super::catalog::read_table(pool, "rv_author")
            .await
            .unwrap()
            .unwrap();
        assert!(
            ix.indexes.iter().any(|i| i.name == "rv_raw_ix"),
            "{crash:?}"
        );
        let kinds = Ledger::default_names()
            .event_kinds(pool, ("project", "0002_ix"))
            .await
            .unwrap();
        assert_eq!(kinds, ["resumed", "applied"], "{crash:?}");
    }
}

/// v2 and v2, and v2 and v1, take the same lock: one waits for the other.
async fn runs_exclude_each_other(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = db.pool.clone();
    let (_root, dir) = project();
    let held = tokio::sync::oneshot::channel::<()>();
    let (tx, rx) = held;
    let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
    let holder_pool = pool.clone();
    let holder = tokio::spawn(async move {
        crate::migrate::with_migrate_lock_held_v2(V2Token(()), &holder_pool, |_| async move {
            let _ = tx.send(());
            let _ = go_rx.await;
            Ok(())
        })
        .await
    });
    rx.await.unwrap();
    let short = Duration::from_millis(300);
    let v2_run = crate::migrate::with_lock_timeout(short, migrate(&pool, &dir, &opts())).await;
    assert!(
        matches!(v2_run, Err(RunError::Db(MigrateError::LockTimeout(_)))),
        "{v2_run:?}"
    );
    let v1_dir = tempfile::tempdir().unwrap();
    let v1_run = crate::migrate::with_lock_timeout(
        short,
        crate::migrate::migrate_pool(&pool, v1_dir.path()),
    )
    .await;
    assert!(
        matches!(v1_run, Err(MigrateError::LockTimeout(_))),
        "{v1_run:?}"
    );
    go_tx.send(()).unwrap();
    holder.await.unwrap().unwrap();
    // And a v1 holder keeps v2 out.
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
    let holder_pool = pool.clone();
    let holder = tokio::spawn(async move {
        crate::migrate::runner::with_migrate_lock_held(&holder_pool, |_| async move {
            let _ = tx.send(());
            let _ = go_rx.await;
            Ok(())
        })
        .await
    });
    rx.await.unwrap();
    let v2_run = crate::migrate::with_lock_timeout(short, migrate(&pool, &dir, &opts())).await;
    assert!(
        matches!(v2_run, Err(RunError::Db(MigrateError::LockTimeout(_)))),
        "{v2_run:?}"
    );
    go_tx.send(()).unwrap();
    holder.await.unwrap().unwrap();
    migrate(&pool, &dir, &opts()).await.unwrap();
}

// ---- adopt ----

fn v1_field(name: &str, ty: &str, extra: Value) -> Value {
    let mut f =
        json!({"name": name, "column": name, "ty": ty, "nullable": true, "primary_key": false});
    if let Value::Object(m) = extra {
        for (k, v) in m {
            f[k] = v;
        }
    }
    f
}

/// A v1 chain: 0001 creates `ad_author` (unique email, a default) and
/// `ad_book` (FK to it, an index); 0002 adds a column.
fn v1_project(dir: &Path) {
    let author = json!({"name": "ad_author", "model": "AdAuthor", "fields": [
        v1_field("id", "i64", json!({"primary_key": true, "auto": true, "nullable": false})),
        v1_field("name", "string", json!({"max_length": 50, "nullable": false, "default": "'x'"})),
        v1_field("email", "string", json!({"max_length": 100, "unique": true}))]});
    let book = json!({"name": "ad_book", "model": "AdBook", "fields": [
        v1_field("id", "i64", json!({"primary_key": true, "auto": true, "nullable": false})),
        v1_field("author_id", "i64", json!({"nullable": false,
            "fk": {"kind": "fk", "to": "ad_author", "on": "id", "on_delete": "CASCADE"}})),
        v1_field("title", "string", json!({"max_length": 200}))]});
    let index = json!({"name": "ad_book_title_idx", "table": "ad_book", "columns": ["title"],
        "unique": false, "method": "btree"});
    put(
        dir,
        "0001_initial",
        &json!({"name": "0001_initial", "created_at": "",
        "snapshot": {"tables": [author.clone(), book.clone()], "indexes": [index.clone()]},
        "forward": [{"schema": {"CreateTable": "ad_author"}}, {"schema": {"CreateTable": "ad_book"}},
                    {"schema": {"CreateIndex": {"name": "ad_book_title_idx", "table": "ad_book",
                        "columns": ["title"], "unique": false}}}]}),
    );
    let mut book2 = book;
    book2["fields"]
        .as_array_mut()
        .unwrap()
        .push(v1_field("pages", "i32", json!({})));
    put(
        dir,
        "0002_pages",
        &json!({"name": "0002_pages", "created_at": "", "prev": "0001_initial",
        "snapshot": {"tables": [author, book2], "indexes": [index]},
        "forward": [{"schema": {"AddColumn": {"table": "ad_book", "column": "pages"}}}]}),
    );
}

/// v1 database → adopt → the first v2 run records the checkpoint with no
/// DDL; a v2 file applies and rolls back; rollback stops at the switch.
async fn adopt_and_switch(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    v1_project(&dir);
    crate::migrate::migrate_pool(pool, &dir).await.unwrap();
    let report = super::super::adopt::adopt(&dir, backend_of(pool)).unwrap();
    assert_eq!(report.written, [dir.join("0003_adopt.json")]);
    assert_eq!(report.removed.len(), 2);
    let rep = migrate(pool, &dir, &opts())
        .await
        .unwrap_or_else(|e| panic!("{b:?}: {e}"));
    assert_eq!(rep.applied, ["0003_adopt"]);
    let kinds = Ledger::default_names()
        .event_kinds(pool, ("project", "0003_adopt"))
        .await
        .unwrap();
    assert_eq!(kinds, ["adopted"]);
    put(
        &dir,
        "0004_isbn",
        &v2(
            "0004_isbn",
            &["0003_adopt"],
            json!([{"op": "add_column", "v": 1,
        "table": "ad_book", "column": {"column": "isbn", "ty": "string", "max_length": 20, "nullable": true}}]),
        ),
    );
    assert_eq!(
        migrate(pool, &dir, &opts()).await.unwrap().applied,
        ["0004_isbn"]
    );
    downgrade(pool, &dir, 1, &opts()).await.unwrap();
    assert_eq!(code(downgrade(pool, &dir, 1, &opts()).await), Code::Floor);
    assert!(!columns(pool, "ad_book").await.contains(&"isbn".to_owned()));
}

/// A database behind the v1 head, a fresh one, tables with no ledger, and drift.
async fn adopt_switch_cases(b: B) {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    v1_project(&dir);
    let v1_copy = tempfile::tempdir().unwrap();
    v1_project(&v1_copy.path().join("migrations"));
    // Behind: v1 applied only 0001.
    let Some(behind) = fresh(b).await else { return };
    crate::migrate::migrate_to_pool(
        &behind.pool,
        &v1_copy.path().join("migrations"),
        "0001_initial",
    )
    .await
    .unwrap();
    // Drift: the v1 head plus a column nobody declared.
    let drifted = fresh(b).await.unwrap();
    crate::migrate::migrate_pool(&drifted.pool, &v1_copy.path().join("migrations"))
        .await
        .unwrap();
    raw_execute_pool(
        &drifted.pool,
        "ALTER TABLE ad_book ADD COLUMN stray INTEGER",
        Vec::new(),
    )
    .await
    .unwrap();
    super::super::adopt::adopt(&dir, backend_of(&behind.pool)).unwrap();
    assert_eq!(
        code(migrate(&behind.pool, &dir, &opts()).await),
        Code::Behind
    );
    assert_eq!(
        code(migrate(&drifted.pool, &dir, &opts()).await),
        Code::Drift
    );
    let accepted = Options {
        accept_drift: Some("hand-added".into()),
        ..Options::default()
    };
    migrate(&drifted.pool, &dir, &accepted).await.unwrap();
    let kinds = Ledger::default_names()
        .event_kinds(&drifted.pool, ("project", "0003_adopt"))
        .await
        .unwrap();
    assert_eq!(kinds, ["drift_accepted", "adopted"]);
    // Fresh: the checkpoint creates its tables; rollback still stops there.
    let fresh_db = fresh(b).await.unwrap();
    migrate(&fresh_db.pool, &dir, &opts())
        .await
        .unwrap_or_else(|e| panic!("{b:?}: {e}"));
    assert_eq!(
        columns(&fresh_db.pool, "ad_book").await,
        ["id", "author_id", "title", "pages"]
    );
    assert_eq!(
        code(downgrade(&fresh_db.pool, &dir, 1, &opts()).await),
        Code::Floor
    );
    // Unowned: the table exists, no v1 ledger.
    let unowned = fresh(b).await.unwrap();
    raw_execute_pool(
        &unowned.pool,
        "CREATE TABLE ad_author (id INTEGER)",
        Vec::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        code(migrate(&unowned.pool, &dir, &opts()).await),
        Code::Unowned
    );
}

/// Registry (tenancy) files run on engine v2 from phase 5.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn registry_files_are_refused() {
    let db = fresh(B::Lite).await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    let mut f = v2("0001_org", &[], json!([]));
    f["target"] = json!("registry");
    put(&dir, "0001_org", &f);
    assert_eq!(
        code(migrate(&db.pool, &dir, &opts()).await),
        Code::NotEnabled
    );
}

#[test]
fn a_tenancy_folder_is_not_adopted_yet() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    put(
        &dir,
        "0001_org",
        &json!({"name": "0001_org", "created_at": "", "scope": "registry",
        "snapshot": {"tables": []}, "forward": []}),
    );
    let e = super::super::adopt::adopt(&dir, Backend::Postgres).unwrap_err();
    assert_eq!(e.code, Code::NotEnabled, "{e}");
}

macro_rules! per_backend {
    ($($name:ident),* $(,)?) => {
        mod per_backend {
            $(
                pub(super) mod $name {
                    #[cfg(feature = "postgres")]
                    #[tokio::test]
                    async fn postgres() { super::super::$name(super::super::B::Pg).await }
                    #[cfg(feature = "mysql")]
                    #[tokio::test]
                    async fn mysql() { super::super::$name(super::super::B::My).await }
                    #[cfg(feature = "sqlite")]
                    #[tokio::test]
                    async fn sqlite() { super::super::$name(super::super::B::Lite).await }
                }
            )*
        }
    };
}

per_backend!(
    apply_rollback_reapply,
    edited_file_and_stored_body,
    crash_and_resume,
    runs_exclude_each_other,
    adopt_and_switch,
    adopt_switch_cases,
);
