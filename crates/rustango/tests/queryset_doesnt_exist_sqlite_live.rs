#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet::doesnt_exist(&pool)` — Eloquent
//! `Builder::doesntExist()` alias for `is_empty()`.

use rustango::sql::{sqlx, Auto, ExistsPool as _, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "dne_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub published: bool,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE dne_post (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            published INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    p.into()
}

async fn insert(pool: &Pool, published: bool) {
    let mut p = Post {
        id: Auto::default(),
        published,
    };
    p.save_pool(pool).await.unwrap();
}

#[tokio::test]
async fn doesnt_exist_true_on_empty_scope() {
    let pool = make_pool().await;
    insert(&pool, false).await;
    insert(&pool, false).await;
    assert!(Post::objects()
        .filter("published", true)
        .doesnt_exist(&pool)
        .await
        .unwrap());
}

#[tokio::test]
async fn doesnt_exist_false_when_any_row_matches() {
    let pool = make_pool().await;
    insert(&pool, true).await;
    insert(&pool, false).await;
    assert!(!Post::objects()
        .filter("published", true)
        .doesnt_exist(&pool)
        .await
        .unwrap());
}

#[tokio::test]
async fn doesnt_exist_true_on_empty_table() {
    let pool = make_pool().await;
    assert!(Post::objects().doesnt_exist(&pool).await.unwrap());
}
