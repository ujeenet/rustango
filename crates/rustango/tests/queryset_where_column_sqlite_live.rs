#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet::where_column(col1, col2)` /
//! `QuerySet::where_column_op(col1, op, col2)` — Eloquent
//! `Builder::whereColumn` parity that compares two columns instead
//! of column-vs-literal.

use rustango::core::Op;
use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "wc_resv")]
#[allow(dead_code)]
pub struct Reservation {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub start_day: i64,
    pub end_day: i64,
    #[rustango(max_length = 40)]
    pub label: String,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE wc_resv (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            start_day INTEGER NOT NULL,
            end_day   INTEGER NOT NULL,
            label     TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    p.into()
}

async fn insert(pool: &Pool, start: i64, end: i64, label: &str) {
    let mut r = Reservation {
        id: Auto::default(),
        start_day: start,
        end_day: end,
        label: label.into(),
    };
    r.save_pool(pool).await.unwrap();
}

#[tokio::test]
async fn where_column_eq_matches_rows_with_equal_columns() {
    let pool = make_pool().await;
    insert(&pool, 5, 5, "same").await;
    insert(&pool, 1, 9, "diff").await;
    insert(&pool, 7, 7, "same2").await;
    let mut got: Vec<String> = Reservation::objects()
        .where_column("start_day", "end_day")
        .fetch_pool(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.label)
        .collect();
    got.sort();
    assert_eq!(got, vec!["same", "same2"]);
}

#[tokio::test]
async fn where_column_op_lt_finds_rows_where_start_before_end() {
    let pool = make_pool().await;
    insert(&pool, 1, 9, "valid").await;
    insert(&pool, 5, 5, "zero-len").await;
    insert(&pool, 9, 1, "reversed").await;
    let mut got: Vec<String> = Reservation::objects()
        .where_column_op("start_day", Op::Lt, "end_day")
        .fetch_pool(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.label)
        .collect();
    got.sort();
    assert_eq!(got, vec!["valid"]);
}
