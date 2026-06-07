#![cfg(feature = "sqlite")]
//! Live SQLite tests for the macro-emitted soft-delete instance
//! methods on the `_pool` family — closes #821's `restore` /
//! `forceDelete` parity items.
//!
//! * `Self::soft_delete(pool)` — set `deleted_at` to NOW().
//! * `Self::restore(pool)` — clear `deleted_at` back to NULL.
//! * `Self::force_delete(pool)` — alias of `delete_pool`
//!   (hard DELETE, ignores soft-delete column).

use chrono::{DateTime, Utc};
use rustango::query::QuerySet;
use rustango::sql::{sqlx, Auto, FetcherPool, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "sdp_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(soft_delete)]
    pub deleted_at: Option<DateTime<Utc>>,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE sdp_post (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            title      TEXT NOT NULL,
            deleted_at TEXT
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed_one(pool: &Pool) -> i64 {
    let mut p = Post {
        id: Auto::default(),
        title: "subject".into(),
        deleted_at: None,
    };
    p.save_pool(pool).await.unwrap();
    p.id.get().copied().unwrap()
}

#[tokio::test]
async fn soft_delete_pool_marks_row_trashed_but_keeps_in_table() {
    let pool = make_pool().await;
    let pk = seed_one(&pool).await;

    // Seed row is alive.
    let row: Post = Post::find_or_fail(pk, &pool).await.unwrap();
    assert!(row.deleted_at.is_none());

    // Soft-delete.
    let affected = row.soft_delete(&pool).await.unwrap();
    assert_eq!(affected, 1);

    // Row still exists in the table; `active()` excludes it.
    let all_with_trashed: Vec<Post> = QuerySet::<Post>::default()
        .with_trashed()
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert_eq!(all_with_trashed.len(), 1);
    assert!(all_with_trashed[0].deleted_at.is_some());

    let actives: Vec<Post> = QuerySet::<Post>::default()
        .active()
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert!(actives.is_empty(), "active() must hide trashed rows");
}

#[tokio::test]
async fn restore_pool_clears_deleted_at_back_to_null() {
    let pool = make_pool().await;
    let pk = seed_one(&pool).await;

    let row = Post::find_or_fail(pk, &pool).await.unwrap();
    row.soft_delete(&pool).await.unwrap();

    // Re-fetch the trashed version, then restore.
    let trashed: Post = QuerySet::<Post>::default()
        .only_trashed()
        .fetch_pool(&pool)
        .await
        .unwrap()
        .pop()
        .expect("trashed row present");
    let affected = trashed.restore(&pool).await.unwrap();
    assert_eq!(affected, 1);

    // Back to active.
    let actives: Vec<Post> = QuerySet::<Post>::default()
        .active()
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert_eq!(actives.len(), 1);
    assert!(actives[0].deleted_at.is_none());
}

#[tokio::test]
async fn force_delete_pool_actually_removes_the_row() {
    let pool = make_pool().await;
    let pk = seed_one(&pool).await;

    let row = Post::find_or_fail(pk, &pool).await.unwrap();
    let affected = row.force_delete(&pool).await.unwrap();
    assert_eq!(affected, 1);

    // Row is gone for real — not even `with_trashed` brings it back.
    let all: Vec<Post> = QuerySet::<Post>::default()
        .with_trashed()
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert!(all.is_empty(), "force_delete_pool removes the row entirely");
}
