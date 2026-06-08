#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet::where_key(pk)` /
//! `QuerySet::where_key_not(pk)` — Eloquent
//! `Builder::whereKey($pk)` / `Builder::whereKeyNot($pk)` parity that
//! filters on the model's PK column without spelling its name.

use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "wk_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 40)]
    pub title: String,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE wk_post (
            id    INTEGER PRIMARY KEY AUTOINCREMENT,
            title TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    p.into()
}

async fn seed(pool: &Pool) -> Vec<i64> {
    let mut ids = Vec::new();
    for t in ["a", "b", "c", "d"] {
        let mut row = Post {
            id: Auto::default(),
            title: t.into(),
        };
        row.save_pool(pool).await.unwrap();
        ids.push(*row.id.get().unwrap());
    }
    ids
}

#[tokio::test]
async fn where_key_filters_to_single_row() {
    let pool = make_pool().await;
    let ids = seed(&pool).await;
    let target = ids[2];
    let rows = Post::objects()
        .where_key(target)
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(*rows[0].id.get().unwrap(), target);
    assert_eq!(rows[0].title, "c");
}

#[tokio::test]
async fn where_key_not_excludes_pk() {
    let pool = make_pool().await;
    let ids = seed(&pool).await;
    let excluded = ids[1];
    let mut got: Vec<i64> = Post::objects()
        .where_key_not(excluded)
        .fetch_pool(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|r| *r.id.get().unwrap())
        .collect();
    got.sort_unstable();
    let mut expected: Vec<i64> = ids.into_iter().filter(|&i| i != excluded).collect();
    expected.sort_unstable();
    assert_eq!(got, expected);
}

#[tokio::test]
async fn where_key_no_match_returns_empty() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = Post::objects()
        .where_key(999_999_i64)
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert!(rows.is_empty());
}
