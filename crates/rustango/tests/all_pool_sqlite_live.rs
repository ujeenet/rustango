#![cfg(feature = "sqlite")]
//! Live SQLite tests for the macro-emitted `Model::all_pool(pool)`
//! shortcut — Eloquent `Model::all()` parity.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "ap_post")]
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
        "CREATE TABLE ap_post (
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
async fn all_pool_returns_every_row() {
    let pool = make_pool().await;
    for t in ["a", "b", "c"] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
        };
        p.save_pool(&pool).await.unwrap();
    }

    let rows = Post::all_pool(&pool).await.unwrap();
    assert_eq!(rows.len(), 3);
    let titles: std::collections::HashSet<&str> = rows.iter().map(|r| r.title.as_str()).collect();
    assert!(titles.contains("a"));
    assert!(titles.contains("b"));
    assert!(titles.contains("c"));
}

#[tokio::test]
async fn all_pool_on_empty_table_returns_empty_vec() {
    let pool = make_pool().await;
    let rows = Post::all_pool(&pool).await.unwrap();
    assert!(rows.is_empty());
}
