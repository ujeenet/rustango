#![cfg(feature = "sqlite")]
//! Live SQLite tests for `Model::increment_each` /
//! `Model::decrement_each`. Eloquent
//! `Model::query()->increment($col, $by)` /
//! `Model::query()->decrement($col, $by)` parity.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mie_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub views: i64,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE mie_post (
            id    INTEGER PRIMARY KEY AUTOINCREMENT,
            title TEXT NOT NULL,
            views INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool) {
    for (t, v) in [("a", 10_i64), ("b", 20), ("c", 30)] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
            views: v,
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn increment_each_adds_to_every_row() {
    let pool = make_pool().await;
    seed(&pool).await;
    let n = Post::increment_each("views", 5, &pool).await.unwrap();
    assert_eq!(n, 3);
    let after: Vec<i64> = Post::pluck::<i64>("views", &pool).await.unwrap();
    let mut sorted = after.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, vec![15, 25, 35]);
}

#[tokio::test]
async fn decrement_each_subtracts_from_every_row() {
    let pool = make_pool().await;
    seed(&pool).await;
    let n = Post::decrement_each("views", 5, &pool).await.unwrap();
    assert_eq!(n, 3);
    let after: Vec<i64> = Post::pluck::<i64>("views", &pool).await.unwrap();
    let mut sorted = after.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, vec![5, 15, 25]);
}

#[tokio::test]
async fn instance_increment_still_works_after_dry_refactor() {
    let pool = make_pool().await;
    seed(&pool).await;
    let p = Post::find(1_i64, &pool).await.unwrap().unwrap();
    p.increment("views", 100, &pool).await.unwrap();
    let after = Post::find(1_i64, &pool).await.unwrap().unwrap();
    assert_eq!(after.views, 110);
    after.decrement("views", 50, &pool).await.unwrap();
    let then = Post::find(1_i64, &pool).await.unwrap().unwrap();
    assert_eq!(then.views, 60);
}

#[tokio::test]
async fn increment_each_unknown_field_errors() {
    let pool = make_pool().await;
    seed(&pool).await;
    let err = Post::increment_each("nope", 1, &pool).await.unwrap_err();
    assert!(err.to_string().contains("nope"));
}
