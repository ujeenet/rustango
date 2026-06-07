#![cfg(feature = "sqlite")]
//! Live SQLite tests for `Model::is` / `Model::is_not` (Eloquent
//! `$model->is($other)`) + `Model::value::<U>(col, &pool)`
//! (Eloquent `Model::query()->value($col)`).

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mip_post")]
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
        "CREATE TABLE mip_post (
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
    for (t, v) in [("alpha", 10), ("beta", 20), ("gamma", 30)] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
            views: v,
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn is_returns_true_for_same_pk_and_false_otherwise() {
    let pool = make_pool().await;
    seed(&pool).await;
    let alpha = Post::find(1_i64, &pool).await.unwrap().unwrap();
    let alpha_again = Post::find(1_i64, &pool).await.unwrap().unwrap();
    let beta = Post::find(2_i64, &pool).await.unwrap().unwrap();
    assert!(alpha.is(&alpha_again));
    assert!(!alpha.is(&beta));
    assert!(alpha.is_not(&beta));
    assert!(!alpha.is_not(&alpha_again));
}

#[tokio::test]
async fn value_pool_returns_scalar_from_first_row() {
    let pool = make_pool().await;
    seed(&pool).await;
    let title: Option<String> = Post::value::<String>("title", &pool).await.unwrap();
    assert_eq!(title.as_deref(), Some("alpha"));
    let views: Option<i64> = Post::value::<i64>("views", &pool).await.unwrap();
    assert_eq!(views, Some(10));
}

#[tokio::test]
async fn value_pool_unknown_field_errors() {
    let pool = make_pool().await;
    seed(&pool).await;
    let err = Post::value::<String>("nope", &pool).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("nope"),
        "expected UnknownField for `nope`, got: {msg}"
    );
}
