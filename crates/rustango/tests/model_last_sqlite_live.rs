#![cfg(feature = "sqlite")]
//! Live SQLite test for `Model::last(&pool) -> Option<Self>` —
//! Eloquent `Model::query()->latest('id')->first()` shortcut.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "lst_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub title: String,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE lst_post (
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
async fn last_returns_highest_pk_row() {
    let pool = make_pool().await;
    for t in ["a", "b", "c"] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
        };
        p.save_pool(&pool).await.unwrap();
    }
    let last = Post::last(&pool).await.unwrap().unwrap();
    assert_eq!(last.title, "c");
}

#[tokio::test]
async fn last_on_empty_table_returns_none() {
    let pool = make_pool().await;
    assert!(Post::last(&pool).await.unwrap().is_none());
}
