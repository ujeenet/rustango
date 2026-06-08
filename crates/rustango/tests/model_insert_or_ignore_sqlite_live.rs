#![cfg(feature = "sqlite")]
//! Live SQLite test for `Model::insert_or_ignore(&pool)` —
//! Eloquent `Model::insertOrIgnore()` parity.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "ioi_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80, unique)]
    pub slug: String,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE ioi_post (
            id   INTEGER PRIMARY KEY AUTOINCREMENT,
            slug TEXT NOT NULL UNIQUE
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

#[tokio::test]
async fn insert_or_ignore_returns_true_when_row_is_new() {
    let pool = make_pool().await;
    let mut p = Post {
        id: Auto::default(),
        slug: "hello-world".into(),
    };
    let inserted = p.insert_or_ignore(&pool).await.unwrap();
    assert!(inserted);
}

#[tokio::test]
async fn insert_or_ignore_returns_false_when_unique_conflict() {
    let pool = make_pool().await;
    let mut p1 = Post {
        id: Auto::default(),
        slug: "dup".into(),
    };
    p1.save_pool(&pool).await.unwrap();

    // Second insert with the same unique slug → conflict → skip.
    let mut p2 = Post {
        id: Auto::default(),
        slug: "dup".into(),
    };
    let inserted = p2.insert_or_ignore(&pool).await.unwrap();
    assert!(
        !inserted,
        "duplicate slug must hit the unique conflict + DO NOTHING"
    );
}
