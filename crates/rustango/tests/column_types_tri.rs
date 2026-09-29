//! Column types that must round-trip the same on every backend.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::Model as _;
use rustango::sql::{Auto, FetcherPool as _, Pool};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "coltypes_page", app = "column_types_tri")]
pub struct Page {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub body: String,
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Page>(pool).await;
}

/// #1708: MySQL `TEXT` refused anything past 64 KiB.
async fn unbounded_string_holds_more_than_64_kib(pool: &Pool) {
    let body = "a".repeat(70_000);
    let mut page = Page {
        id: Auto::default(),
        body: body.clone(),
    };
    page.insert_pool(pool).await.expect("insert 70 KB body");
    let rows: Vec<Page> = Page::objects().fetch(pool).await.expect("fetch");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].body.len(), body.len(), "{}", pool.dialect().name());
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        unbounded_string_holds_more_than_64_kib,
    ],
}
