#![cfg(feature = "sqlite")]
//! Live SQLite tests for the macro-emitted `Model::first_pool` /
//! `first_or_fail_pool` shortcuts — Eloquent `Model::first()` /
//! `Model::firstOrFail()` parity.

use rustango::sql::{sqlx, Auto, ExecError, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mfp_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE mfp_post (
            id    INTEGER PRIMARY KEY AUTOINCREMENT,
            title TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

#[tokio::test]
async fn first_pool_returns_some_for_populated_table() {
    let pool = make_pool().await;
    let mut p1 = Post {
        id: Auto::default(),
        title: "alpha".into(),
    };
    p1.save_pool(&pool).await.unwrap();
    let mut p2 = Post {
        id: Auto::default(),
        title: "beta".into(),
    };
    p2.save_pool(&pool).await.unwrap();

    let row = Post::first_pool(&pool).await.unwrap();
    assert!(row.is_some());
    // PK-ASC default: lowest id wins.
    assert_eq!(row.unwrap().title, "alpha");
}

#[tokio::test]
async fn first_pool_returns_none_for_empty_table() {
    let pool = make_pool().await;
    let row = Post::first_pool(&pool).await.unwrap();
    assert!(row.is_none());
}

#[tokio::test]
async fn first_or_fail_pool_errors_on_empty_table() {
    let pool = make_pool().await;
    match Post::first_or_fail_pool(&pool).await {
        Err(ExecError::Driver(rustango::sql::sqlx::Error::RowNotFound)) => {} // ok
        other => panic!("expected RowNotFound, got: {other:?}"),
    }
}
