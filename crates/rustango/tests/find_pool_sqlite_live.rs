#![cfg(feature = "sqlite")]
//! Live SQLite tests for the macro-emitted `Model::find(pk, pool)`
//! shortcut — Eloquent `Model::find()` / Django `Model.objects.get(pk=)`
//! parity (non-throwing, returns `Option<Self>`).

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "fp_post")]
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
        "CREATE TABLE fp_post (
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
async fn find_pool_returns_some_for_existing_pk() {
    let pool = make_pool().await;
    let mut p1 = Post {
        id: Auto::default(),
        title: "first".into(),
    };
    p1.save_pool(&pool).await.unwrap();
    let pk = p1.id.get().copied().unwrap();

    let found = Post::find(pk, &pool).await.unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().title, "first");
}

#[tokio::test]
async fn find_pool_returns_none_for_missing_pk() {
    let pool = make_pool().await;
    // No rows seeded.
    let found = Post::find(9999_i64, &pool).await.unwrap();
    assert!(found.is_none(), "non-existent PK returns None");
}

#[tokio::test]
async fn find_pool_disambiguates_between_rows() {
    let pool = make_pool().await;
    let mut a = Post {
        id: Auto::default(),
        title: "a".into(),
    };
    a.save_pool(&pool).await.unwrap();
    let mut b = Post {
        id: Auto::default(),
        title: "b".into(),
    };
    b.save_pool(&pool).await.unwrap();
    let pk_b = b.id.get().copied().unwrap();

    let found = Post::find(pk_b, &pool).await.unwrap().unwrap();
    assert_eq!(found.title, "b");
}
