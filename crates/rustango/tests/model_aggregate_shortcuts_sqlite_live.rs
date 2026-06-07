#![cfg(feature = "sqlite")]
//! Live SQLite tests for the macro-emitted aggregate-shortcut
//! family: `Model::sum_pool` / `avg_pool` / `min_pool` / `max_pool`
//! / `doesnt_exist_pool`. Eloquent `Model::sum($col)` /
//! `Model::avg($col)` / `Model::min($col)` / `Model::max($col)` /
//! `Model::doesntExist()` parity.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mas_post")]
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
        "CREATE TABLE mas_post (
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
    for (t, v) in [("alpha", 10_i64), ("beta", 20), ("gamma", 30)] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
            views: v,
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn aggregate_quartet_on_seeded_table() {
    let pool = make_pool().await;
    seed(&pool).await;
    let s = Post::sum_pool::<i64>("views", &pool).await.unwrap();
    assert_eq!(s, Some(60));
    let mn = Post::min_pool::<i64>("views", &pool).await.unwrap();
    assert_eq!(mn, Some(10));
    let mx = Post::max_pool::<i64>("views", &pool).await.unwrap();
    assert_eq!(mx, Some(30));
    let av = Post::avg_pool::<f64>("views", &pool).await.unwrap();
    assert!(av.is_some());
    assert!((av.unwrap() - 20.0).abs() < 0.0001);
}

#[tokio::test]
async fn aggregate_returns_none_on_empty_table() {
    let pool = make_pool().await;
    assert_eq!(Post::sum_pool::<i64>("views", &pool).await.unwrap(), None);
    assert_eq!(Post::min_pool::<i64>("views", &pool).await.unwrap(), None);
    assert_eq!(Post::max_pool::<i64>("views", &pool).await.unwrap(), None);
    assert_eq!(Post::avg_pool::<f64>("views", &pool).await.unwrap(), None);
}

#[tokio::test]
async fn doesnt_exist_pool_is_inverse_of_exists() {
    let pool = make_pool().await;
    assert!(Post::doesnt_exist_pool(&pool).await.unwrap());
    assert!(!Post::exists_pool(&pool).await.unwrap());
    seed(&pool).await;
    assert!(!Post::doesnt_exist_pool(&pool).await.unwrap());
    assert!(Post::exists_pool(&pool).await.unwrap());
}

#[tokio::test]
async fn aggregate_unknown_field_errors() {
    let pool = make_pool().await;
    seed(&pool).await;
    let err = Post::sum_pool::<i64>("nope", &pool).await.unwrap_err();
    assert!(err.to_string().contains("nope"));
}
