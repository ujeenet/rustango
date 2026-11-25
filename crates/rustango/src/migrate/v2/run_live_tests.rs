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
    url: String,
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
                // A session left open by a failed test must not hang the run.
                let drop = raw_execute_pool(&admin, &sql, Vec::new());
                if tokio::time::timeout(Duration::from_secs(30), drop)
                    .await
                    .is_err()
                {
                    // `close` would wait for the drop still in flight.
                    eprintln!("leaving database `{name}`: its drop waited 30 s on a lock");
                    return;
                }
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
                url,
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
    let url = format!("{base}/{name}");
    let pool = Pool::connect(&url).await.unwrap();
    Some(Db {
        pool,
        url,
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
        v1_field("email", "string", json!({"max_length": 100, "unique": true})),
        v1_field("rank", "i32", json!({"min": 0, "max": 9}))]});
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
    // MySQL named v1's inline CHECK `<t>_chk_1`: the switch renames it.
    let renamed: &[&str] = if b == B::My { &["renamed"] } else { &[] };
    assert_eq!(kinds, [renamed, &["adopted", "applied"]].concat());
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
    let renamed: &[&str] = if b == B::My { &["renamed"] } else { &[] };
    assert_eq!(
        kinds,
        [&["drift_accepted"][..], renamed, &["adopted", "applied"]].concat()
    );
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

/// The catalog's view of `t`, with what engine v1 and a fresh install may
/// spell differently left out: SQLite constraint names (v1 leaves them
/// unnamed) and whether a unique index backs a constraint.
async fn shape(pool: &Pool, t: &str) -> super::super::catalog::LiveTable {
    let mut live = super::super::catalog::read_table(pool, t)
        .await
        .unwrap()
        .unwrap();
    let lite = pool.dialect().kind() == crate::sql::DialectKind::Sqlite;
    for i in &mut live.indexes {
        if lite && i.unique && !i.primary {
            i.name = format!("unique({})", i.columns.join(","));
        }
    }
    live.indexes.sort_by(|a, b| a.name.cmp(&b.name));
    if lite {
        live.checks.iter_mut().for_each(|c| c.name = None);
        live.fks.iter_mut().for_each(|f| f.name = None);
    }
    live.checks.sort_by(|a, b| a.expr.cmp(&b.expr));
    live.fks.sort_by(|a, b| a.columns.cmp(&b.columns));
    live
}

/// The switch renames objects v1 left under another name (a MySQL
/// `_chk_<n>`) to the checkpoint's, listed by a dry run; afterwards an
/// adopted database and a fresh install of the checkpoint have one catalog.
async fn switch_renames_and_fresh_matches(b: B) {
    let Some(adopted) = fresh(b).await else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    v1_project(&dir);
    crate::migrate::migrate_pool(&adopted.pool, &dir)
        .await
        .unwrap();
    super::super::adopt::adopt(&dir, backend_of(&adopted.pool)).unwrap();
    let dry = Options {
        dry_run: true,
        ..Options::default()
    };
    let plan = migrate(&adopted.pool, &dir, &dry).await.unwrap().planned;
    let renamed = plan.iter().any(|l| l.contains("ad_author_rank_check"));
    assert_eq!(renamed, b == B::My, "{b:?}: {plan:?}");
    assert!(
        rows(&adopted.pool).await.is_empty(),
        "a dry run writes nothing"
    );
    migrate(&adopted.pool, &dir, &opts()).await.unwrap();
    let kinds = Ledger::default_names()
        .event_kinds(&adopted.pool, ("project", "0003_adopt"))
        .await
        .unwrap();
    assert_eq!(
        kinds.contains(&"renamed".to_owned()),
        b == B::My,
        "{kinds:?}"
    );
    let installed = fresh(b).await.unwrap();
    migrate(&installed.pool, &dir, &opts()).await.unwrap();
    for t in ["ad_author", "ad_book"] {
        assert_eq!(
            shape(&adopted.pool, t).await,
            shape(&installed.pool, t).await,
            "{b:?}: {t}"
        );
    }
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

/// A live test that waits on a lock past this fails instead of hanging.
const HANG: Duration = Duration::from_secs(120);

async fn bounded(test: impl std::future::Future<Output = ()>) {
    tokio::time::timeout(HANG, test)
        .await
        .unwrap_or_else(|_| panic!("hung for {HANG:?}: waiting on a lock or a lost connection"));
}

macro_rules! per_backend {
    ($($name:ident),* $(,)?) => {
        mod per_backend {
            $(
                pub(super) mod $name {
                    #[cfg(feature = "postgres")]
                    #[tokio::test]
                    async fn postgres() { super::super::bounded(super::super::$name(super::super::B::Pg)).await }
                    #[cfg(feature = "mysql")]
                    #[tokio::test]
                    async fn mysql() { super::super::bounded(super::super::$name(super::super::B::My)).await }
                    #[cfg(feature = "sqlite")]
                    #[tokio::test]
                    async fn sqlite() { super::super::bounded(super::super::$name(super::super::B::Lite)).await }
                }
            )*
        }
    };
}

/// Boot migrate (6.11): three replicas start together, one migrates and
/// the others wait, then find nothing pending.
async fn three_replicas_one_migrates(b: B) {
    let Some(db) = fresh(b).await else { return };
    let (_root, dir) = project();
    let mut pools = Vec::new();
    for _ in 0..3 {
        pools.push(Pool::connect(&db.url).await.unwrap());
    }
    let o = opts();
    let (r1, r2, r3) = tokio::join!(
        migrate(&pools[0], &dir, &o),
        migrate(&pools[1], &dir, &o),
        migrate(&pools[2], &dir, &o),
    );
    let mut applied: Vec<Vec<String>> = [r1, r2, r3]
        .into_iter()
        .map(|r| r.expect("every replica serves").applied)
        .collect();
    applied.sort();
    assert_eq!(
        applied,
        [
            vec![],
            vec![],
            vec!["0001_author".to_owned(), "0002_bio".to_owned()]
        ]
    );
    for p in pools {
        p.close().await;
    }
}

/// A shutdown signal mid-run stops after the unit in flight; the next
/// start resumes and finishes.
async fn stop_mid_run_then_resume(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let (_root, dir) = project();
    let stopping = Options {
        stop: Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            true,
        ))),
        ..Options::default()
    };
    let e = migrate(pool, &dir, &stopping).await.expect_err("stopped");
    assert!(e.to_string().contains("shutdown signal"), "{e}");
    assert_eq!(columns(pool, "rv_author").await, ["id", "name"]);
    let rep = migrate(pool, &dir, &opts()).await.unwrap();
    assert_eq!(rep.applied, ["0002_bio"]);
    assert_eq!(columns(pool, "rv_author").await, ["id", "name", "bio"]);
}

/// Every 2.6 default passes `check` online on its own backend.
async fn defaults_pass_check_online(b: B) {
    let Some(db) = fresh(b).await else { return };
    let backend = backend_of(&db.pool);
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    let rows = super::super::check::tests::defaults_table(backend);
    super::super::check::tests::project_with(&dir, &rows);
    let found = super::super::check::check(&dir, &[backend], None, Some(&db.pool))
        .await
        .unwrap();
    assert!(found.is_empty(), "{b:?}: {found:#?}");
    // A default the server refuses is an error naming it.
    let bad = tmp.path().join("bad");
    super::super::check::tests::project_with(&bad, &[("i32", None, "(no_such_fn())".into())]);
    let found = super::super::check::check(&bad, &[backend], None, Some(&db.pool))
        .await
        .unwrap();
    assert!(
        found
            .iter()
            .any(|f| f.msg.contains(&format!("on {}", backend.as_str()))
                && f.severity == super::super::check::Severity::Error),
        "{found:#?}"
    );
}

/// ChainFacts (4.9): a v2 project owns a framework table and changes it;
/// the system chain leaves it to the project instead of creating it first.
async fn project_owns_a_framework_table(b: B) {
    #[cfg(feature = "admin")]
    owned_framework_table(b).await;
    #[cfg(not(feature = "admin"))]
    let _ = b;
}

#[cfg(feature = "admin")]
async fn owned_framework_table(b: B) {
    use crate::core::ModelScope;
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let root = tempfile::tempdir().unwrap();
    for scope in [ModelScope::Registry, ModelScope::Tenant] {
        crate::migrate::make_migrations_system(root.path(), scope, None).unwrap();
    }
    let sys = root.path().join("system/migrations");
    let mut snap = crate::migrate::snapshot::SchemaSnapshot::default();
    for e in std::fs::read_dir(&sys).unwrap() {
        let m = crate::migrate::file::load(&e.unwrap().path()).unwrap();
        snap.tables.extend(m.snapshot.tables);
        snap.indexes.extend(m.snapshot.indexes);
    }
    let (table, column) = ("rustango_admin_users", "sessions_revoked_at");
    let t = snap.tables.iter().find(|t| t.name == table).unwrap();
    let state = super::super::make::table_of(t, &snap, &[backend_of(pool)], "test").unwrap();
    let late = state.column(column).unwrap().clone();
    let fields: Vec<Value> = state
        .columns
        .iter()
        .filter(|c| c.column != column)
        .map(|c| serde_json::to_value(c).unwrap())
        .collect();
    let create = json!({"op": "create_table", "v": 1,
        "table": {"name": table, "scope": "tenant", "fields": fields},
        "indexes": state.indexes, "checks": state.checks});
    let add = json!({"op": "add_column", "v": 1, "table": table, "column": late});
    let dir = root.path().join("migrations");
    put(&dir, "0001_users", &v2("0001_users", &[], json!([create])));
    put(
        &dir,
        "0002_revoked",
        &v2("0002_revoked", &["0001_users"], json!([add])),
    );
    let rep = migrate(pool, &dir, &opts()).await.unwrap();
    assert_eq!(rep.applied, ["0001_users", "0002_revoked"]);
    assert!(columns(pool, table).await.iter().any(|c| c == column));
    assert!(migrate(pool, &dir, &opts())
        .await
        .unwrap()
        .applied
        .is_empty());
}

/// Two apps, each with its own ledger, in one database.
async fn two_apps_two_ledgers(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let app = |base: &str, table: &str| {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("migrations");
        let create = json!({"op": "create_table", "v": 1, "table": {"name": table,
            "scope": "tenant", "fields": [
                {"column": "id", "ty": "i64", "primary_key": true, "nullable": false}]}});
        put(
            &dir,
            "0001_initial",
            &v2("0001_initial", &[], json!([create])),
        );
        let o = Options {
            ledger: Some(base.to_owned()),
            ..Options::default()
        };
        (root, dir, o)
    };
    let (_ra, a, oa) = app("__alpha_migrations__", "la_one");
    let (_rb, bdir, ob) = app("__beta_migrations__", "lb_two");
    assert_eq!(
        migrate(pool, &a, &oa).await.unwrap().applied,
        ["0001_initial"]
    );
    assert_eq!(
        migrate(pool, &bdir, &ob).await.unwrap().applied,
        ["0001_initial"]
    );
    for (o, n) in [(&oa, 1), (&ob, 1)] {
        assert_eq!(Ledger::of(o).rows(pool).await.unwrap().len(), n);
    }
    // Rolling one back leaves the other.
    downgrade(pool, &a, 1, &oa).await.unwrap();
    assert!(columns(pool, "la_one").await.is_empty());
    assert_eq!(columns(pool, "lb_two").await, ["id"]);
    assert!(migrate(pool, &bdir, &ob).await.unwrap().applied.is_empty());
}

/// A models snapshot of `rn_t`: `id` plus `cols` as `(name, extra)`.
fn rn_models(cols: &[(&str, Value)]) -> crate::migrate::snapshot::SchemaSnapshot {
    let mut fields = vec![v1_field(
        "id",
        "i64",
        json!({"primary_key": true, "auto": true, "nullable": false}),
    )];
    for (name, extra) in cols {
        fields.push(v1_field(name, "i32", extra.clone()));
    }
    serde_json::from_value(json!({"tables": [{"name": "rn_t", "model": "RnT", "fields": fields}]}))
        .unwrap()
}

fn all_backends() -> super::super::make::MakeOptions {
    super::super::make::MakeOptions {
        backends: vec![Backend::Postgres, Backend::Mysql, Backend::Sqlite],
        ..super::super::make::MakeOptions::default()
    }
}

async fn insert(pool: &Pool, col: &str, v: i64) -> Result<(), crate::sql::ExecError> {
    let d = pool.dialect();
    let sql = format!("INSERT INTO rn_t ({}) VALUES ({v})", d.quote_ident(col));
    raw_execute_pool(pool, &sql, Vec::new()).await.map(|_| ())
}

/// min/max on `rank`, reserved on MySQL, in a three-backend project: the
/// range renders with each backend's quoting and holds on each.
async fn a_range_check_on_a_reserved_name(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    let m = rn_models(&[("rank", json!({"min": 0, "max": 9}))]);
    super::super::make::make(&dir, &dir, &m, &all_backends())
        .unwrap()
        .unwrap();
    migrate(pool, &dir, &opts()).await.unwrap();
    insert(pool, "rank", 5).await.unwrap();
    insert(pool, "rank", 10).await.expect_err("out of range");
}

/// `make --rename` keeps the rows; `--no-rename` writes drop and add.
async fn a_rename_keeps_the_data(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    let make = super::super::make::make;
    make(
        &dir,
        &dir,
        &rn_models(&[("score", json!({}))]),
        &all_backends(),
    )
    .unwrap()
    .unwrap();
    migrate(pool, &dir, &opts()).await.unwrap();
    insert(pool, "score", 7).await.unwrap();
    let renamed = rn_models(&[("points", json!({}))]);
    let e = make(&dir, &dir, &renamed, &all_backends()).unwrap_err();
    assert_eq!(e.code, Code::Rename, "{e}");
    let o = super::super::make::MakeOptions {
        renames: vec![("rn_t".into(), "score".into(), "points".into())],
        ..all_backends()
    };
    make(&dir, &dir, &renamed, &o).unwrap().unwrap();
    assert_eq!(
        migrate(pool, &dir, &opts()).await.unwrap().applied,
        ["0002_rename_column_rn_t"]
    );
    let got: Vec<(i64,)> = crate::sql::raw_query_pool(
        "SELECT COUNT(*) FROM rn_t WHERE points = 7",
        Vec::new(),
        pool,
    )
    .await
    .unwrap();
    assert_eq!(got, [(1,)]);
}

/// `rd_user` with `name`; rows go in through `seed`.
fn rd_users() -> Value {
    json!({"op": "create_table", "v": 1, "table": {"name": "rd_user", "scope": "tenant", "fields": [
        {"column": "id", "ty": "i64", "primary_key": true, "nullable": false},
        {"column": "name", "ty": "string", "max_length": 20, "nullable": true}]}})
}

async fn seed(pool: &Pool, n: i64) {
    for i in 1..=n {
        raw_execute_pool(
            pool,
            &format!("INSERT INTO rd_user (id, name) VALUES ({i}, 'u{i}')"),
            Vec::new(),
        )
        .await
        .unwrap();
    }
}

async fn count(pool: &Pool, sql: &str) -> i64 {
    let r: Vec<(i64,)> = crate::sql::raw_query_pool(sql, Vec::new(), pool)
        .await
        .unwrap();
    r[0].0
}

/// Example A without batching (2.5): add nullable, backfill, make required.
fn example_a() -> Value {
    json!([
        {"op": "add_column", "v": 1, "table": "rd_user",
         "column": {"column": "locale", "ty": "string", "max_length": 8, "nullable": true}},
        {"op": "data_sql", "v": 1, "id": "backfill_locale_v1",
         "tables": {"read": ["rd_user"], "write": ["rd_user.locale"]},
         "up": {"any": "UPDATE {{t:rd_user}} SET {{c:locale}} = {{?}} WHERE {{c:locale}} IS NULL"},
         "binds": [{"ty": "string", "value": "en"}],
         "down": {"noop": "the column is dropped by the reverse of op 0"},
         "on_fresh": "skip"},
        {"op": "alter_column", "v": 1, "table": "rd_user", "column": "locale",
         "changes": {"nullable": {"from": true, "to": false}}}
    ])
}

fn rd_project() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    put(
        &dir,
        "0001_users",
        &v2("0001_users", &[], json!([rd_users()])),
    );
    (root, dir)
}

/// Upgrade with rows, downgrade (the noop passes), re-apply, and fresh.
async fn example_a_upgrade_downgrade_fresh(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let (_root, dir) = rd_project();
    migrate(pool, &dir, &opts()).await.unwrap();
    seed(pool, 3).await;
    put(
        &dir,
        "0002_locale",
        &v2("0002_locale", &["0001_users"], example_a()),
    );
    assert_eq!(
        migrate(pool, &dir, &opts()).await.unwrap().applied,
        ["0002_locale"]
    );
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM rd_user WHERE locale = 'en'").await,
        3
    );
    let t = super::super::catalog::read_table(pool, "rd_user")
        .await
        .unwrap()
        .unwrap();
    assert!(t.columns.iter().any(|c| c.name == "locale" && !c.nullable));
    assert_eq!(
        downgrade(pool, &dir, 1, &opts()).await.unwrap().rolled_back,
        ["0002_locale"]
    );
    assert_eq!(columns(pool, "rd_user").await, ["id", "name"]);
    migrate(pool, &dir, &opts()).await.unwrap();
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM rd_user WHERE locale = 'en'").await,
        3
    );
    // Fresh: the whole chain on an empty database.
    let Some(db2) = fresh(b).await else { return };
    assert_eq!(
        migrate(&db2.pool, &dir, &opts()).await.unwrap().applied,
        ["0001_users", "0002_locale"]
    );
    // SQLite runs Example A as 3 units: add, data, rebuild.
    if b == B::Lite {
        let p = super::super::load::load_project(&dir).unwrap();
        let units = super::super::render::render_project(&p, Backend::Sqlite).unwrap();
        let kinds: Vec<UnitKind> = units[1].1.iter().map(|u| u.kind).collect();
        assert_eq!(kinds, [UnitKind::Tx, UnitKind::Data, UnitKind::Rebuild]);
    }
}

/// A failure after the data write rolls the unit and its ledger move back;
/// the next run resumes from the unit.
async fn a_failure_after_the_data_write_rolls_back(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let (_root, dir) = rd_project();
    migrate(pool, &dir, &opts()).await.unwrap();
    seed(pool, 2).await;
    put(
        &dir,
        "0002_locale",
        &v2("0002_locale", &["0001_users"], example_a()),
    );
    let crashing = Options {
        crash: Some(Crash::AfterData(1)),
        ..Options::default()
    };
    migrate(pool, &dir, &crashing).await.expect_err("crashed");
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM rd_user WHERE locale IS NULL").await,
        2
    );
    let row = Ledger::default_names()
        .rows(pool)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.id == "0002_locale")
        .unwrap();
    assert_eq!(
        (row.state, row.pos),
        (RowState::Failed, 0),
        "the add_column unit only"
    );
    assert_eq!(
        migrate(pool, &dir, &opts()).await.unwrap().applied,
        ["0002_locale"]
    );
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM rd_user WHERE locale = 'en'").await,
        2
    );
}

/// Data steps with nothing to run here are refused together, before the
/// lock, the system chain and the first statement.
async fn missing_steps_run_nothing(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let other = if b == B::Pg { "mysql" } else { "postgres" };
    let (_root, dir) = rd_project();
    let step = |id: &str| {
        json!({"op": "data_sql", "v": 1, "id": id,
            "tables": {"read": ["rd_user"], "write": ["rd_user.name"]},
            "up": {other: "UPDATE {{t:rd_user}} SET {{c:name}} = 'x'"},
            "down": {"noop": "test"}, "allow": [{"code": "MV2-E028", "reason": "test"}]})
    };
    put(
        &dir,
        "0002_a",
        &v2("0002_a", &["0001_users"], json!([step("a_v1")])),
    );
    put(
        &dir,
        "0003_b",
        &v2("0003_b", &["0002_a"], json!([step("b_v1")])),
    );
    let e = migrate(pool, &dir, &opts()).await.expect_err("refused");
    assert_eq!(e.code(), Some(Code::Unresolved), "{e}");
    let msg = e.to_string();
    assert!(msg.contains("`a_v1`") && msg.contains("`b_v1`"), "{msg}");
    assert!(
        columns(pool, "rd_user").await.is_empty(),
        "0001 did not run"
    );
    assert!(rows(pool).await.is_empty());
    assert!(
        !crate::migrate::try_table_exists_here(pool, crate::migrate::SYSTEM_LEDGER_TABLE)
            .await
            .unwrap()
    );
    // The CLI says the same.
    let mut out = Vec::new();
    let e = crate::migrate::manage::run_with_writer(
        pool,
        &dir,
        ["dbmigrate".to_owned(), "apply".to_owned()],
        &mut out,
    )
    .await
    .unwrap_err();
    assert!(e.to_string().contains("MV2-E029"), "{e}");
}

/// Any statement error rolls the unit back even when the step goes on; a
/// savepoint is the way to recover on purpose (5.4).
async fn a_swallowed_error_rolls_the_unit_back(b: B) {
    use super::super::step::{run_step, Scope, StepCtx, UnitError};
    use futures_util::future::BoxFuture;
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    raw_execute_pool(
        pool,
        "CREATE TABLE sw_t (id INTEGER PRIMARY KEY)",
        Vec::new(),
    )
    .await
    .unwrap();
    fn swallow<'t>(
        savepoint: bool,
    ) -> impl for<'a> FnOnce(
        &'a mut StepCtx<'t>,
    ) -> BoxFuture<'a, Result<(), super::super::step::StepError>> {
        move |ctx| {
            Box::pin(async move {
                ctx.execute("INSERT INTO sw_t (id) VALUES (1)", Vec::new())
                    .await?;
                if savepoint {
                    let _ = ctx
                        .savepoint(|c| {
                            Box::pin(async move {
                                c.execute("INSERT INTO no_such_t (id) VALUES (1)", Vec::new())
                                    .await
                                    .map(drop)
                            })
                        })
                        .await;
                } else {
                    let _ = ctx
                        .execute("INSERT INTO no_such_t (id) VALUES (1)", Vec::new())
                        .await;
                }
                Ok(())
            })
        }
    }
    // As the runner does: commit only an Ok step, roll back otherwise, and
    // end the transaction before any assertion can unwind past it.
    let mut conn = pool.acquire().await.unwrap();
    conn.close_on_drop();
    let tx = conn.begin().await.unwrap();
    let (tx, r) = run_step(tx, Scope::default(), swallow(false)).await;
    let poisoned = matches!(r, Err(UnitError::Poisoned { .. }));
    if r.is_ok() {
        tx.commit().await.unwrap();
    } else {
        tx.rollback().await.unwrap();
    }
    let tx = conn.begin().await.unwrap();
    let (tx, saved) = run_step(tx, Scope::default(), swallow(true)).await;
    if saved.is_ok() {
        tx.commit().await.unwrap();
    } else {
        tx.rollback().await.unwrap();
    }
    drop(conn);
    assert!(poisoned, "the swallowed error must poison the unit: {r:?}");
    assert!(saved.is_ok(), "{saved:?}");
    assert_eq!(count(pool, "SELECT COUNT(*) FROM sw_t").await, 1);
}

/// Inside a step the app's pools are out of reach (5.4).
async fn the_escape_guard_refuses_pools(b: B) {
    use super::super::step::{run_step, Scope, StepCtx};
    use futures_util::future::BoxFuture;
    let Some(db) = fresh(b).await else { return };
    let pool = db.pool.clone();
    fn escape<'t>(
        pool: Pool,
    ) -> impl for<'a> FnOnce(
        &'a mut StepCtx<'t>,
    ) -> BoxFuture<'a, Result<(), super::super::step::StepError>> {
        // No panic inside the step: the unit's transaction is still open.
        move |_ctx| {
            Box::pin(async move {
                let refused = |r: Result<(), crate::sql::ExecError>| {
                    matches!(r, Err(crate::sql::ExecError::InMigrationStep))
                };
                let checks = [
                    (
                        "raw_execute_pool",
                        refused(
                            raw_execute_pool(&pool, "SELECT 1", Vec::new())
                                .await
                                .map(drop),
                        ),
                    ),
                    (
                        "raw_query_pool",
                        refused(
                            crate::sql::raw_query_pool::<(i64,)>("SELECT 1", Vec::new(), &pool)
                                .await
                                .map(drop),
                        ),
                    ),
                    (
                        "transaction_pool",
                        refused(crate::sql::transaction_pool(&pool).await.map(drop)),
                    ),
                    ("acquire", refused(pool.acquire().await.map(drop))),
                    ("databases::get", crate::databases::get("default").is_none()),
                    ("databases::default", crate::databases::default().is_none()),
                ];
                match checks.iter().find(|(_, ok)| !ok) {
                    None => Ok(()),
                    Some((what, _)) => Err(crate::sql::ExecError::Driver(sqlx::Error::Protocol(
                        format!("`{what}` reached a pool inside the step"),
                    ))
                    .into()),
                }
            })
        }
    }
    let mut conn = db.pool.acquire().await.unwrap();
    conn.close_on_drop();
    let tx = conn.begin().await.unwrap();
    let (tx, r) = run_step(tx, Scope::default(), escape(pool.clone())).await;
    tx.rollback().await.unwrap();
    drop(conn);
    assert!(r.is_ok(), "{r:?}");
    // Outside a step the pool works.
    raw_execute_pool(&pool, "SELECT 1", Vec::new())
        .await
        .unwrap();
}

/// A data unit blocked by a held lock fails after the lock timeout, and
/// the next run resumes it.
async fn a_blocked_data_unit_times_out(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let (_root, dir) = rd_project();
    put(
        &dir,
        "0002_locale",
        &v2("0002_locale", &["0001_users"], json!([example_a()[0]])),
    );
    migrate(pool, &dir, &opts()).await.unwrap();
    seed(pool, 1).await;
    let mut fill = example_a()[1].clone();
    fill["allow"] = json!([{"code": "MV2-E028", "reason": "test"}]);
    put(
        &dir,
        "0003_fill",
        &v2("0003_fill", &["0002_locale"], json!([fill])),
    );
    // Another session holds the row (SQLite: the database) for writing.
    let holder = Pool::connect(&db.url).await.unwrap();
    let mut hconn = holder.acquire().await.unwrap();
    let mut htx = hconn.begin().await.unwrap();
    crate::sql::raw_execute_tx(
        &mut htx,
        "UPDATE rd_user SET name = 'held' WHERE id = 1",
        Vec::new(),
    )
    .await
    .unwrap();
    let quick = Options {
        data_lock_timeout: Some(Duration::from_secs(1)),
        ..Options::default()
    };
    let started = std::time::Instant::now();
    let r = tokio::time::timeout(Duration::from_secs(30), migrate(pool, &dir, &quick)).await;
    let waited = started.elapsed();
    // Release the holder before any assertion can unwind past it.
    htx.rollback().await.unwrap();
    drop(hconn);
    holder.close().await;
    let r = r.unwrap_or_else(|_| panic!("the lock timeout did not end the wait in 30 s"));
    assert!(r.is_err(), "blocked");
    assert!(waited < Duration::from_secs(20), "{waited:?}");
    assert_eq!(
        migrate(pool, &dir, &opts()).await.unwrap().applied,
        ["0003_fill"]
    );
}

/// `0001_author` applied, `0002_bio` failed after its first unit.
async fn crashed_bio(pool: &Pool, dir: &Path) {
    migrate(pool, dir, &opts()).await.unwrap_or_default();
    let crashing = Options {
        crash: Some(Crash::BeforeUnit(1)),
        ..Options::default()
    };
    put(
        dir,
        "0002_bio",
        &v2("0002_bio", &["0001_author"], add_bio()),
    );
    migrate(pool, dir, &crashing).await.expect_err("crashed");
}

fn author_only() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    put(
        &dir,
        "0001_author",
        &v2("0001_author", &[], json!([author()])),
    );
    (root, dir)
}

async fn ledger_snapshot(pool: &Pool) -> Vec<(String, RowState, i64, Vec<String>)> {
    let l = Ledger::default_names();
    let mut out = Vec::new();
    for r in l.rows(pool).await.unwrap() {
        let kinds = l.event_kinds(pool, (&r.app, &r.id)).await.unwrap();
        out.push((r.id, r.state, r.pos, kinds));
    }
    out
}

/// Dry runs never write: no resume of a crashed row, no ledger row and no
/// DDL for a fresh adopt (review fix 1).
async fn dry_runs_never_write(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let (_root, dir) = author_only();
    crashed_bio(pool, &dir).await;
    let before = ledger_snapshot(pool).await;
    assert_eq!(before.last().unwrap().1, RowState::Failed);
    let cols = columns(pool, "rv_author").await;
    let dry = Options {
        dry_run: true,
        ..Options::default()
    };
    let rep = migrate(pool, &dir, &dry).await.unwrap();
    assert!(
        rep.planned.iter().any(|l| l.contains("rv_author_name_idx")),
        "{rep:?}"
    );
    assert_eq!(ledger_snapshot(pool).await, before);
    assert_eq!(columns(pool, "rv_author").await, cols);
    // A fresh adopt, with and without the v2 ledger table.
    for with_ledger in [true, false] {
        let Some(db2) = fresh(b).await else { return };
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("migrations");
        v1_project(&dir);
        super::super::adopt::adopt(&dir, backend_of(&db2.pool)).unwrap();
        if with_ledger {
            Ledger::default_names().ensure(&db2.pool).await.unwrap();
        }
        let rep = migrate(&db2.pool, &dir, &dry).await.unwrap();
        assert!(
            rep.planned.iter().any(|l| l.contains("CREATE TABLE")),
            "{rep:?}"
        );
        assert!(rows(&db2.pool).await.is_empty());
        assert!(columns(&db2.pool, "ad_author").await.is_empty());
    }
}

/// `--to` rolls back what the ledger applied after the target, in `seq`
/// order across apps (review fix 2).
async fn down_to_follows_the_ledger(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    let table = |name: &str| {
        json!({"op": "create_table", "v": 1, "table": {"name": name, "scope": "tenant",
            "fields": [{"column": "id", "ty": "i64", "primary_key": true, "nullable": false}]}})
    };
    put(
        &dir,
        "0001_pa",
        &v2("0001_pa", &[], json!([table("dt_pa")])),
    );
    let blog = root.path().join("blog/migrations");
    let mut b1 = v2("0001_bl", &[], json!([table("dt_bl")]));
    b1["app"] = json!("blog");
    b1["depends_on"] = json!(["project:0001_pa"]);
    put(&blog, "0001_bl", &b1);
    let mut p2 = v2("0002_pb", &["0001_pa"], json!([table("dt_pb")]));
    p2["depends_on"] = json!(["blog:0001_bl"]);
    put(&dir, "0002_pb", &p2);
    assert_eq!(
        migrate(pool, &dir, &opts()).await.unwrap().applied,
        ["0001_pa", "0001_bl", "0002_pb"]
    );
    let rep = downgrade_to(pool, &dir, "project:0002_pb", &opts())
        .await
        .unwrap();
    assert!(rep.rolled_back.is_empty(), "{rep:?}");
    let mut out = Vec::new();
    crate::migrate::manage::run_with_writer(
        pool,
        &dir,
        ["dbmigrate", "down", "--to", "blog:0001_bl"].map(String::from),
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "Rolled back 0002_pb\n");
    assert_eq!(columns(pool, "dt_bl").await, ["id"]);
}

/// A rollback cut off after a statement resumes from its probe (review fix 3).
async fn a_cut_off_rollback_resumes_by_probe(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let (_root, dir) = project();
    migrate(pool, &dir, &opts()).await.unwrap();
    let crashing = Options {
        crash: Some(Crash::AfterStatement(0)),
        ..Options::default()
    };
    let cut = downgrade(pool, &dir, 1, &crashing).await;
    // The first reverse unit is autocommit on MySQL (DDL) and PG (DROP INDEX
    // CONCURRENTLY), where the cut lands; SQLite has none.
    assert_eq!(cut.is_err(), b != B::Lite, "{cut:?}");
    if cut.is_err() {
        // Zero more steps: only the half-finished rollback finishes.
        assert_eq!(
            downgrade(pool, &dir, 0, &opts()).await.unwrap().rolled_back,
            ["0002_bio"]
        );
    }
    assert_eq!(columns(pool, "rv_author").await, ["id", "name"]);
}

/// Every inverse and blocker is planned before the first statement
/// (review fix 4), and a failed row blocks rollback (review fix 5).
async fn rollback_is_planned_up_front(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let (_root, dir) = project();
    let drop = json!([{"op": "drop_column", "v": 1, "table": "rv_author", "column": "bio",
        "was": {"column": "bio", "ty": "string", "max_length": 80, "nullable": true}}]);
    put(&dir, "0003_drop", &v2("0003_drop", &["0002_bio"], drop));
    let nick = json!([{"op": "add_column", "v": 1, "table": "rv_author",
        "column": {"column": "nick", "ty": "string", "max_length": 20, "nullable": true}}]);
    put(&dir, "0004_nick", &v2("0004_nick", &["0003_drop"], nick));
    migrate(pool, &dir, &opts()).await.unwrap();
    assert_eq!(code(downgrade(pool, &dir, 2, &opts()).await), Code::Blocked);
    assert_eq!(columns(pool, "rv_author").await, ["id", "name", "nick"]);
    assert_eq!(rows(pool).await.len(), 4);
    // A failed row: rollback waits for it.
    let Some(db2) = fresh(b).await else { return };
    let (_root, dir) = author_only();
    crashed_bio(&db2.pool, &dir).await;
    let before = ledger_snapshot(&db2.pool).await;
    assert_eq!(
        code(downgrade(&db2.pool, &dir, 1, &opts()).await),
        Code::State
    );
    assert_eq!(ledger_snapshot(&db2.pool).await, before);
}

/// A custom ledger name keeps its case on every query (review fix 11), and
/// adopt reads the app's own v1 ledger (review fix 7).
async fn custom_ledgers_keep_their_names(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let caps = Options {
        ledger: Some("__MyApp_Migrations__".to_owned()),
        ..Options::default()
    };
    let (_root, dir) = project();
    assert_eq!(migrate(pool, &dir, &caps).await.unwrap().applied.len(), 2);
    assert_eq!(Ledger::of(&caps).rows(pool).await.unwrap().len(), 2);
    downgrade(pool, &dir, 1, &caps).await.unwrap();
    assert_eq!(
        migrate(pool, &dir, &caps).await.unwrap().applied,
        ["0002_bio"]
    );
    // An app whose v1 ledger is its own: the default one has none of its names.
    let Some(db2) = fresh(b).await else { return };
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    v1_project(&dir);
    crate::migrate::Builder::new()
        .ledger("__alpha_migrations__")
        .migrate(&db2.pool, &dir)
        .await
        .unwrap();
    super::super::adopt::adopt(&dir, backend_of(&db2.pool)).unwrap();
    let alpha = Options {
        ledger: Some("__alpha_migrations__".to_owned()),
        ..Options::default()
    };
    assert_eq!(
        migrate(&db2.pool, &dir, &alpha).await.unwrap().applied,
        ["0003_adopt"]
    );
    let kinds = Ledger::of(&alpha)
        .event_kinds(&db2.pool, ("project", "0003_adopt"))
        .await
        .unwrap();
    assert!(kinds.contains(&"adopted".to_owned()), "{kinds:?}");
}

/// A drop cut off by a kill leaves an invalid index on PG: resume runs the
/// drop again (review fix 10).
async fn an_invalid_index_left_by_a_drop_is_dropped_again(b: B) {
    if b != B::Pg {
        return;
    }
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let (_root, dir) = project();
    migrate(pool, &dir, &opts()).await.unwrap();
    let drop = json!([{"op": "drop_index", "v": 1, "table": "rv_author", "name": "rv_author_name_idx",
        "was": {"name": "rv_author_name_idx", "columns": ["name"]}}]);
    put(&dir, "0003_drop", &v2("0003_drop", &["0002_bio"], drop));
    let crashing = Options {
        crash: Some(Crash::AfterStart(0)),
        ..Options::default()
    };
    migrate(pool, &dir, &crashing).await.expect_err("cut off");
    // What a killed CONCURRENTLY drop leaves: the index, invalid.
    let marked = raw_execute_pool(
        pool,
        "UPDATE pg_index SET indisvalid = false WHERE indexrelid = 'rv_author_name_idx'::regclass",
        Vec::new(),
    )
    .await;
    if marked.is_err() {
        eprintln!("skipping: the test role cannot mark an index invalid");
        return;
    }
    assert_eq!(
        migrate(pool, &dir, &opts()).await.unwrap().applied,
        ["0003_drop"]
    );
}

/// Drift tells a unique index from a unique constraint (review fix 8).
async fn drift_sees_a_constraint_kind(b: B) {
    if b == B::My {
        return;
    }
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let (_root, dir) = author_only();
    migrate(pool, &dir, &opts()).await.unwrap();
    raw_execute_pool(
        pool,
        "CREATE UNIQUE INDEX rv_author_name_key ON rv_author (name)",
        Vec::new(),
    )
    .await
    .unwrap();
    let p = super::super::load::load_project(&dir).unwrap();
    let mut state = super::super::load::replay(&p).unwrap().states[&Target::Tenant].clone();
    let mut t = (*state.tables["rv_author"]).clone();
    t.indexes.push(super::super::state::IndexDef {
        name: "rv_author_name_key".into(),
        columns: vec!["name".into()],
        unique: true,
        method: "btree".into(),
        predicate: None,
        include: Vec::new(),
        skipped_on: Vec::new(),
        constraint: true,
    });
    state
        .tables
        .insert("rv_author".into(), std::sync::Arc::new(t));
    let diff = super::super::probe::drift(pool, &state).await.unwrap();
    assert!(diff.iter().any(|m| m.what == "constraint"), "{diff:?}");
}

/// A run dropped mid-unit (SIGTERM, task abort, a timeout) frees the
/// migrate lock: the next run takes it at once.
async fn a_dropped_run_frees_the_lock(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let (_root, dir) = rd_project();
    put(
        &dir,
        "0002_locale",
        &v2("0002_locale", &["0001_users"], json!([example_a()[0]])),
    );
    migrate(pool, &dir, &opts()).await.unwrap();
    seed(pool, 1).await;
    let mut fill = example_a()[1].clone();
    fill["allow"] = json!([{"code": "MV2-E028", "reason": "test"}]);
    put(
        &dir,
        "0003_fill",
        &v2("0003_fill", &["0002_locale"], json!([fill])),
    );
    // Block the data unit on a held row, then drop the run while it waits.
    let holder = Pool::connect(&db.url).await.unwrap();
    let mut hconn = holder.acquire().await.unwrap();
    let mut htx = hconn.begin().await.unwrap();
    crate::sql::raw_execute_tx(
        &mut htx,
        "UPDATE rd_user SET name = 'held' WHERE id = 1",
        Vec::new(),
    )
    .await
    .unwrap();
    let slow = Options {
        data_lock_timeout: Some(Duration::from_secs(60)),
        ..Options::default()
    };
    let cut = tokio::time::timeout(Duration::from_secs(3), migrate(pool, &dir, &slow)).await;
    // The dropped run's statement may still wait server-side: the lock must
    // be free anyway, while the row is still held.
    let lock = crate::migrate::with_lock_timeout(
        Duration::from_secs(3),
        crate::migrate::with_migrate_lock_held_v2(V2Token(()), pool, |_| async { Ok(()) }),
    )
    .await;
    htx.rollback().await.unwrap();
    drop(hconn);
    holder.close().await;
    assert!(cut.is_err(), "the run was still waiting");
    lock.unwrap_or_else(|e| panic!("the dropped run kept the migrate lock: {e}"));
    let next =
        crate::migrate::with_lock_timeout(Duration::from_secs(5), migrate(pool, &dir, &opts()))
            .await;
    let rep = next.unwrap_or_else(|e| panic!("the dropped run kept the lock: {e}"));
    assert_eq!(rep.applied, ["0003_fill"]);
}

// ---- data_rust (2b) ----

use super::super::step::{Batch, StepCtx, StepError, StepResult};

/// `st_user` (id, name, greeting) gets "hi" where the greeting is unset;
/// down unsets it again.
#[crate::data_step(app = "steps", id = "greet_v1")]
impl GreetV1 {
    async fn up(ctx: &mut StepCtx<'_>, batch: Batch) -> StepResult {
        let t = ctx.table("st_user")?;
        t.query()
            .filter_is_null("greeting", true)
            .update(ctx, &[("greeting", "hi".into())])
            .await?;
        Ok(batch.done())
    }

    async fn down(ctx: &mut StepCtx<'_>, batch: Batch) -> StepResult {
        let t = ctx.table("st_user")?;
        t.query()
            .filter_is_null("greeting", false)
            .update(ctx, &[("greeting", crate::core::SqlValue::Null)])
            .await?;
        Ok(batch.done())
    }
}

/// Writes, then swallows a failed statement: the unit must roll back.
#[crate::data_step(app = "steps", id = "swallow_v1")]
impl SwallowV1 {
    async fn up(ctx: &mut StepCtx<'_>, batch: Batch) -> StepResult {
        let t = ctx.table("st_user")?;
        t.query().update(ctx, &[("greeting", "x".into())]).await?;
        let _ = ctx
            .raw(
                "UPDATE {{t:st_user}} SET no_such_column = 1",
                Vec::new(),
                None,
            )
            .await;
        Ok(batch.done())
    }
}

/// Reaches for the app's default pool inside the step.
#[crate::data_step(app = "steps", id = "escape_v1")]
impl EscapeV1 {
    async fn up(_ctx: &mut StepCtx<'_>, batch: Batch) -> StepResult {
        match crate::databases::default() {
            None => Err(StepError::Message(
                "databases::default() is refused in a step".into(),
            )),
            Some(_) => Ok(batch.done()),
        }
    }
}

static PANICKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Panics on its first run in this process, then succeeds.
#[crate::data_step(app = "steps", id = "panic_once_v1")]
impl PanicOnceV1 {
    async fn up(ctx: &mut StepCtx<'_>, batch: Batch) -> StepResult {
        let t = ctx.table("st_user")?;
        t.query().update(ctx, &[("greeting", "p".into())]).await?;
        assert!(
            PANICKED.swap(true, std::sync::atomic::Ordering::SeqCst),
            "first run panics"
        );
        Ok(batch.done())
    }
}

/// Touches a table outside its op's footprint.
#[crate::data_step(app = "steps", id = "outside_v1")]
impl OutsideV1 {
    async fn up(ctx: &mut StepCtx<'_>, batch: Batch) -> StepResult {
        ctx.table("st_other")?;
        Ok(batch.done())
    }
}

/// [`v2`] for the app the test steps are registered under.
fn vs(id: &str, parents: &[&str], ops: Value) -> Value {
    let mut f = v2(id, parents, ops);
    f["app"] = json!("steps");
    f
}

fn st_users() -> Value {
    json!({"op": "create_table", "v": 1, "table": {"name": "st_user", "scope": "tenant", "fields": [
        {"column": "id", "ty": "i64", "primary_key": true, "auto": true, "nullable": false},
        {"column": "name", "ty": "string", "max_length": 20, "nullable": true},
        {"column": "greeting", "ty": "string", "max_length": 20, "nullable": true}]}})
}

fn rust_op(id: &str, down: Value) -> Value {
    json!({"op": "data_rust", "v": 1, "id": id,
        "tables": {"read": ["st_user"], "write": ["st_user.greeting"]}, "down": down,
        "allow": [{"code": "MV2-E028", "reason": "test"}]})
}

/// `0001` creates `st_user`; rows go in; `0002` runs `ops`.
async fn st_project(pool: &Pool, ops: Value) -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    put(
        &dir,
        "0001_users",
        &vs("0001_users", &[], json!([st_users()])),
    );
    migrate(pool, &dir, &opts()).await.unwrap();
    for n in ["a", "b"] {
        raw_execute_pool(
            pool,
            &format!("INSERT INTO st_user (name) VALUES ('{n}')"),
            Vec::new(),
        )
        .await
        .unwrap();
    }
    put(&dir, "0002_step", &vs("0002_step", &["0001_users"], ops));
    (root, dir)
}

/// A Rust step applies, rolls back by its `down`, and re-applies; its
/// code hash goes in the ledger.
async fn a_rust_step_runs_both_ways(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let (_root, dir) = st_project(pool, json!([rust_op("greet_v1", json!("rust"))])).await;
    assert_eq!(
        migrate(pool, &dir, &opts()).await.unwrap().applied,
        ["0002_step"]
    );
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM st_user WHERE greeting = 'hi'").await,
        2
    );
    let hashes = Ledger::default_names().step_hashes(pool).await.unwrap();
    let want = super::super::step::registered("steps", "greet_v1")[0].code_hash();
    assert_eq!(
        hashes.get(&("steps".into(), "greet_v1".into())),
        Some(&want)
    );
    downgrade(pool, &dir, 1, &opts()).await.unwrap();
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM st_user WHERE greeting IS NULL").await,
        2
    );
    migrate(pool, &dir, &opts()).await.unwrap();
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM st_user WHERE greeting = 'hi'").await,
        2
    );
}

/// A swallowed statement error rolls the unit back; the app's pools are
/// out of reach; a footprint miss fails the unit (5.3, 5.4).
async fn rust_step_guards(b: B) {
    for (id, want) in [
        ("swallow_v1", "rolled back"),
        ("escape_v1", "databases::default()"),
        ("outside_v1", "not in the op's `tables`"),
    ] {
        let Some(db) = fresh(b).await else { return };
        let pool = &db.pool;
        // A registered default pool, so only the guard can refuse it.
        crate::databases::register("default", pool.clone());
        let (_root, dir) = st_project(pool, json!([rust_op(id, json!({"noop": "test"}))])).await;
        let e = migrate(pool, &dir, &opts()).await.expect_err(id);
        assert!(e.to_string().contains(want), "{id}: {e}");
        assert_eq!(
            count(pool, "SELECT COUNT(*) FROM st_user WHERE greeting IS NULL").await,
            2,
            "{id}: nothing stayed written"
        );
    }
}

/// A panicking step leaves the row failed and resumable.
async fn a_panicking_step_is_resumable(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    PANICKED.store(false, std::sync::atomic::Ordering::SeqCst);
    let (_root, dir) = st_project(
        pool,
        json!([rust_op("panic_once_v1", json!({"noop": "t"}))]),
    )
    .await;
    let e = migrate(pool, &dir, &opts()).await.expect_err("panicked");
    assert!(e.to_string().contains("panicked"), "{e}");
    assert_eq!(rows(pool).await.last().unwrap().1, RowState::Failed);
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM st_user WHERE greeting IS NULL").await,
        2
    );
    assert_eq!(
        migrate(pool, &dir, &opts()).await.unwrap().applied,
        ["0002_step"]
    );
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM st_user WHERE greeting = 'p'").await,
        2
    );
}

/// A step that is not registered is refused before the first statement,
/// naming near matches and the binary.
async fn a_missing_rust_step_runs_nothing(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    put(
        &dir,
        "0001_users",
        &vs(
            "0001_users",
            &[],
            json!([st_users(), rust_op("greet_v2", json!("rust"))]),
        ),
    );
    let e = migrate(pool, &dir, &opts()).await.expect_err("refused");
    assert_eq!(e.code(), Some(Code::Unresolved), "{e}");
    let msg = e.to_string();
    assert!(msg.contains("near: steps:greet_v1"), "{msg}");
    let bin = std::env::current_exe().unwrap();
    assert!(
        msg.contains(&*bin.file_name().unwrap().to_string_lossy()),
        "{msg}"
    );
    assert!(columns(pool, "st_user").await.is_empty());
    assert!(rows(pool).await.is_empty());
}

/// A changed step: a warning once applied; a refusal for a failed row
/// until `--accept-step-change` records the new hash.
async fn a_changed_step_is_warned_or_refused(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let (_root, dir) = st_project(pool, json!([rust_op("greet_v1", json!("rust"))])).await;
    migrate(pool, &dir, &opts()).await.unwrap();
    let ledger = Ledger::default_names();
    let k = ("steps", "0002_step");
    ledger
        .event(pool, STEP_HASH, k, None, "test", Some("greet_v1 0000"))
        .await
        .unwrap();
    let rep = migrate(pool, &dir, &opts()).await.unwrap();
    assert!(
        rep.warnings.iter().any(|w| w.contains("greet_v1")),
        "{rep:?}"
    );
    let found = super::super::check::check(&dir, &[backend_of(pool)], None, Some(pool))
        .await
        .unwrap();
    assert!(
        found.iter().any(|f| f.msg.contains("changed after it ran")),
        "{found:#?}"
    );
    // Accepting a change is for a partial or failed step: an applied one only warns.
    let accept_applied = Options {
        accept_step_change: Some(("greet_v1".into(), "reviewed".into())),
        ..Options::default()
    };
    assert_eq!(
        code(migrate(pool, &dir, &accept_applied).await),
        Code::StepChanged
    );
    // A failed row whose step changed.
    let Some(db2) = fresh(b).await else { return };
    let pool = &db2.pool;
    let (_root, dir) = st_project(pool, json!([rust_op("greet_v1", json!("rust"))])).await;
    let crashing = Options {
        crash: Some(Crash::AfterData(0)),
        ..Options::default()
    };
    migrate(pool, &dir, &crashing).await.expect_err("crashed");
    ledger
        .event(pool, STEP_HASH, k, None, "test", Some("greet_v1 0000"))
        .await
        .unwrap();
    assert_eq!(code(migrate(pool, &dir, &opts()).await), Code::StepChanged);
    let accept = Options {
        accept_step_change: Some(("greet_v1".into(), "reviewed".into())),
        ..Options::default()
    };
    assert_eq!(
        migrate(pool, &dir, &accept).await.unwrap().applied,
        ["0002_step"]
    );
    let kinds = ledger.event_kinds(pool, k).await.unwrap();
    assert!(
        kinds.contains(&"step_change_accepted".to_owned()),
        "{kinds:?}"
    );
}

/// `migrate::v2::testing`: a database before the step, then up/down and
/// the resume check.
async fn the_testing_helper_runs_a_step(b: B) {
    let Some(db) = fresh(b).await else { return };
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    put(
        &dir,
        "0001_users",
        &vs("0001_users", &[], json!([st_users()])),
    );
    put(
        &dir,
        "0002_step",
        &vs(
            "0002_step",
            &["0001_users"],
            json!([rust_op("greet_v1", json!("rust"))]),
        ),
    );
    let t = super::super::testing::at(&db.pool, &dir, "steps", "greet_v1")
        .await
        .unwrap();
    raw_execute_pool(
        t.pool(),
        "INSERT INTO st_user (name) VALUES ('a')",
        Vec::new(),
    )
    .await
    .unwrap();
    t.assert_prefix_down().await.unwrap();
    t.assert_resume_equals_full().await.unwrap();
    assert_eq!(
        count(
            t.pool(),
            "SELECT COUNT(*) FROM st_user WHERE greeting = 'hi'"
        )
        .await,
        1
    );
}

static LOCK_PROBE_URL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static LOCK_PROBE: std::sync::Mutex<Option<bool>> = std::sync::Mutex::new(None);

/// Before writing anything, asks a second SQLite connection for the write
/// lock: under `BEGIN IMMEDIATE` it is already taken.
#[crate::data_step(app = "steps", id = "lock_probe_v1")]
impl LockProbeV1 {
    async fn up(_ctx: &mut StepCtx<'_>, batch: Batch) -> StepResult {
        use sqlx::Connection as _;
        let url = LOCK_PROBE_URL.get().expect("set by the test");
        let mut other = sqlx::SqliteConnection::connect(url)
            .await
            .map_err(|e| StepError::Message(e.to_string()))?;
        sqlx::query("PRAGMA busy_timeout = 0")
            .execute(&mut other)
            .await
            .map_err(|e| StepError::Message(e.to_string()))?;
        let taken = sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut other)
            .await
            .is_err();
        *LOCK_PROBE.lock().unwrap() = Some(taken);
        let _ = other.close().await;
        Ok(batch.done())
    }
}

/// SQLite data units take the write lock at `BEGIN` (6.9).
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_data_units_begin_immediate() {
    bounded(async {
        let Some(db) = fresh(B::Lite).await else {
            return;
        };
        LOCK_PROBE_URL.set(db.url.clone()).unwrap();
        let (_root, dir) = st_project(
            &db.pool,
            json!([rust_op("lock_probe_v1", json!({"noop": "t"}))]),
        )
        .await;
        migrate(&db.pool, &dir, &opts()).await.unwrap();
        assert_eq!(
            *LOCK_PROBE.lock().unwrap(),
            Some(true),
            "the unit began deferred"
        );
    })
    .await;
}

/// The leaked historical-table bytes are logged once per run.
#[cfg(all(feature = "sqlite", feature = "runtime", feature = "testkit"))]
#[tokio::test]
async fn leaked_bytes_are_logged_once_per_run() {
    use tracing_subscriber::layer::SubscriberExt as _;
    let buf = crate::testkit::CaptureWriter::default();
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(buf.clone()),
    );
    let _guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();
    bounded(async {
        let Some(db) = fresh(B::Lite).await else {
            return;
        };
        let (_root, dir) = st_project(&db.pool, json!([rust_op("greet_v1", json!("rust"))])).await;
        migrate(&db.pool, &dir, &opts()).await.unwrap();
    })
    .await;
    let out = buf.contents();
    let lines: Vec<&str> = out
        .lines()
        .filter(|l| l.contains("historical tables interned this run"))
        .collect();
    assert_eq!(lines.len(), 1, "{out}");
    assert!(lines[0].contains("leaked_bytes="), "{}", lines[0]);
}

// ---- batching (3a) ----

/// Example D's backfill: users above the cursor, `size` at a time, copied
/// into `ed_address`; the cursor is the last user id copied.
#[crate::data_step(app = "steps", id = "move_address_v1")]
impl MoveAddressV1 {
    async fn up(ctx: &mut StepCtx<'_>, batch: Batch) -> StepResult {
        let users = ctx.table("ed_user")?;
        let mut q = users.query().order_by("id", false);
        if let Some(lo) = batch.lo() {
            q = q.filter_gt("id", lo.clone());
        }
        let rows = q
            .limit(i64::try_from(batch.size()).unwrap_or(i64::MAX))
            .fetch(ctx)
            .await?;
        let address = ctx.table("ed_address")?;
        let copies: Vec<Vec<(&str, crate::core::SqlValue)>> = rows
            .iter()
            .map(|r| {
                vec![
                    ("user_id", r.key("id").unwrap()),
                    ("street", r.key("street").unwrap()),
                ]
            })
            .collect();
        address.insert_many(ctx, &copies).await?;
        Ok(batch.advance_to(rows.last().and_then(|r| r.key("id"))))
    }
}

/// Never moves its cursor: the runner must refuse the second batch.
#[crate::data_step(app = "steps", id = "stall_v1")]
impl StallV1 {
    async fn up(_ctx: &mut StepCtx<'_>, batch: Batch) -> StepResult {
        Ok(batch.advance_to(Some(crate::core::SqlValue::I64(1))))
    }
}

fn ed_user() -> Value {
    json!({"op": "create_table", "v": 1, "table": {"name": "ed_user", "scope": "tenant", "fields": [
        {"column": "id", "ty": "i64", "primary_key": true, "nullable": false},
        {"column": "street", "ty": "string", "max_length": 40, "nullable": true}]}})
}

/// `0001` creates `ed_user` and `n` rows go in, ids `1..=n` (or `ids`).
async fn ed_project(pool: &Pool, ids: &[i64]) -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    put(&dir, "0001_user", &vs("0001_user", &[], json!([ed_user()])));
    migrate(pool, &dir, &opts()).await.unwrap();
    for chunk in ids.chunks(500) {
        let values: Vec<String> = chunk.iter().map(|i| format!("({i}, 's{i}')")).collect();
        raw_execute_pool(
            pool,
            &format!(
                "INSERT INTO ed_user (id, street) VALUES {}",
                values.join(", ")
            ),
            Vec::new(),
        )
        .await
        .unwrap();
    }
    (root, dir)
}

/// Example D's expand, backfill and contract files (2.5).
fn example_d(dir: &Path, size: u64) {
    let address = json!({"op": "create_table", "v": 1, "table": {"name": "ed_address",
        "scope": "tenant", "fields": [
            {"column": "id", "ty": "i64", "primary_key": true, "auto": true, "nullable": false},
            {"column": "user_id", "ty": "i64", "nullable": false},
            {"column": "street", "ty": "string", "max_length": 40, "nullable": true}]},
        "indexes": [{"name": "ed_address_user_key", "columns": ["user_id"], "unique": true}]});
    put(
        dir,
        "0002_address",
        &vs("0002_address", &["0001_user"], json!([address])),
    );
    let backfill = json!({"op": "data_rust", "v": 1, "id": "move_address_v1",
        "tables": {"read": ["ed_user"], "write": ["ed_address"]},
        "batch": {"table": "ed_user", "key": "id", "size": size},
        "down": {"noop": "the reverse of 0002 drops the table"},
        "allow": [{"code": "MV2-E028", "reason": "test"}]});
    put(
        dir,
        "0003_backfill",
        &vs("0003_backfill", &["0002_address"], json!([backfill])),
    );
    let drop = json!({"op": "drop_column", "v": 1, "table": "ed_user", "column": "street",
        "was": {"column": "street", "ty": "string", "max_length": 40, "nullable": true}});
    put(
        dir,
        "0004_contract",
        &vs("0004_contract", &["0003_backfill"], json!([drop])),
    );
}

const ED_ROWS: i64 = 50_000;

/// Example D with 50k rows, killed at `crash`, then resumed: `applied`,
/// every row moved exactly once.
async fn example_d_with(b: B, crash: Crash, dies: bool) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let ids: Vec<i64> = (1..=ED_ROWS).collect();
    let (_root, dir) = ed_project(pool, &ids).await;
    example_d(&dir, 2000);
    let crashing = Options {
        crash: Some(crash),
        ..Options::default()
    };
    let first = migrate(pool, &dir, &crashing).await;
    assert_eq!(first.is_err(), dies, "{crash:?}: {first:?}");
    if dies {
        migrate(pool, &dir, &opts())
            .await
            .unwrap_or_else(|e| panic!("{crash:?}: resume: {e}"));
    }
    let states: Vec<RowState> = rows(pool).await.into_iter().map(|r| r.1).collect();
    assert!(
        states.iter().all(|s| *s == RowState::Applied),
        "{crash:?}: {states:?}"
    );
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM ed_address").await,
        ED_ROWS,
        "{crash:?}"
    );
    assert_eq!(
        count(pool, "SELECT COUNT(DISTINCT user_id) FROM ed_address").await,
        ED_ROWS,
        "{crash:?}"
    );
    assert_eq!(
        columns(pool, "ed_user").await,
        ["id"],
        "{crash:?}: contract ran"
    );
}

/// Killed before each unit of Example D.
async fn example_d_unit_kills(b: B) {
    for n in 0..3 {
        example_d_with(b, Crash::BeforeUnit(n), true).await;
    }
}

/// Killed in the middle of a batch: first, middle and last.
async fn example_d_mid_batch_kills(b: B) {
    for n in [0, 12, 24] {
        example_d_with(b, Crash::MidBatch(n), true).await;
    }
}

/// The commit of a batch lands but its answer is lost: the run reads the
/// progress and goes on without redoing it.
async fn example_d_lost_acks(b: B) {
    for n in [0, 12, 24] {
        example_d_with(b, Crash::LostAck(n), false).await;
    }
}

fn batched_sql(size: u64) -> Value {
    json!({"op": "data_sql", "v": 1, "id": "mark_v1",
        "tables": {"read": ["ed_user"], "write": ["ed_user.street"]},
        "up": {"any": "UPDATE {{t:ed_user}} SET {{c:street}} = {{?}} WHERE {{batch_range}}"},
        "binds": [{"ty": "string", "value": "moved"}],
        "batch": {"table": "ed_user", "key": "id", "size": size},
        "down": {"noop": "test"}, "allow": [{"code": "MV2-E028", "reason": "test"}]})
}

async fn batch_events(pool: &Pool, mig: &str) -> usize {
    Ledger::default_names()
        .event_kinds(pool, ("steps", mig))
        .await
        .unwrap()
        .iter()
        .filter(|k| *k == "batch")
        .count()
}

/// Keys 1 and 1e9 take 2 batches: boundaries come from the keys, not a
/// MIN..MAX range.
async fn sparse_keys_take_two_batches(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let (_root, dir) = ed_project(pool, &[1, 1_000_000_000]).await;
    put(
        &dir,
        "0002_mark",
        &vs("0002_mark", &["0001_user"], json!([batched_sql(1)])),
    );
    migrate(pool, &dir, &opts()).await.unwrap();
    assert_eq!(batch_events(pool, "0002_mark").await, 2);
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM ed_user WHERE street = 'moved'").await,
        2
    );
}

/// A step whose cursor does not advance is refused.
async fn a_batch_must_advance(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let (_root, dir) = ed_project(pool, &[1, 2, 3]).await;
    let stall = json!({"op": "data_rust", "v": 1, "id": "stall_v1",
        "tables": {"read": ["ed_user"]}, "batch": {"table": "ed_user", "key": "id", "size": 1},
        "down": {"noop": "test"}});
    put(
        &dir,
        "0002_stall",
        &vs("0002_stall", &["0001_user"], json!([stall])),
    );
    let e = migrate(pool, &dir, &opts()).await.expect_err("refused");
    assert!(e.to_string().contains("must advance"), "{e}");
}

/// `--max-duration` stops cleanly between batches; the next run resumes.
async fn max_duration_stops_between_batches(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let ids: Vec<i64> = (1..=30).collect();
    let (_root, dir) = ed_project(pool, &ids).await;
    put(
        &dir,
        "0002_mark",
        &vs("0002_mark", &["0001_user"], json!([batched_sql(10)])),
    );
    let brief = Options {
        max_duration: Some(Duration::ZERO),
        ..Options::default()
    };
    let rep = migrate(pool, &dir, &brief).await.unwrap();
    assert!(rep.stopped.is_some(), "{rep:?}");
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM ed_user WHERE street = 'moved'").await,
        10
    );
    assert_eq!(rows(pool).await.last().unwrap().1, RowState::Running);
    assert_eq!(
        migrate(pool, &dir, &opts()).await.unwrap().applied,
        ["0002_mark"]
    );
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM ed_user WHERE street = 'moved'").await,
        30
    );
    assert_eq!(batch_events(pool, "0002_mark").await, 3);
}

/// `max_batches_per_run` stops a CLI run; boot runs every batch, so it never
/// serves a half-done move.
async fn max_batches_per_run_binds_the_cli_only(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let ids: Vec<i64> = (1..=30).collect();
    let (_root, dir) = ed_project(pool, &ids).await;
    let mut capped = batched_sql(10);
    capped["batch"]["max_batches_per_run"] = json!(1);
    put(
        &dir,
        "0002_mark",
        &vs("0002_mark", &["0001_user"], json!([capped])),
    );
    let cli = Options {
        per_run_limits: true,
        ..Options::default()
    };
    let rep = migrate(pool, &dir, &cli).await.unwrap();
    assert!(rep.stopped.is_some(), "{rep:?}");
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM ed_user WHERE street = 'moved'").await,
        10
    );
    let boot = migrate(pool, &dir, &Options::default()).await.unwrap();
    assert!(boot.stopped.is_none(), "{boot:?}");
    assert_eq!(boot.applied, ["0002_mark"]);
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM ed_user WHERE street = 'moved'").await,
        30
    );
}

/// A shutdown signal stops between batches; the next start resumes at the
/// stored cursor and moves every row once.
async fn a_shutdown_stops_between_batches(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let ids: Vec<i64> = (1..=30).collect();
    let (_root, dir) = ed_project(pool, &ids).await;
    put(
        &dir,
        "0002_mark",
        &vs("0002_mark", &["0001_user"], json!([batched_sql(10)])),
    );
    let stopping = Options {
        stop: Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            true,
        ))),
        ..Options::default()
    };
    let e = migrate(pool, &dir, &stopping).await.expect_err("stopped");
    assert!(e.to_string().contains("between batches"), "{e}");
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM ed_user WHERE street = 'moved'").await,
        10
    );
    assert_eq!(
        migrate(pool, &dir, &opts()).await.unwrap().applied,
        ["0002_mark"]
    );
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM ed_user WHERE street = 'moved'").await,
        30
    );
    assert_eq!(batch_events(pool, "0002_mark").await, 3);
}

/// A lock taken between batches and held past the lock timeout: the next
/// batch times out, is retried, and goes through once the lock is released.
async fn a_held_lock_is_retried(b: B) {
    let Some(db) = fresh(b).await else { return };
    let pool = &db.pool;
    let (_root, dir) = ed_project(pool, &[1, 2, 3]).await;
    let mut slow = batched_sql(1);
    slow["batch"]["sleep_ms"] = json!(300);
    put(
        &dir,
        "0002_mark",
        &vs("0002_mark", &["0001_user"], json!([slow])),
    );
    // After the first batch, another session holds the last row (SQLite:
    // the write lock) for 2.5 s, well past the 1 s lock timeout.
    let url = db.url.clone();
    let holder = tokio::spawn(async move {
        let hp = Pool::connect(&url).await.unwrap();
        for _ in 0..500 {
            if batch_events(&hp, "0002_mark").await >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut hconn = hp.acquire().await.unwrap();
        let mut htx = hconn.begin().await.unwrap();
        crate::sql::raw_execute_tx(
            &mut htx,
            "UPDATE ed_user SET street = 'held' WHERE id = 3",
            Vec::new(),
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(2500)).await;
        htx.rollback().await.unwrap();
        drop(hconn);
        hp.close().await;
    });
    let quick = Options {
        data_lock_timeout: Some(Duration::from_secs(1)),
        ..Options::default()
    };
    let rep = migrate(pool, &dir, &quick).await;
    holder.await.unwrap();
    assert_eq!(rep.unwrap().applied, ["0002_mark"]);
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM ed_user WHERE street = 'moved'").await,
        3
    );
}

/// Two replicas: the one migrating dies mid-move, the waiter takes the
/// lock and finishes it, every row once.
async fn a_waiter_finishes_a_killed_move(b: B) {
    let Some(db) = fresh(b).await else { return };
    let ids: Vec<i64> = (1..=40).collect();
    let (_root, dir) = ed_project(&db.pool, &ids).await;
    let mut slow = batched_sql(5);
    slow["batch"]["sleep_ms"] = json!(150);
    put(
        &dir,
        "0002_mark",
        &vs("0002_mark", &["0001_user"], json!([slow])),
    );
    let a_pool = Pool::connect(&db.url).await.unwrap();
    let b_pool = Pool::connect(&db.url).await.unwrap();
    let (da, dbb) = (dir.clone(), dir.clone());
    let a = tokio::spawn(async move { migrate(&a_pool, &da, &Options::default()).await });
    tokio::time::sleep(Duration::from_millis(400)).await;
    let waiter = tokio::spawn(async move { migrate(&b_pool, &dbb, &Options::default()).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    a.abort();
    let _ = a.await;
    let rep = waiter.await.unwrap().unwrap();
    assert_eq!(rep.applied, ["0002_mark"]);
    assert_eq!(
        count(
            &db.pool,
            "SELECT COUNT(*) FROM ed_user WHERE street = 'moved'"
        )
        .await,
        40
    );
    assert_eq!(
        batch_events(&db.pool, "0002_mark").await,
        8,
        "each batch once"
    );
}

per_backend!(
    apply_rollback_reapply,
    edited_file_and_stored_body,
    crash_and_resume,
    runs_exclude_each_other,
    adopt_and_switch,
    adopt_switch_cases,
    switch_renames_and_fresh_matches,
    three_replicas_one_migrates,
    stop_mid_run_then_resume,
    defaults_pass_check_online,
    two_apps_two_ledgers,
    project_owns_a_framework_table,
    a_range_check_on_a_reserved_name,
    a_rename_keeps_the_data,
    example_a_upgrade_downgrade_fresh,
    a_failure_after_the_data_write_rolls_back,
    missing_steps_run_nothing,
    a_swallowed_error_rolls_the_unit_back,
    the_escape_guard_refuses_pools,
    a_blocked_data_unit_times_out,
    dry_runs_never_write,
    down_to_follows_the_ledger,
    a_cut_off_rollback_resumes_by_probe,
    rollback_is_planned_up_front,
    custom_ledgers_keep_their_names,
    an_invalid_index_left_by_a_drop_is_dropped_again,
    drift_sees_a_constraint_kind,
    a_dropped_run_frees_the_lock,
    a_rust_step_runs_both_ways,
    rust_step_guards,
    a_panicking_step_is_resumable,
    a_missing_rust_step_runs_nothing,
    a_changed_step_is_warned_or_refused,
    the_testing_helper_runs_a_step,
    example_d_unit_kills,
    example_d_mid_batch_kills,
    example_d_lost_acks,
    sparse_keys_take_two_batches,
    a_batch_must_advance,
    max_duration_stops_between_batches,
    a_shutdown_stops_between_batches,
    max_batches_per_run_binds_the_cli_only,
    a_held_lock_is_retried,
    a_waiter_finishes_a_killed_move,
);

/// Legacy SQLite datetime text is swept before the first data unit, so a
/// range filter in the step sees it (5.5).
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn the_sqlite_sweep_runs_before_the_first_data_unit() {
    let Some(db) = fresh(B::Lite).await else {
        return;
    };
    let pool = &db.pool;
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    let ev = json!({"op": "create_table", "v": 1, "table": {"name": "sw_ev", "scope": "tenant",
        "fields": [{"column": "id", "ty": "i64", "primary_key": true, "nullable": false},
                   {"column": "at", "ty": "datetime", "nullable": true},
                   {"column": "late", "ty": "bool", "nullable": true}]}});
    put(&dir, "0001_ev", &v2("0001_ev", &[], json!([ev])));
    migrate(pool, &dir, &opts()).await.unwrap();
    raw_execute_pool(
        pool,
        "INSERT INTO sw_ev (id, at) VALUES (1, '2024-01-01 10:00:00')",
        Vec::new(),
    )
    .await
    .unwrap();
    let step = json!({"op": "data_sql", "v": 1, "id": "late_v1",
        "tables": {"read": ["sw_ev"], "write": ["sw_ev.late"]},
        "up": {"any": "UPDATE {{t:sw_ev}} SET {{c:late}} = TRUE WHERE {{c:at}} >= {{?}}"},
        "binds": [{"ty": "datetime", "value": "2024-01-01T00:00:00Z"}],
        "down": {"noop": "test"}, "allow": [{"code": "MV2-E028", "reason": "test"}]});
    put(
        &dir,
        "0002_late",
        &v2("0002_late", &["0001_ev"], json!([step])),
    );
    migrate(pool, &dir, &opts()).await.unwrap();
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM sw_ev WHERE late = 1").await,
        1
    );
}
