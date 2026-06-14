#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet::is_empty(&pool)` — inverse of
//! `exists`, more readable in negation-flavored code.

use rustango::sql::{sqlx, Auto, ExistsPool as _, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "ie_post")]
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
        "CREATE TABLE ie_post (
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
async fn is_empty_returns_true_for_empty_filter() {
    let pool = make_pool().await;
    assert!(Post::objects().is_empty(&pool).await.unwrap());
}

#[tokio::test]
async fn is_empty_returns_false_when_rows_match() {
    let pool = make_pool().await;
    let mut p = Post {
        id: Auto::default(),
        title: "alpha".into(),
    };
    p.save_pool(&pool).await.unwrap();
    assert!(!Post::objects().is_empty(&pool).await.unwrap());
}

#[tokio::test]
async fn is_empty_is_inverse_of_exists() {
    let pool = make_pool().await;
    // Empty table — exists=false, is_empty=true.
    assert_eq!(
        Post::objects().exists(&pool).await.unwrap(),
        !Post::objects().is_empty(&pool).await.unwrap()
    );

    // After insert — exists=true, is_empty=false.
    let mut p = Post {
        id: Auto::default(),
        title: "beta".into(),
    };
    p.save_pool(&pool).await.unwrap();
    assert_eq!(
        Post::objects().exists(&pool).await.unwrap(),
        !Post::objects().is_empty(&pool).await.unwrap()
    );
}
