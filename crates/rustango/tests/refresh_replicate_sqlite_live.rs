#![cfg(feature = "sqlite")]
//! Live SQLite tests for the model-instance helpers shipped in
//! issue #825:
//!
//! * `refresh_from_db_pool` — re-SELECT this row by PK and overwrite
//!   stale in-memory fields. Django's `refresh_from_db`.
//! * `replicate` — clone-as-insertable. PK reset to `Auto::Unset` for
//!   `Auto<T>` PKs so the next `save_pool` allocates a fresh
//!   autoincrement.
//!
//! Backend-neutral (the methods route through `QuerySet::fetch_pool`
//! + the existing `_pool` save family); SQLite proves the round-trip.

use rustango::sql::{sqlx, Auto, FetcherPool, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone, PartialEq)]
#[rustango(table = "rr_post")]
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
        "CREATE TABLE rr_post (
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

#[tokio::test]
async fn refresh_from_db_picks_up_external_update() {
    let pool = make_pool().await;

    let mut post = Post {
        id: Auto::default(),
        title: "First version".into(),
        views: 10,
    };
    post.save_pool(&pool).await.unwrap();
    let pk = post.id.get().copied().unwrap();

    // Another process / connection updates the row out of band.
    let Pool::Sqlite(raw) = &pool else {
        unreachable!()
    };
    sqlx::query("UPDATE rr_post SET title = ?, views = ? WHERE id = ?")
        .bind("Edited externally")
        .bind(99_i64)
        .bind(pk)
        .execute(raw)
        .await
        .unwrap();

    // In-memory copy is still stale.
    assert_eq!(post.title, "First version");
    assert_eq!(post.views, 10);

    // Refresh — fields should overwrite.
    post.refresh_from_db_pool(&pool).await.unwrap();
    assert_eq!(post.title, "Edited externally");
    assert_eq!(post.views, 99);
    // PK preserved.
    assert_eq!(post.id.get().copied(), Some(pk));
}

#[tokio::test]
async fn refresh_from_db_errors_when_row_was_deleted() {
    let pool = make_pool().await;

    let mut post = Post {
        id: Auto::default(),
        title: "Doomed".into(),
        views: 0,
    };
    post.save_pool(&pool).await.unwrap();
    let pk = post.id.get().copied().unwrap();

    let Pool::Sqlite(raw) = &pool else {
        unreachable!()
    };
    sqlx::query("DELETE FROM rr_post WHERE id = ?")
        .bind(pk)
        .execute(raw)
        .await
        .unwrap();

    let result = post.refresh_from_db_pool(&pool).await;
    assert!(
        result.is_err(),
        "refresh on deleted row must surface RowNotFound"
    );
}

#[tokio::test]
async fn replicate_resets_auto_pk_and_clones_other_fields() {
    let pool = make_pool().await;

    let mut original = Post {
        id: Auto::default(),
        title: "Original".into(),
        views: 7,
    };
    original.save_pool(&pool).await.unwrap();
    let original_pk = original.id.get().copied().unwrap();

    // Clone-as-insertable: PK reset, fields preserved.
    let mut copy = original.replicate();
    assert!(
        matches!(copy.id, Auto::Unset),
        "replicate must reset Auto<T> PK to Unset"
    );
    assert_eq!(copy.title, "Original");
    assert_eq!(copy.views, 7);

    // Saving the copy allocates a fresh PK.
    copy.save_pool(&pool).await.unwrap();
    let copy_pk = copy.id.get().copied().unwrap();
    assert_ne!(copy_pk, original_pk, "fresh autoincrement");

    // Both rows live in the table.
    let rows: Vec<Post> = rustango::query::QuerySet::<Post>::default()
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
}

#[tokio::test]
async fn replicate_then_modify_then_save_does_not_touch_original() {
    let pool = make_pool().await;

    let mut original = Post {
        id: Auto::default(),
        title: "Source".into(),
        views: 1,
    };
    original.save_pool(&pool).await.unwrap();
    let original_pk = original.id.get().copied().unwrap();

    let mut copy = original.replicate();
    copy.title = "Spinoff".into();
    copy.views = 999;
    copy.save_pool(&pool).await.unwrap();

    // Re-read the original to confirm it's untouched.
    original.refresh_from_db_pool(&pool).await.unwrap();
    assert_eq!(original.title, "Source");
    assert_eq!(original.views, 1);
    assert_eq!(original.id.get().copied(), Some(original_pk));
}
