//! SPIKE S5 (migrations engine v2): autocommit probes and resume (6.5).
//! A child process (this test binary, re-run with `S5_CHILD`) applies
//! examples A and F and is SIGKILLed at a unit boundary; the parent then
//! resumes from the ledger's frozen plan and compares the catalog with an
//! uninterrupted run. Every test makes and drops its own database. Throwaway.

#![cfg(all(
    feature = "migrate-v2-spike",
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use rustango::migrate::v2_spike::catalog::{read_table, LiveTable};
use rustango::migrate::v2_spike::exec::run_units;
use rustango::migrate::v2_spike::lock::LockScope;
use rustango::migrate::v2_spike::ops::{CheckDecl, ColumnDecl, IndexDecl, SpikeOp, TableDecl};
use rustango::migrate::v2_spike::probe::{Mismatch, Prop, Verdict};
use rustango::migrate::v2_spike::render::{apply, render_file, IndexState, Op, PerDialect, State};
#[cfg(any(feature = "postgres", feature = "mysql"))]
use rustango::migrate::v2_spike::resume::session_alive;
use rustango::migrate::v2_spike::resume::{
    decide, fenced, plan, read_row, Action, DataSql, File, FileOp, HookFut, PlanKind, Point,
    RunError, Runner,
};
use rustango::sql::{raw_execute_pool, raw_query_pool, DialectKind, Pool};
use rustango::testkit::matrix::Backend;

// -------------------------------------------------------------- fixtures

fn col(name: &str, ty: &str) -> ColumnDecl {
    ColumnDecl::new(name, ty)
}

fn user() -> TableDecl {
    let mut id = col("id", "i64");
    id.primary_key = true;
    id.auto = true;
    id.nullable = false;
    let mut name = col("name", "string");
    name.max_length = Some(100);
    name.nullable = false;
    TableDecl {
        name: "user".into(),
        columns: vec![id, name],
        comment: None,
        checks: vec![],
        fks: vec![],
    }
}

fn locale(nullable: bool) -> ColumnDecl {
    let mut c = col("locale", "string");
    c.max_length = Some(8);
    c.nullable = nullable;
    c
}

/// The adopted state: `user` only.
fn state0() -> State {
    let mut s = State::default();
    apply(
        &mut s,
        &Op::CreateTable {
            table: user(),
            indexes: vec![],
        },
    )
    .unwrap();
    s
}

/// Example A: add nullable, batched backfill, make required.
fn file_a() -> File {
    File {
        app: "accounts".into(),
        id: "0012_add_locale_to_user".into(),
        ops: vec![
            FileOp::Schema(Op::AddColumn {
                table: "user".into(),
                column: locale(true),
                fill: None,
            }),
            FileOp::Data(DataSql {
                id: "backfill_locale_v1".into(),
                table: "user".into(),
                key: "id".into(),
                size: 10,
                up: "UPDATE {{t:user}} SET {{c:locale}} = 'en' WHERE {{c:locale}} IS NULL AND {{batch_range}}".into(),
            }),
            FileOp::Schema(Op::AlterColumn {
                table: "user".into(),
                from: locale(true),
                to: locale(false),
            }),
        ],
    }
}

fn trgm() -> IndexDecl {
    IndexDecl {
        name: "user_name_trgm".into(),
        table: "user".into(),
        columns: vec!["name".into()],
        unique: false,
        method: "gin".into(),
        predicate: None,
        include: vec![],
    }
}

/// Example F: raw CONCURRENTLY with a declared state, skipped off PG.
fn file_f() -> File {
    File {
        app: "accounts".into(),
        id: "0013_user_name_trgm".into(),
        ops: vec![FileOp::Schema(Op::RawSql {
            up: PerDialect {
                postgres: Some(vec![
                    "CREATE INDEX CONCURRENTLY user_name_trgm ON {{t:user}} USING gin ({{c:name}} gin_trgm_ops)".into(),
                ]),
                mysql: None,
                sqlite: None,
            },
            down: PerDialect::default(),
            state: vec![Op::AddIndex {
                index: IndexState {
                    decl: trgm(),
                    skipped_on: vec![DialectKind::Mysql, DialectKind::Sqlite],
                },
                concurrently: Some(true),
            }],
            autocommit: true,
        })],
    }
}

fn nonempty() -> CheckDecl {
    CheckDecl {
        name: "user_name_nonempty".into(),
        expr: "name <> ''".into(),
    }
}

/// PG raw autocommit: NOT VALID then VALIDATE, two autocommit units.
fn file_g() -> File {
    File {
        app: "accounts".into(),
        id: "0014_user_name_nonempty".into(),
        ops: vec![FileOp::Schema(Op::RawSql {
            up: PerDialect {
                postgres: Some(vec![
                    "ALTER TABLE {{t:user}} ADD CONSTRAINT user_name_nonempty CHECK (name <> '') NOT VALID".into(),
                    "ALTER TABLE {{t:user}} VALIDATE CONSTRAINT user_name_nonempty".into(),
                ]),
                mysql: None,
                sqlite: None,
            },
            down: PerDialect::default(),
            state: vec![Op::AddCheck {
                table: "user".into(),
                check: nonempty(),
            }],
            autocommit: true,
        })],
    }
}

fn files_named(names: &str) -> Vec<File> {
    names
        .chars()
        .map(|c| match c {
            'A' => file_a(),
            'F' => file_f(),
            'G' => file_g(),
            _ => panic!("no file {c}"),
        })
        .collect()
}

fn short(f: &str) -> &'static str {
    match f {
        "0012_add_locale_to_user" => "A",
        "0013_user_name_trgm" => "F",
        _ => "G",
    }
}

// ------------------------------------------------------------- databases

struct Db {
    kind: DialectKind,
    name: String,
    url: String,
    admin: Option<Pool>,
    pool: Pool,
    scope: LockScope,
}

static SEQ: AtomicUsize = AtomicUsize::new(0);

fn backend(k: DialectKind) -> Backend {
    match k {
        DialectKind::Postgres => Backend::Postgres,
        DialectKind::Mysql => Backend::MySql,
        _ => Backend::Sqlite,
    }
}

/// A fresh database of `kind` with `user` and 30 rows; `None` when unset.
async fn fresh(kind: DialectKind, test: &str) -> Option<Db> {
    let name = format!(
        "s5_{}_{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    );
    let (url, admin, scope) = if kind == DialectKind::Sqlite {
        let dir = std::env::temp_dir().join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.sqlite");
        (format!("sqlite://{}?mode=rwc", path.display()), None, None)
    } else {
        let admin_url = backend(kind).admin_url(test)?;
        let admin = Pool::connect(&admin_url).await.unwrap();
        exec(&admin, &format!("CREATE DATABASE {name}")).await;
        let base = admin_url.rsplit_once('/').unwrap().0;
        (
            format!("{base}/{name}"),
            Some(admin),
            Some(LockScope::Database(name.clone())),
        )
    };
    let pool = Pool::connect(&url).await.unwrap();
    let scope = scope.unwrap_or_else(|| {
        let path = url
            .trim_start_matches("sqlite://")
            .split('?')
            .next()
            .unwrap();
        LockScope::Database(path.to_owned())
    });
    let d = pool.dialect();
    let (setup, _) = render_file(
        &State::default(),
        &[Op::CreateTable {
            table: user(),
            indexes: vec![],
        }],
        d,
    )
    .unwrap();
    run_units(&pool, &setup).await.unwrap();
    let rows: Vec<String> = (1..=30).map(|i| format!("('n{i}')")).collect();
    exec(
        &pool,
        &format!(
            "INSERT INTO {} (name) VALUES {}",
            d.quote_ident("user"),
            rows.join(", ")
        ),
    )
    .await;
    if kind == DialectKind::Postgres {
        // Provisioning, as decided in S4 (extension outside the chain).
        exec(&pool, "CREATE EXTENSION IF NOT EXISTS pg_trgm").await;
    }
    Some(Db {
        kind,
        name,
        url,
        admin,
        pool,
        scope,
    })
}

impl Db {
    async fn drop(self) {
        self.pool.close().await;
        match (&self.admin, self.kind) {
            (Some(a), DialectKind::Postgres) => {
                exec(
                    a,
                    &format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.name),
                )
                .await;
            }
            (Some(a), _) => exec(a, &format!("DROP DATABASE IF EXISTS {}", self.name)).await,
            (None, _) => {
                let _ = std::fs::remove_dir_all(std::env::temp_dir().join(&self.name));
            }
        }
    }

    fn runner(&self) -> Runner<'_> {
        Runner {
            pool: &self.pool,
            scope: self.scope.clone(),
            hook: None,
        }
    }
}

async fn exec(pool: &Pool, sql: &str) {
    raw_execute_pool(pool, sql, Vec::new())
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// What the comparison looks at: the `user` table and its data.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Snap {
    table: Option<LiveTable>,
    rows: i64,
    en: Option<i64>,
}

async fn snap(pool: &Pool) -> Snap {
    let table = read_table(pool, "user").await.unwrap();
    let u = pool.dialect().quote_ident("user");
    let rows: Vec<(i64,)> = raw_query_pool(&format!("SELECT COUNT(*) FROM {u}"), vec![], pool)
        .await
        .unwrap();
    let en = if table
        .as_ref()
        .is_some_and(|t| t.columns.iter().any(|c| c.name == "locale"))
    {
        let v: Vec<(i64,)> = raw_query_pool(
            &format!("SELECT COUNT(*) FROM {u} WHERE locale = 'en'"),
            vec![],
            pool,
        )
        .await
        .unwrap();
        Some(v[0].0)
    } else {
        None
    };
    Snap {
        table,
        rows: rows[0].0,
        en,
    }
}

// ----------------------------------------------------------------- child

fn point_name(p: Point) -> &'static str {
    match p {
        Point::Started => "started",
        Point::Executed => "executed",
        Point::Committed => "committed",
    }
}

/// The child: run `S5_FILES` and stop at `S5_KILL` = `<file>:<unit>:<point>[:go]`.
/// At the point it prints `S5-KILLPOINT`, then hangs (or goes on with `:go`)
/// until the parent SIGKILLs it.
#[test]
fn s5_child_entry() {
    let Ok(url) = std::env::var("S5_CHILD") else {
        return;
    };
    let scope = LockScope::Database(std::env::var("S5_SCOPE").unwrap());
    let files = files_named(&std::env::var("S5_FILES").unwrap());
    let kill = std::env::var("S5_KILL").unwrap();
    let parts: Vec<String> = kill.split(':').map(str::to_owned).collect();
    let go = parts.get(3).is_some_and(|s| s == "go");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let pool = Pool::connect(&url).await.unwrap();
        let hook = move |f: &str, u: usize, p: Point| -> HookFut {
            let hit =
                short(f) == parts[0] && u.to_string() == parts[1] && point_name(p) == parts[2];
            Box::pin(async move {
                if hit {
                    println!("S5-KILLPOINT");
                    std::io::stdout().flush().unwrap();
                    if !go {
                        std::future::pending::<()>().await;
                    }
                }
            })
        };
        let r = Runner {
            pool: &pool,
            scope,
            hook: Some(&hook),
        };
        let out = r.migrate(&files, &state0()).await;
        println!("S5-DONE {out:?}");
    });
}

struct Kid {
    child: Child,
    lines: mpsc::Receiver<String>,
}

fn spawn(db: &Db, files: &str, kill: &str) -> Kid {
    let LockScope::Database(scope) = &db.scope else {
        unreachable!()
    };
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "s5_child_entry",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("S5_CHILD", &db.url)
        .env("S5_SCOPE", scope)
        .env("S5_FILES", files)
        .env("S5_KILL", kill)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let (tx, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for l in BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send(l).is_err() {
                break;
            }
        }
    });
    Kid { child, lines }
}

impl Kid {
    /// Wait (60 s at most) for the kill point.
    fn at_point(&self, what: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let mut seen = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(l) if l.contains("S5-KILLPOINT") => return,
                Ok(l) => seen.push(l),
                Err(e) => panic!("{what}: no kill point ({e:?}); child said {seen:?}"),
            }
        }
    }

    /// SIGKILL (Rust's `Child::kill` on Unix) and reap the process.
    fn sigkill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

// ------------------------------------------------------- reference runs

/// An uninterrupted run, with the catalog after each unit and before the first.
async fn reference(
    kind: DialectKind,
    test: &str,
    files: &str,
) -> Option<(Snap, BTreeMap<(String, usize), Snap>)> {
    let db = fresh(kind, test).await?;
    let initial = snap(&db.pool).await;
    let seen: Arc<tokio::sync::Mutex<BTreeMap<(String, usize), Snap>>> = Arc::default();
    let pool = db.pool.clone();
    let s2 = seen.clone();
    let hook = move |f: &str, u: usize, p: Point| -> HookFut {
        let (pool, seen, f) = (pool.clone(), s2.clone(), short(f).to_owned());
        Box::pin(async move {
            if p == Point::Committed {
                let s = snap(&pool).await;
                seen.lock().await.insert((f, u), s);
            }
        })
    };
    let r = Runner {
        pool: &db.pool,
        scope: db.scope.clone(),
        hook: Some(&hook),
    };
    r.migrate(&files_named(files), &state0()).await.unwrap();
    let mut by_unit = seen.lock().await.clone();
    by_unit.insert(("end".into(), 0), snap(&db.pool).await);
    db.drop().await;
    Some((initial, by_unit))
}

/// Every `(file, unit, kind)` of `files` on `kind`, in run order.
fn units_of(kind: DialectKind, files: &str) -> Vec<(String, usize, PlanKind)> {
    let d: &dyn rustango::sql::Dialect = match kind {
        DialectKind::Postgres => &rustango::sql::Postgres,
        DialectKind::Mysql => &rustango::sql::MySql,
        _ => &rustango::sql::Sqlite,
    };
    let mut s = state0();
    let mut out = Vec::new();
    for f in files_named(files) {
        let (units, after) = plan(&f, &s, d).unwrap();
        for (i, u) in units.iter().enumerate() {
            out.push((short(&f.id).to_owned(), i, u.kind));
        }
        s = after;
    }
    out
}

async fn assert_applied(db: &Db, files: &str) {
    for f in files_named(files) {
        let r = read_row(&db.pool, &f.app, &f.id).await.unwrap().unwrap();
        assert_eq!(r.state, "applied", "{}", f.id);
        assert_eq!(r.started, None);
    }
}

// ---------------------------------------------------- criteria 1, 2, SQLite

/// Kill after every unit boundary of A then F; resume reaches `applied`
/// with the catalog of an uninterrupted run. Right after the kill, a tx
/// unit left nothing (`executed`) or all of it (`committed`).
async fn kill_matrix(kind: DialectKind) {
    let test = "kill_matrix";
    let Some((initial, refs)) = reference(kind, test, "AF").await else {
        return;
    };
    let order = units_of(kind, "AF");
    let mut cases = 0;
    for (n, (file, unit, pk)) in order.iter().enumerate() {
        let before = if n == 0 {
            initial.clone()
        } else {
            refs[&(order[n - 1].0.clone(), order[n - 1].1)].clone()
        };
        let after = refs[&(file.clone(), *unit)].clone();
        let points: &[Point] = match pk {
            PlanKind::Autocommit => &[Point::Started, Point::Executed, Point::Committed],
            _ => &[Point::Executed, Point::Committed],
        };
        for &p in points {
            let tag = format!("{kind:?} {file}:{unit} {pk:?} {}", point_name(p));
            let db = fresh(kind, test).await.unwrap();
            let kid = spawn(&db, "AF", &format!("{file}:{unit}:{}", point_name(p)));
            kid.at_point(&tag);
            kid.sigkill();
            let fobj = files_named(file).remove(0);
            let row = read_row(&db.pool, &fobj.app, &fobj.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.state, "running", "{tag}");
            let now = snap(&db.pool).await;
            match (pk, p) {
                (PlanKind::Batched, Point::Committed) => {
                    // Mid-op: the first batch is in and recorded.
                    assert_eq!(row.effect_hi, Some(10), "{tag}");
                    assert_eq!(now.en, Some(10), "{tag}");
                }
                (_, Point::Committed) => {
                    assert_eq!(now, after, "{tag}: whole unit");
                    assert_eq!(row.pos as usize, unit + 1, "{tag}");
                }
                (PlanKind::Autocommit, Point::Executed) => {
                    assert_eq!(now, after, "{tag}: DDL landed");
                    assert_eq!(row.started, Some(*unit as i64), "{tag}");
                }
                _ => {
                    assert_eq!(now, before, "{tag}: nothing of the unit");
                    assert_eq!(row.pos as usize, *unit, "{tag}");
                }
            }
            let out = db.runner().migrate(&files_named("AF"), &state0()).await;
            let out = out.unwrap_or_else(|e| panic!("{tag}: resume: {e}"));
            assert_eq!(out.resumed, vec![fobj.id.clone()], "{tag}");
            let want: Vec<(usize, Verdict, Action)> = match (pk, p) {
                (PlanKind::Autocommit, Point::Started) => {
                    vec![(*unit, Verdict::Absent, Action::Run)]
                }
                (PlanKind::Autocommit, Point::Executed) => {
                    vec![(*unit, Verdict::Present, Action::Skip)]
                }
                _ => vec![],
            };
            let got: Vec<(usize, Verdict, Action)> = out
                .decisions
                .into_iter()
                .map(|d| (d.unit, d.verdict, d.action))
                .collect();
            assert_eq!(got, want, "{tag}");
            assert_applied(&db, "AF").await;
            assert_eq!(
                snap(&db.pool).await,
                refs[&("end".into(), 0)],
                "{tag}: final catalog"
            );
            db.drop().await;
            cases += 1;
            eprintln!("S5 case ok: {tag}");
        }
    }
    eprintln!("S5 {kind:?}: {cases} kill cases over {} units", order.len());
    assert!(cases > 0);
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_kill_matrix() {
    kill_matrix(DialectKind::Postgres).await;
}

#[cfg(feature = "mysql")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mysql_kill_matrix() {
    kill_matrix(DialectKind::Mysql).await;
}

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_kill_matrix() {
    kill_matrix(DialectKind::Sqlite).await;
}

/// The plans the matrix walks: MySQL one unit per DDL, PG CONCURRENTLY
/// autocommit, SQLite transactional only.
#[test]
fn plans_have_the_expected_units() {
    use PlanKind::{Autocommit as Ac, Batched as B, Rebuild as Rb, Tx};
    let k = |v: Vec<(String, usize, PlanKind)>| v.into_iter().map(|x| x.2).collect::<Vec<_>>();
    assert_eq!(
        k(units_of(DialectKind::Postgres, "AF")),
        vec![Tx, B, Tx, Tx, Tx, Ac]
    );
    assert_eq!(k(units_of(DialectKind::Mysql, "AF")), vec![Ac, B, Ac]);
    assert_eq!(k(units_of(DialectKind::Sqlite, "AF")), vec![Tx, B, Rb]);
}

// ------------------------------------------------- criterion 3: orphans

#[cfg(any(feature = "postgres", feature = "mysql"))]
/// Poll `sql` (a COUNT) until it is non-zero; 30 s at most.
async fn until_nonzero(pool: &Pool, sql: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let n: Vec<(i64,)> = raw_query_pool(sql, vec![], pool).await.unwrap();
        if n[0].0 > 0 {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {sql}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
async fn until_gone(pool: &Pool, session: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while session_alive(pool, session).await.unwrap() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "session {session} never went"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Kill the child while its CREATE INDEX CONCURRENTLY waits on a writer.
/// The server keeps running the orphan (a dead client does not stop it), so
/// resume reaps it, the probe sees an invalid index, and it is rebuilt.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_cic_killed_mid_build_is_rebuilt() {
    let test = "pg_cic_killed_mid_build_is_rebuilt";
    let Some((_, refs)) = reference(DialectKind::Postgres, test, "AF").await else {
        return;
    };
    let db = fresh(DialectKind::Postgres, test).await.unwrap();
    db.runner()
        .migrate(&files_named("A"), &state0())
        .await
        .unwrap();
    let mut blocker = db.pool.acquire().await.unwrap();
    let mut btx = blocker.begin().await.unwrap();
    rustango::sql::raw_execute_tx(
        &mut btx,
        "INSERT INTO \"user\" (name, locale) VALUES ('blocker', 'en')",
        vec![],
    )
    .await
    .unwrap();
    let kid = spawn(&db, "AF", "F:0:started:go");
    kid.at_point(test);
    until_nonzero(
        &db.pool,
        "SELECT COUNT(*) FROM pg_stat_activity WHERE query LIKE 'CREATE INDEX CONCURRENTLY%' AND wait_event_type = 'Lock'",
    )
    .await;
    kid.sigkill();
    let f = file_f();
    let row = read_row(&db.pool, &f.app, &f.id).await.unwrap().unwrap();
    assert_eq!(row.started, Some(0));
    let session = row.session.clone().unwrap();
    assert!(
        session_alive(&db.pool, &session).await.unwrap(),
        "premise: the orphan CIC outlives its SIGKILLed client"
    );
    let live = read_table(&db.pool, "user").await.unwrap().unwrap();
    let ix = live
        .indexes
        .iter()
        .find(|i| i.name == "user_name_trgm")
        .unwrap();
    assert!(!ix.valid, "the orphan left an invalid index");

    let (pool, scope) = (db.pool.clone(), db.scope.clone());
    let resume = tokio::spawn(async move {
        let r = Runner {
            pool: &pool,
            scope,
            hook: None,
        };
        r.migrate(&files_named("AF"), &state0()).await
    });
    until_gone(&db.pool, &session).await;
    btx.rollback().await.unwrap();
    drop(blocker);
    let out = resume.await.unwrap().unwrap();
    assert_eq!(out.reaped, vec![session]);
    assert_eq!(out.decisions.len(), 1, "{:?}", out.decisions);
    let d = &out.decisions[0];
    assert_eq!(d.action, Action::Rebuild);
    let Verdict::Partial(ms) = &d.verdict else {
        panic!("{:?}", d.verdict)
    };
    assert_eq!(
        ms.iter().map(|m| m.prop).collect::<Vec<_>>(),
        vec![Prop::IndexValid]
    );
    assert_applied(&db, "AF").await;
    assert_eq!(snap(&db.pool).await, refs[&("end".into(), 0)]);
    db.drop().await;
}

/// Kill the child while its MySQL ALTER waits on a metadata lock. The orphan
/// would apply when the lock frees; resume KILLs it, then the probe says
/// Absent and the unit runs once.
#[cfg(feature = "mysql")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mysql_ddl_killed_in_lock_wait_is_reaped() {
    let test = "mysql_ddl_killed_in_lock_wait_is_reaped";
    let Some((_, refs)) = reference(DialectKind::Mysql, test, "AF").await else {
        return;
    };
    let db = fresh(DialectKind::Mysql, test).await.unwrap();
    let mut blocker = db.pool.acquire().await.unwrap();
    let mut btx = blocker.begin().await.unwrap();
    let _: Vec<(i64,)> = rustango::sql::raw_query_tx(&mut btx, "SELECT id FROM `user`", vec![])
        .await
        .unwrap();
    let kid = spawn(&db, "AF", "A:0:started:go");
    kid.at_point(test);
    until_nonzero(
        &db.pool,
        &format!(
            "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE DB = '{}' AND STATE = 'Waiting for table metadata lock' AND INFO LIKE 'ALTER TABLE%'",
            db.name
        ),
    )
    .await;
    kid.sigkill();
    let a = file_a();
    let row = read_row(&db.pool, &a.app, &a.id).await.unwrap().unwrap();
    let session = row.session.clone().unwrap();
    assert!(
        session_alive(&db.pool, &session).await.unwrap(),
        "premise: the orphan ALTER outlives its SIGKILLed client"
    );
    let (pool, scope) = (db.pool.clone(), db.scope.clone());
    let resume = tokio::spawn(async move {
        let r = Runner {
            pool: &pool,
            scope,
            hook: None,
        };
        r.migrate(&files_named("AF"), &state0()).await
    });
    until_gone(&db.pool, &session).await;
    btx.rollback().await.unwrap();
    drop(blocker);
    let out = resume.await.unwrap().unwrap();
    assert_eq!(out.reaped, vec![session]);
    let got: Vec<(usize, Verdict, Action)> = out
        .decisions
        .into_iter()
        .map(|d| (d.unit, d.verdict, d.action))
        .collect();
    assert_eq!(got, vec![(0, Verdict::Absent, Action::Run)]);
    assert_applied(&db, "AF").await;
    assert_eq!(snap(&db.pool).await, refs[&("end".into(), 0)]);
    db.drop().await;
}

// ------------------------------------------- criterion 4: Partial actions

/// A kill between NOT VALID and VALIDATE: the probe says only "not
/// validated", so resume runs VALIDATE instead of refusing.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_not_validated_check_is_validated_on_resume() {
    let test = "pg_not_validated_check_is_validated_on_resume";
    let Some(db) = fresh(DialectKind::Postgres, test).await else {
        return;
    };
    let kid = spawn(&db, "G", "G:0:executed");
    kid.at_point(test);
    kid.sigkill();
    let out = db
        .runner()
        .migrate(&files_named("G"), &state0())
        .await
        .unwrap();
    assert_eq!(out.decisions.len(), 1, "{:?}", out.decisions);
    let d = &out.decisions[0];
    assert_eq!(
        d.action,
        Action::Validate(vec!["user_name_nonempty".into()])
    );
    let live = read_table(&db.pool, "user").await.unwrap().unwrap();
    let c = live
        .checks
        .iter()
        .find(|c| c.name.as_deref() == Some("user_name_nonempty"))
        .unwrap();
    assert!(c.validated);
    assert_applied(&db, "G").await;
    db.drop().await;
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
fn refused_props(r: Result<impl std::fmt::Debug, RunError>) -> Vec<Prop> {
    match r {
        Err(RunError::Refused { diff, .. }) => diff.iter().map(|m| m.prop).collect(),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// Someone built a btree index of the same name before resume: refuse with
/// the difference, change nothing.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_foreign_index_is_refused() {
    let test = "pg_foreign_index_is_refused";
    let Some(db) = fresh(DialectKind::Postgres, test).await else {
        return;
    };
    let kid = spawn(&db, "AF", "F:0:started");
    kid.at_point(test);
    kid.sigkill();
    exec(&db.pool, "CREATE INDEX user_name_trgm ON \"user\" (name)").await;
    let r = db.runner().migrate(&files_named("AF"), &state0()).await;
    assert_eq!(refused_props(r), vec![Prop::IndexMethod]);
    let f = file_f();
    let row = read_row(&db.pool, &f.app, &f.id).await.unwrap().unwrap();
    assert_eq!(
        (row.state.as_str(), row.pos, row.started),
        ("running", 0, Some(0))
    );
    db.drop().await;
}

/// A column of the same name but another type: refuse with ColType.
#[cfg(feature = "mysql")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mysql_foreign_column_is_refused() {
    let test = "mysql_foreign_column_is_refused";
    let Some(db) = fresh(DialectKind::Mysql, test).await else {
        return;
    };
    let kid = spawn(&db, "AF", "A:0:started");
    kid.at_point(test);
    kid.sigkill();
    exec(&db.pool, "ALTER TABLE `user` ADD COLUMN locale INT").await;
    let r = db.runner().migrate(&files_named("AF"), &state0()).await;
    assert!(refused_props(r).contains(&Prop::ColType));
    db.drop().await;
}

fn mm(prop: Prop, object: &str) -> Mismatch {
    Mismatch {
        prop,
        object: object.into(),
        want: String::new(),
        got: String::new(),
    }
}

/// The decision table, every row.
#[test]
fn decide_table() {
    let add_ix = SpikeOp::AddIndex(trgm());
    let drop_ix = SpikeOp::DropIndex { was: trgm() };
    let add_ck = SpikeOp::AddCheck {
        table: "user".into(),
        check: nonempty(),
    };
    let p = |v: Vec<Mismatch>| Verdict::Partial(v);
    assert_eq!(decide(&add_ix, &Verdict::Present), Action::Skip);
    assert_eq!(decide(&add_ix, &Verdict::Absent), Action::Run);
    assert_eq!(
        decide(&add_ix, &p(vec![mm(Prop::IndexValid, "i")])),
        Action::Rebuild
    );
    assert_eq!(
        decide(&drop_ix, &p(vec![mm(Prop::IndexValid, "i")])),
        Action::Run
    );
    assert_eq!(
        decide(&add_ck, &p(vec![mm(Prop::CheckValidated, "c")])),
        Action::Validate(vec!["c".into()])
    );
    for v in [
        vec![mm(Prop::IndexMethod, "i")],
        vec![mm(Prop::IndexValid, "i"), mm(Prop::IndexColumns, "i")],
        vec![mm(Prop::CheckValidated, "c"), mm(Prop::CheckExpr, "c")],
        vec![mm(Prop::IndexValid, "i"), mm(Prop::CheckValidated, "c")],
    ] {
        assert!(
            matches!(decide(&add_ix, &p(v.clone())), Action::Refuse(_)),
            "{v:?}"
        );
    }
}

/// MySQL `add_column` with `fill` is two autocommit units; one op-level
/// probe cannot judge the middle state, so planning refuses it.
#[test]
fn multi_unit_autocommit_op_needs_unit_probes() {
    let mut c = locale(false);
    c.nullable = false;
    let f = File {
        app: "accounts".into(),
        id: "x".into(),
        ops: vec![FileOp::Schema(Op::AddColumn {
            table: "user".into(),
            column: c,
            fill: Some("'en'".into()),
        })],
    };
    let e = plan(&f, &state0(), &rustango::sql::MySql).unwrap_err();
    assert!(e.contains("no probe"), "{e}");
}

// ------------------------------------------------- criterion 5: fencing

/// Two resumers after a kill: the one holding the lock runs, the other is
/// refused; a write under the dead run's `run_id` is fenced out.
async fn racing_resumers(kind: DialectKind, kill: &str) {
    let test = "racing_resumers";
    let Some(db) = fresh(kind, test).await else {
        return;
    };
    let kid = spawn(&db, "AF", kill);
    kid.at_point(test);
    kid.sigkill();
    let fid = files_named(&kill[..1]).remove(0);
    let dead = read_row(&db.pool, &fid.app, &fid.id)
        .await
        .unwrap()
        .unwrap();

    let arrived = Arc::new(tokio::sync::Notify::new());
    let go = Arc::new(tokio::sync::Notify::new());
    let (pool, scope) = (db.pool.clone(), db.scope.clone());
    let (a2, g2) = (arrived.clone(), go.clone());
    let first = tokio::spawn(async move {
        let once = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook = move |_: &str, _: usize, _: Point| -> HookFut {
            let (a, g, once) = (a2.clone(), g2.clone(), once.clone());
            Box::pin(async move {
                if !once.swap(true, Ordering::SeqCst) {
                    a.notify_one();
                    g.notified().await;
                }
            })
        };
        let r = Runner {
            pool: &pool,
            scope,
            hook: Some(&hook),
        };
        r.migrate(&files_named("AF"), &state0()).await
    });
    tokio::time::timeout(Duration::from_secs(60), arrived.notified())
        .await
        .expect("first resumer reached a unit");
    let second = db.runner().migrate(&files_named("AF"), &state0()).await;
    assert!(matches!(second, Err(RunError::LockHeld)), "{second:?}");
    go.notify_one();
    let out = first.await.unwrap().unwrap();
    assert_eq!(out.resumed, vec![fid.id.clone()]);
    assert_applied(&db, "AF").await;

    // The dead run's token no longer matches the row.
    let now = read_row(&db.pool, &fid.app, &fid.id)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(now.run_id, dead.run_id);
    let stale = fenced(
        &db.pool,
        &[("pos", Some(rustango::core::SqlValue::from(0i64)))],
        &fid,
        &dead.run_id,
        now.pos,
    )
    .await;
    assert!(matches!(stale, Err(RunError::LockLost { .. })), "{stale:?}");
    db.drop().await;
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_racing_resumers_one_proceeds() {
    racing_resumers(DialectKind::Postgres, "F:0:executed").await;
}

#[cfg(feature = "mysql")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mysql_racing_resumers_one_proceeds() {
    racing_resumers(DialectKind::Mysql, "A:0:executed").await;
}

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_racing_resumers_one_proceeds() {
    racing_resumers(DialectKind::Sqlite, "A:2:executed").await;
}
