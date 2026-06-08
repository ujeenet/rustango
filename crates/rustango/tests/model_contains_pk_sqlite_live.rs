#![cfg(feature = "sqlite")]
//! Live SQLite test for `Model::contains_pk(pk, &pool)` —
//! Eloquent `Model::query()->whereKey($pk)->exists()` shortcut.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "cpk_post")]
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
        "CREATE TABLE cpk_post (
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
async fn contains_pk_returns_true_when_row_exists() {
    let pool = make_pool().await;
    let mut p = Post {
        id: Auto::default(),
        title: "alpha".into(),
    };
    p.save_pool(&pool).await.unwrap();
    let pk = p.id.get().copied().unwrap();
    assert!(Post::contains_pk(pk, &pool).await.unwrap());
}

#[tokio::test]
async fn contains_pk_returns_false_when_pk_missing() {
    let pool = make_pool().await;
    assert!(!Post::contains_pk(999_999_i64, &pool).await.unwrap());
}

#[tokio::test]
async fn contains_pk_on_empty_table_returns_false() {
    let pool = make_pool().await;
    assert!(!Post::contains_pk(1_i64, &pool).await.unwrap());
}
