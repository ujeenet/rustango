#![cfg(feature = "sqlite")]
//! Live SQLite tests for the macro-emitted
//! `Model::latest(field, pool)` / `earliest_pool(field, pool)`
//! shortcuts — Eloquent `Model::latest($field)->first()` /
//! `oldest($field)->first()` parity.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mle_post")]
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
        "CREATE TABLE mle_post (
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
    for (title, views) in [("low", 10), ("mid", 50), ("high", 500)] {
        let mut p = Post {
            id: Auto::default(),
            title: title.into(),
            views,
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn latest_pool_picks_largest_field_value() {
    let pool = make_pool().await;
    seed(&pool).await;
    let row = Post::latest("views", &pool).await.unwrap().unwrap();
    assert_eq!(row.title, "high", "latest by views should be 500");
}

#[tokio::test]
async fn earliest_pool_picks_smallest_field_value() {
    let pool = make_pool().await;
    seed(&pool).await;
    let row = Post::earliest("views", &pool).await.unwrap().unwrap();
    assert_eq!(row.title, "low", "earliest by views should be 10");
}

#[tokio::test]
async fn latest_pool_returns_none_for_empty_table() {
    let pool = make_pool().await;
    let row = Post::latest("views", &pool).await.unwrap();
    assert!(row.is_none());
}
