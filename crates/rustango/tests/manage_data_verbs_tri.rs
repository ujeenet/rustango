//! `flush`, `dumpdata` and `loaddata` on every backend (#1911, #1912).

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::Model as _;
use rustango::sql::{Array, Auto, FetcherPool as _, ForeignKey, Pool};
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

/// Never created: dumpdata must refuse it before it reads a row.
#[derive(Model, Debug, Clone)]
#[rustango(table = "cli1911_tagged", app = "cli1911")]
pub struct Tagged {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub tags: Array<String>,
}

async fn fresh_parent_child(pool: &Pool) {
    rustango::testkit::matrix::drop_table(pool, Child::SCHEMA.table).await;
    rustango::testkit::matrix::fresh_table::<Parent>(pool).await;
    rustango::testkit::matrix::fresh_table::<Child>(pool).await;
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Row>(pool).await;
    fresh_parent_child(pool).await;
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
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        flush_yes_clears_the_table,
        dump_and_load_round_trip,
        dumpdata_refuses_columns_it_cannot_read,
    ],
}
