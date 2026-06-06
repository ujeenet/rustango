#![cfg(feature = "sqlite")]
//! Live SQLite tests for the macro-emitted
//! `Model::truncate_pool(pool)` shortcut — Eloquent
//! `Model::truncate()` / Django `Model.objects.all().delete()`
//! parity.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mtr_post")]
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
        "CREATE TABLE mtr_post (
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
async fn truncate_pool_clears_table() {
    let pool = make_pool().await;
    for t in ["a", "b", "c"] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
        };
        p.save_pool(&pool).await.unwrap();
    }
    assert_eq!(Post::count_pool(&pool).await.unwrap(), 3);

    Post::truncate_pool(&pool).await.unwrap();
    assert_eq!(Post::count_pool(&pool).await.unwrap(), 0);
}

#[tokio::test]
async fn truncate_pool_on_empty_table_is_noop() {
    let pool = make_pool().await;
    Post::truncate_pool(&pool).await.unwrap();
    assert_eq!(Post::count_pool(&pool).await.unwrap(), 0);
}
