#![cfg(feature = "sqlite")]
//! Live SQLite test for `Model::find_or_new(pk, &pool, make_fn)` —
//! Eloquent `findOrNew` parity returning (Self, exists: bool).

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "fon_post")]
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
        "CREATE TABLE fon_post (
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
async fn find_or_new_returns_existing_with_exists_true() {
    let pool = make_pool().await;
    let mut p = Post {
        id: Auto::default(),
        title: "real".into(),
    };
    p.save_pool(&pool).await.unwrap();
    let pk = p.id.get().copied().unwrap();

    let (found, exists) = Post::find_or_new(pk, &pool, || Post {
        id: Auto::default(),
        title: "fallback".into(),
    })
    .await
    .unwrap();
    assert!(exists);
    assert_eq!(found.title, "real");
}

#[tokio::test]
async fn find_or_new_returns_fallback_with_exists_false() {
    let pool = make_pool().await;
    let (built, exists) = Post::find_or_new(999_999_i64, &pool, || Post {
        id: Auto::default(),
        title: "fallback".into(),
    })
    .await
    .unwrap();
    assert!(!exists);
    assert_eq!(built.title, "fallback");
}
