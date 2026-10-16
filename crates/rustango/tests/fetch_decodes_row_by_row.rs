//! `fetch` decodes each row as it arrives instead of buffering the whole
//! raw result set first (+68% peak memory, rel-0608 performance-001).
//! Row 1 fails to decode and row 2 fails in SQLite's `ABS` overflow: a
//! streaming read reports row 1's decode error, a buffering one row 2's.

#![cfg(all(feature = "sqlite", feature = "testkit"))]

use rustango::core::{funcs::abs, BulkInsertQuery, Model as _, Op, SqlValue, WhereExpr, F};
use rustango::sql::{sqlx, ExecError, FetcherPool as _, Pool};
use rustango::Model;

#[derive(Model, Debug)]
#[rustango(table = "fdr_item")]
#[allow(dead_code)]
pub struct Loose {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 20)]
    pub b: Option<String>,
    pub n: i64,
}

/// The same table, read with `b` required.
#[derive(Model, Debug)]
#[rustango(table = "fdr_item")]
#[allow(dead_code)]
pub struct Strict {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 20)]
    pub b: String,
    pub n: i64,
}

#[tokio::test]
async fn fetch_decodes_each_row_before_the_next_arrives() {
    let pool = Pool::Sqlite(
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap(),
    );
    rustango::testkit::matrix::fresh_table::<Loose>(&pool).await;
    let rows = vec![
        vec![SqlValue::I64(1), SqlValue::Null, SqlValue::I64(1)],
        vec![
            SqlValue::I64(2),
            SqlValue::String("x".into()),
            SqlValue::I64(i64::MIN),
        ],
    ];
    let q = BulkInsertQuery::new(Loose::SCHEMA, vec!["id", "b", "n"], rows);
    rustango::sql::bulk_insert_pool(&pool, &q).await.unwrap();

    let res = Strict::objects()
        .where_raw(WhereExpr::ExprCompare {
            lhs: abs(F("n")),
            op: Op::Gte,
            rhs: F("id").into(),
        })
        .order_by(&[("id", false)])
        .fetch(&pool)
        .await;
    assert!(
        matches!(res, Err(ExecError::Driver(sqlx::Error::ColumnDecode { .. }))),
        "row 1's decode error must win over row 2's overflow: {res:?}"
    );
}
