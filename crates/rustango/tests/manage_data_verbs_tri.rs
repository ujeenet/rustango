//! `flush`, `dumpdata` and `loaddata` on every backend (#1911, #1912).

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::Model as _;
use rustango::sql::{Auto, FetcherPool as _, Pool};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "cli1912_row", app = "cli1912")]
pub struct Row {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub name: String,
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Row>(pool).await;
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

tri_dialect_test! {
    setup: setup,
    scenarios: [flush_yes_clears_the_table],
}
