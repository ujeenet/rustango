#![cfg(feature = "sqlite")]
//! Live SQLite tests for the macro-emitted `Model::count` /
//! `Model::exists` shortcuts — Eloquent `Model::count()` /
//! `Model::query()->exists()` parity.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mce_post")]
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
        "CREATE TABLE mce_post (
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
async fn count_returns_row_count() {
    let pool = make_pool().await;
    assert_eq!(Post::count(&pool).await.unwrap(), 0);

    for t in ["a", "b", "c"] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
        };
        p.save_pool(&pool).await.unwrap();
    }
    assert_eq!(Post::count(&pool).await.unwrap(), 3);
}

#[tokio::test]
async fn exists_returns_false_for_empty_table() {
    let pool = make_pool().await;
    assert!(!Post::exists(&pool).await.unwrap());
}

#[tokio::test]
async fn exists_returns_true_after_insert() {
    let pool = make_pool().await;
    let mut p = Post {
        id: Auto::default(),
        title: "x".into(),
    };
    p.save_pool(&pool).await.unwrap();
    assert!(Post::exists(&pool).await.unwrap());
}
