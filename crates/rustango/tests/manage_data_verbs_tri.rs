//! `flush`, `dumpdata` and `loaddata` on every backend (#1911, #1912, #2285).

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::Model as _;
use rustango::sql::{raw_execute_pool, Array, Auto, FetcherPool as _, ForeignKey, Pool};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "cli1912_row", app = "cli1912")]
pub struct Row {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "cli1911_parent", app = "cli1911")]
pub struct Parent {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "cli1911_child", app = "cli1911")]
pub struct Child {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub parent: ForeignKey<Parent, i64>,
    pub at: chrono::NaiveTime,
    pub n: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "cli1911_node", app = "cli1911")]
pub struct Node {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "cli1911_node", on = "id")]
    pub parent_id: Option<i64>,
}

/// Never created: dumpdata must refuse it before it reads a row.
#[derive(Model, Debug, Clone)]
#[rustango(table = "cli1911_tagged", app = "cli1911")]
pub struct Tagged {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub tags: Array<String>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "cli2285_owned", app = "cli2285")]
pub struct Owned {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "cli2285_legacy", app = "cli2285", managed = false)]
pub struct Legacy {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "cli2285_view", app = "cli2285", view)]
pub struct OwnedView {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 32)]
    pub name: String,
}

async fn fresh_parent_child(pool: &Pool) {
    rustango::testkit::matrix::drop_table(pool, Child::SCHEMA.table).await;
    rustango::testkit::matrix::fresh_table::<Parent>(pool).await;
    rustango::testkit::matrix::fresh_table::<Child>(pool).await;
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Row>(pool).await;
    rustango::testkit::matrix::fresh_table::<Node>(pool).await;
    fresh_parent_child(pool).await;
}

/// `--fail-fast` stops on a bad row but still moves the id sequence past
/// the rows it already wrote, so the next insert does not collide.
async fn fail_fast_still_resets_sequences(pool: &Pool) {
    let fixture = serde_json::json!([
        {"model": "cli1911.Parent", "pk": 1, "fields": {"name": "a"}},
        {"model": "nope.Unknown", "pk": 1, "fields": {}},
    ]);
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bad.json");
    std::fs::write(&path, fixture.to_string()).unwrap();
    let res = manage(pool, &["loaddata", path.to_str().unwrap(), "--fail-fast"]).await;
    assert!(res.is_err(), "the unknown model must fail: {res:?}");
    let mut next = Parent {
        id: Auto::default(),
        name: "next".into(),
    };
    next.insert_pool(pool)
        .await
        .expect("an insert after a --fail-fast load reused a loaded id");
}

/// A self-FK child listed before its parent still loads (tree tables).
async fn self_fk_child_before_parent_loads(pool: &Pool) {
    let mut root = Node {
        id: Auto::default(),
        parent_id: None,
    };
    root.insert_pool(pool).await.expect("root");
    let mut leaf = Node {
        id: Auto::default(),
        parent_id: Some(root.id.get().copied().expect("pk")),
    };
    leaf.insert_pool(pool).await.expect("leaf");

    let dumped = manage(
        pool,
        &["dumpdata", "--model", "cli1911.Node", "--indent", "0"],
    )
    .await
    .expect("dumpdata");
    let before: serde_json::Value = serde_json::from_str(dumped.trim()).unwrap();
    let mut rows = before.as_array().expect("array").clone();
    rows.sort_by_key(|e| e["fields"]["parent_id"].is_null());
    assert!(
        !rows[0]["fields"]["parent_id"].is_null(),
        "child first: {rows:?}"
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let fixture = dir.path().join("nodes.json");
    std::fs::write(&fixture, serde_json::to_string(&rows).unwrap()).unwrap();

    rustango::testkit::matrix::fresh_table::<Node>(pool).await;
    let out = manage(pool, &["loaddata", fixture.to_str().unwrap()]).await;
    assert!(out.is_ok(), "loaddata: {out:?}");
    let n: Vec<Node> = Node::objects().fetch(pool).await.expect("fetch");
    assert_eq!(n.len(), 2);
}

async fn manage(pool: &Pool, args: &[&str]) -> Result<String, String> {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut buf: Vec<u8> = Vec::new();
    rustango::migrate::manage::run_with_writer(
        pool,
        dir.path(),
        args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
        &mut buf,
    )
    .await
    .map(|()| String::from_utf8_lossy(&buf).into_owned())
    .map_err(|e| e.to_string())
}

async fn insert(pool: &Pool, name: &str) {
    let mut r = Row {
        id: Auto::default(),
        name: name.into(),
    };
    r.insert_pool(pool).await.expect("insert");
}

/// `flush --yes` clears the table; it once sent `DELETE FROM "t"` to MySQL.
async fn flush_yes_clears_the_table(pool: &Pool) {
    insert(pool, "a").await;
    insert(pool, "b").await;
    manage(pool, &["flush", "--yes", "--model", "cli1912.Row"])
        .await
        .expect("flush --yes");
    let left: Vec<Row> = Row::objects().fetch(pool).await.expect("fetch");
    assert!(left.is_empty(), "flush left {} row(s)", left.len());
}

/// `flush` leaves unmanaged tables and views alone (#2285).
async fn flush_skips_unmanaged_tables_and_views(pool: &Pool) {
    let _ = raw_execute_pool(pool, "DROP VIEW IF EXISTS cli2285_view", vec![]).await;
    rustango::testkit::matrix::fresh_table::<Owned>(pool).await;
    rustango::testkit::matrix::fresh_table::<Legacy>(pool).await;
    raw_execute_pool(
        pool,
        "CREATE VIEW cli2285_view AS SELECT id, name FROM cli2285_owned",
        vec![],
    )
    .await
    .expect("create view");
    for name in ["a", "b"] {
        let mut o = Owned {
            id: Auto::default(),
            name: name.into(),
        };
        o.insert_pool(pool).await.expect("owned");
        let mut l = Legacy {
            id: Auto::default(),
            name: name.into(),
        };
        l.insert_pool(pool).await.expect("legacy");
    }

    let out = manage(pool, &["flush", "--yes", "--app", "cli2285"]).await;
    let owned: Vec<Owned> = Owned::objects().fetch(pool).await.expect("owned");
    let legacy: Vec<Legacy> = Legacy::objects().fetch(pool).await.expect("legacy");
    raw_execute_pool(pool, "DROP VIEW cli2285_view", vec![])
        .await
        .expect("drop view");
    let out = out.expect("flush --app cli2285");
    assert!(out.contains("cleared 1 table(s)"), "{out}");
    assert!(
        owned.is_empty(),
        "the managed table kept {} row(s)",
        owned.len()
    );
    assert_eq!(legacy.len(), 2, "flush wiped the unmanaged table");
}

async fn dump(pool: &Pool) -> serde_json::Value {
    let out = manage(
        pool,
        &[
            "dumpdata",
            "--model",
            "cli1911.Parent",
            "--model",
            "cli1911.Child",
            "--indent",
            "0",
        ],
    )
    .await
    .expect("dumpdata");
    serde_json::from_str(out.trim()).expect("dumpdata JSON")
}

/// dump → fresh tables → load (children first) → dump gives the same
/// rows, and the next plain insert does not reuse a loaded id.
async fn dump_and_load_round_trip(pool: &Pool) {
    let mut p = Parent {
        id: Auto::default(),
        name: "p".into(),
    };
    p.insert_pool(pool).await.expect("parent");
    let mut c = Child {
        id: Auto::default(),
        parent: ForeignKey::unloaded(p.id.get().copied().expect("pk")),
        at: chrono::NaiveTime::from_hms_milli_opt(12, 34, 56, 789).unwrap(),
        n: 7,
    };
    c.insert_pool(pool).await.expect("child");

    let before = dump(pool).await;
    let mut reversed = before.as_array().expect("array").clone();
    reversed.sort_by_key(|e| e["model"] != "cli1911.Child");
    let dir = tempfile::tempdir().expect("tempdir");
    let fixture = dir.path().join("fixture.json");
    std::fs::write(&fixture, serde_json::to_string(&reversed).unwrap()).unwrap();

    fresh_parent_child(pool).await;
    let out = manage(pool, &["loaddata", fixture.to_str().unwrap()]).await;
    assert!(out.is_ok(), "loaddata: {out:?}");
    assert_eq!(dump(pool).await, before, "the reload changed the rows");

    let mut next = Parent {
        id: Auto::default(),
        name: "after".into(),
    };
    next.insert_pool(pool)
        .await
        .expect("an insert after loaddata reused a loaded id");
}

/// A column dumpdata would write as `null` is refused, not silently lost.
async fn dumpdata_refuses_columns_it_cannot_read(pool: &Pool) {
    let err = manage(pool, &["dumpdata", "--model", "cli1911.Tagged"])
        .await
        .expect_err("an Array column dumped as null");
    assert!(err.contains("tags") && err.contains("cannot"), "{err}");
    let out = manage(
        pool,
        &[
            "dumpdata",
            "--model",
            "cli1911.Tagged",
            "--exclude",
            "cli1911.Tagged",
        ],
    )
    .await
    .expect("--exclude leaves it out");
    assert_eq!(out.trim(), "[]");
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        flush_yes_clears_the_table,
        flush_skips_unmanaged_tables_and_views,
        dump_and_load_round_trip,
        dumpdata_refuses_columns_it_cannot_read,
        self_fk_child_before_parent_loads,
        fail_fast_still_resets_sequences,
    ],
}

/// `db:restore --clean` leaves the database intact when the dump is missing
/// or fails to load (#2283). Postgres only: it shells out to `psql`.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn restore_clean_keeps_the_database_on_a_bad_dump() {
    use rustango::testkit::matrix::{live_lock, Backend};
    let _guard = live_lock().lock().await;
    let Some(pool) = Backend::Postgres.pool().await else {
        eprintln!("DATABASE_URL not set — skipping");
        return;
    };
    assert!(
        std::process::Command::new("psql")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success()),
        "db:restore needs `psql` on PATH"
    );
    rustango::testkit::matrix::fresh_table::<Row>(&pool).await;
    insert(&pool, "kept").await;
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("nope.sql");
    let bad = dir.path().join("bad.sql");
    std::fs::write(
        &bad,
        "CREATE TABLE cli2283_half (id int);\nSELECT * FROM cli2283_no_such_table;\n",
    )
    .unwrap();

    for (file, why) in [(&missing, "cannot read"), (&bad, "psql exited")] {
        let res = manage(
            &pool,
            &["db:restore", "--clean", "--yes", file.to_str().unwrap()],
        )
        .await;
        let left: Vec<Row> = Row::objects()
            .fetch(&pool)
            .await
            .unwrap_or_else(|e| panic!("{why}: the restore dropped the schema: {e}"));
        assert_eq!(left.len(), 1, "{why}");
        let err = res.expect_err(why);
        assert!(err.contains(why), "{err}");
    }
}
