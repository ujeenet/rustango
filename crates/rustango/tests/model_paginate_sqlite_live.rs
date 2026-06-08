#![cfg(feature = "sqlite")]
//! Live SQLite test for `Model::paginate(page, per_page, &pool) ->
//! (Vec<Self>, total)` — Eloquent paginate parity.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "pag_post")]
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
        "CREATE TABLE pag_post (
            id    INTEGER PRIMARY KEY AUTOINCREMENT,
            title TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool, n: i32) {
    for i in 0..n {
        let mut p = Post {
            id: Auto::default(),
            title: format!("post-{i}"),
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn paginate_returns_page_rows_and_total() {
    let pool = make_pool().await;
    seed(&pool, 25).await;
    let (rows, total) = Post::paginate(2, 10, &pool).await.unwrap();
    assert_eq!(total, 25);
    assert_eq!(rows.len(), 10);
}

#[tokio::test]
async fn paginate_last_page_returns_partial() {
    let pool = make_pool().await;
    seed(&pool, 25).await;
    let (rows, total) = Post::paginate(3, 10, &pool).await.unwrap();
    assert_eq!(total, 25);
    assert_eq!(rows.len(), 5);
}

#[tokio::test]
async fn paginate_empty_table_returns_empty_with_zero_total() {
    let pool = make_pool().await;
    let (rows, total) = Post::paginate(1, 10, &pool).await.unwrap();
    assert_eq!(total, 0);
    assert!(rows.is_empty());
}
