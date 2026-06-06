#![cfg(feature = "sqlite")]
//! Live SQLite tests for the macro-emitted
//! `Model::increment_pool(col, by, pool)` /
//! `Model::decrement_pool(col, by, pool)` shortcuts — Eloquent
//! `Model::increment` / `decrement` parity.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "min_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub views: i64,
    pub likes: i64,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE min_post (
            id    INTEGER PRIMARY KEY AUTOINCREMENT,
            title TEXT NOT NULL,
            views INTEGER NOT NULL,
            likes INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool) -> i64 {
    let mut p = Post {
        id: Auto::default(),
        title: "x".into(),
        views: 10,
        likes: 5,
    };
    p.save_pool(pool).await.unwrap();
    p.id.get().copied().unwrap()
}

#[tokio::test]
async fn increment_pool_bumps_by_n() {
    let pool = make_pool().await;
    let pk = seed(&pool).await;
    let row = Post::find_or_fail_pool(pk, &pool).await.unwrap();

    let n = row.increment_pool("views", 5, &pool).await.unwrap();
    assert_eq!(n, 1);

    // Re-fetch to confirm persisted value.
    let fresh = Post::find_or_fail_pool(pk, &pool).await.unwrap();
    assert_eq!(fresh.views, 15);
    // Other columns untouched.
    assert_eq!(fresh.likes, 5);
}

#[tokio::test]
async fn decrement_pool_subtracts() {
    let pool = make_pool().await;
    let pk = seed(&pool).await;
    let row = Post::find_or_fail_pool(pk, &pool).await.unwrap();

    let n = row.decrement_pool("views", 3, &pool).await.unwrap();
    assert_eq!(n, 1);

    let fresh = Post::find_or_fail_pool(pk, &pool).await.unwrap();
    assert_eq!(fresh.views, 7);
}

#[tokio::test]
async fn increment_pool_unknown_field_errors() {
    let pool = make_pool().await;
    let pk = seed(&pool).await;
    let row = Post::find_or_fail_pool(pk, &pool).await.unwrap();

    let res = row.increment_pool("nope", 1, &pool).await;
    assert!(res.is_err(), "unknown field must surface as error");
}

#[tokio::test]
async fn increment_pool_does_not_mutate_self() {
    let pool = make_pool().await;
    let pk = seed(&pool).await;
    let row = Post::find_or_fail_pool(pk, &pool).await.unwrap();
    let original_views = row.views;

    row.increment_pool("views", 100, &pool).await.unwrap();
    // self stays stale — caller must refresh_from_db_pool / fresh_pool.
    assert_eq!(row.views, original_views);
}
