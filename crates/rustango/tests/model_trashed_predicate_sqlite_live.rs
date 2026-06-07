#![cfg(feature = "sqlite")]
//! Live SQLite tests for the macro-emitted `Model::trashed(&self) ->
//! bool` predicate. Eloquent `$model->trashed()` parity — returns
//! whether the row's `#[rustango(soft_delete)]` column is currently
//! set. Pure in-memory check, does not hit the DB.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mtr_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(soft_delete)]
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE mtr_post (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            title      TEXT NOT NULL,
            deleted_at TEXT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

#[tokio::test]
async fn trashed_is_false_for_live_row() {
    let pool = make_pool().await;
    let mut p = Post {
        id: Auto::default(),
        title: "hi".into(),
        deleted_at: None,
    };
    p.save_pool(&pool).await.unwrap();
    assert!(!p.trashed());
}

#[tokio::test]
async fn trashed_is_true_after_soft_delete_and_refetch() {
    let pool = make_pool().await;
    let mut p = Post {
        id: Auto::default(),
        title: "hi".into(),
        deleted_at: None,
    };
    p.save_pool(&pool).await.unwrap();
    p.soft_delete_pool(&pool).await.unwrap();
    // The local instance still has deleted_at=None; trashed() is a
    // pure in-memory predicate so we must refetch from DB.
    let only_trashed = Post::only_trashed_pool(&pool).await.unwrap();
    assert_eq!(only_trashed.len(), 1);
    assert!(only_trashed[0].trashed());
}

#[tokio::test]
async fn trashed_flips_on_in_memory_field_set() {
    // Pure-predicate behavior: setting the field directly flips
    // the answer without touching the DB.
    let mut p = Post {
        id: Auto::default(),
        title: "hi".into(),
        deleted_at: None,
    };
    assert!(!p.trashed());
    p.deleted_at = Some(chrono::Utc::now());
    assert!(p.trashed());
}
