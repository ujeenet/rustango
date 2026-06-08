#![cfg(feature = "sqlite")]
//! Live SQLite test for `Model::find_or_insert(pk, &pool, fallback)`
//! — Eloquent `findOrCreate` parity (persisting variant of
//! `find_or_new`). Returns `(row, exists: bool)`.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "foi_post")]
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
        "CREATE TABLE foi_post (
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
async fn find_or_insert_returns_existing_row() {
    let pool = make_pool().await;
    let mut p = Post {
        id: Auto::default(),
        title: "existing".into(),
    };
    p.save_pool(&pool).await.unwrap();
    let pk = p.id.get().copied().unwrap();

    let (found, exists) = Post::find_or_insert(pk, &pool, || Post {
        id: Auto::default(),
        title: "fallback".into(),
    })
    .await
    .unwrap();
    assert!(exists, "row existed → exists=true");
    assert_eq!(found.title, "existing");
}

#[tokio::test]
async fn find_or_insert_persists_fallback_when_missing() {
    let pool = make_pool().await;
    let (inserted, exists) = Post::find_or_insert(999_999_i64, &pool, || Post {
        id: Auto::default(),
        title: "new-row".into(),
    })
    .await
    .unwrap();
    assert!(!exists, "row was missing → exists=false");
    assert_eq!(inserted.title, "new-row");
    // PK back-propagated.
    assert!(inserted.id.get().is_some());

    // Verify it's actually in the DB.
    let total = Post::count(&pool).await.unwrap();
    assert_eq!(total, 1);
}
