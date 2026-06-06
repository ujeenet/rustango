#![cfg(feature = "sqlite")]
//! Live SQLite test for the macro-emitted
//! `Model::pluck_pool::<U>(col, pool)` shortcut — Eloquent
//! `Model::pluck($column)` / Django
//! `Model.objects.values_list('col', flat=True)` parity.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mpp_post")]
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
        "CREATE TABLE mpp_post (
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
    for (t, v) in [("alpha", 10), ("beta", 50), ("gamma", 250)] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
            views: v,
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn pluck_pool_strings() {
    let pool = make_pool().await;
    seed(&pool).await;
    let titles: Vec<String> = Post::pluck_pool::<String>("title", &pool).await.unwrap();
    assert_eq!(titles.len(), 3);
    assert!(titles.contains(&"alpha".to_owned()));
    assert!(titles.contains(&"beta".to_owned()));
    assert!(titles.contains(&"gamma".to_owned()));
}

#[tokio::test]
async fn pluck_pool_integers() {
    let pool = make_pool().await;
    seed(&pool).await;
    let mut counts: Vec<i64> = Post::pluck_pool::<i64>("views", &pool).await.unwrap();
    counts.sort();
    assert_eq!(counts, vec![10, 50, 250]);
}

#[tokio::test]
async fn pluck_pool_empty_table_returns_empty_vec() {
    let pool = make_pool().await;
    let titles: Vec<String> = Post::pluck_pool::<String>("title", &pool).await.unwrap();
    assert!(titles.is_empty());
}
