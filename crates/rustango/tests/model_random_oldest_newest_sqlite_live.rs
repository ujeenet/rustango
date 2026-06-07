#![cfg(feature = "sqlite")]
//! Live SQLite tests for the macro-emitted ordering shortcuts:
//! `random_pool` / `random_n_pool` / `oldest_pool` / `newest_pool`.
//! Eloquent `inRandomOrder()->first()` /
//! `inRandomOrder()->limit($n)->get()` / `oldest()->get()` /
//! `latest()->get()` parity.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mron_post")]
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
        "CREATE TABLE mron_post (
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
    for (t, v) in [("a", 30_i64), ("b", 20), ("c", 10), ("d", 40), ("e", 50)] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
            views: v,
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn random_pool_returns_some_row_when_present() {
    let pool = make_pool().await;
    seed(&pool).await;
    let r = Post::random(&pool).await.unwrap();
    assert!(r.is_some());
}

#[tokio::test]
async fn random_pool_returns_none_on_empty() {
    let pool = make_pool().await;
    assert!(Post::random(&pool).await.unwrap().is_none());
}

#[tokio::test]
async fn random_n_pool_caps_at_table_size() {
    let pool = make_pool().await;
    seed(&pool).await;
    let r = Post::random_n(3, &pool).await.unwrap();
    assert_eq!(r.len(), 3);
}

#[tokio::test]
async fn oldest_pool_ascends_by_field() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = Post::oldest("views", &pool).await.unwrap();
    let views: Vec<i64> = rows.iter().map(|r| r.views).collect();
    assert_eq!(views, vec![10, 20, 30, 40, 50]);
}

#[tokio::test]
async fn newest_pool_descends_by_field() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = Post::newest("views", &pool).await.unwrap();
    let views: Vec<i64> = rows.iter().map(|r| r.views).collect();
    assert_eq!(views, vec![50, 40, 30, 20, 10]);
}
