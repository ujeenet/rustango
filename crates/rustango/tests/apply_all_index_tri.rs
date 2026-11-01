//! `apply_all_pool` fails when a declared index cannot be built, on every
//! backend. Each model below holds an index one backend refuses:
//! a partial index over a missing column (PG, SQLite; MySQL drops the
//! condition) and a key over 3072 bytes in utf8mb4 (MySQL 1071).

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::sql::{Auto, Pool};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug)]
#[rustango(
    table = "aaidx_tri_bad_where",
    index_when(
        columns = "a",
        condition = "aaidx_no_such_col > 0",
        name = "aaidx_bad_where_idx"
    )
)]
#[allow(dead_code)]
pub struct BadWhere {
    #[rustango(primary_key)]
    id: Auto<i64>,
    a: i64,
}

#[derive(Model, Debug)]
#[rustango(table = "aaidx_tri_wide")]
#[allow(dead_code)]
pub struct WideKey {
    #[rustango(primary_key)]
    id: Auto<i64>,
    #[rustango(max_length = 1000, index)]
    body: String,
}

/// Every registered table gone, so `apply_all_pool` starts clean.
async fn setup(pool: &Pool) {
    use rustango::sql::raw_execute_pool;
    let mysql = pool.dialect().name() == "mysql";
    if mysql {
        let _ = raw_execute_pool(pool, "SET FOREIGN_KEY_CHECKS = 0", vec![]).await;
    }
    rustango::migrate::drop_all_pool(pool)
        .await
        .expect("drop_all_pool");
    if mysql {
        let _ = raw_execute_pool(pool, "SET FOREIGN_KEY_CHECKS = 1", vec![]).await;
    }
}

async fn an_index_that_cannot_be_built_fails_the_run(pool: &Pool) {
    let (model, index) = if pool.dialect().name() == "mysql" {
        ("WideKey", "aaidx_tri_wide_body_idx")
    } else {
        ("BadWhere", "aaidx_bad_where_idx")
    };
    let err = rustango::migrate::apply_all_pool(pool)
        .await
        .expect_err("an index the backend refuses must fail the bootstrap");
    let msg = err.to_string();
    assert!(msg.contains(index) && msg.contains(model), "{msg}");
}

tri_dialect_test!(
    setup: setup,
    scenarios: [an_index_that_cannot_be_built_fails_the_run],
);
