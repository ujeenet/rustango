//! Tests for `Model::save_partial_typed((Col, Col), &pool)` — the
//! typed-column counterpart to `save_partial` (issue #67). The shape
//! mostly proves that the macro emission compiles and forwards to the
//! string-keyed `save_partial` correctly; the deep semantic coverage
//! lives in `save_partial.rs` already.

#![cfg(feature = "sqlite")]

use rustango::sql::{sqlx, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "spt_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 20)]
    pub status: String,
    pub views: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "spt_author")]
#[allow(dead_code)]
pub struct Author {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 80)]
    pub name: String,
}

async fn fresh_pool() -> Pool {
    let pool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE spt_post (\
            id INTEGER PRIMARY KEY, \
            title TEXT NOT NULL, \
            status TEXT NOT NULL, \
            views INTEGER NOT NULL DEFAULT 0)",
    )
    .execute(&pool)
    .await
    .expect("create table");
    sqlx::query("INSERT INTO spt_post (id, title, status, views) VALUES (1, 'orig', 'draft', 0)")
        .execute(&pool)
        .await
        .expect("seed row");
    Pool::Sqlite(pool)
}

/// Single-field tuple `(Post::title,)` narrows the UPDATE to one column.
#[tokio::test]
async fn single_field_tuple_narrows_update() {
    let pool = fresh_pool().await;
    let mut row = Post {
        id: 1,
        title: "rewritten".into(),
        status: "rewritten-status".into(),
        views: 999,
    };
    // One-field tuple needs the trailing comma — standard Rust idiom.
    row.save_partial_typed((Post::title,), &pool).await.unwrap();
    if let Pool::Sqlite(sq) = &pool {
        let (title, status, views): (String, String, i64) =
            sqlx::query_as("SELECT title, status, views FROM spt_post WHERE id = 1")
                .fetch_one(sq)
                .await
                .unwrap();
        assert_eq!(title, "rewritten");
        assert_eq!(status, "draft", "status not in tuple → DB value preserved");
        assert_eq!(views, 0);
    }
}

/// Two-field tuple `(Post::title, Post::views)` rewrites both, leaves
/// the third (status) untouched.
#[tokio::test]
async fn multi_field_tuple_narrows_update() {
    let pool = fresh_pool().await;
    let mut row = Post {
        id: 1,
        title: "new-title".into(),
        status: "would-be-overwritten".into(),
        views: 50,
    };
    row.save_partial_typed((Post::title, Post::views), &pool)
        .await
        .unwrap();
    if let Pool::Sqlite(sq) = &pool {
        let (title, status, views): (String, String, i64) =
            sqlx::query_as("SELECT title, status, views FROM spt_post WHERE id = 1")
                .fetch_one(sq)
                .await
                .unwrap();
        assert_eq!(title, "new-title");
        assert_eq!(status, "draft", "status not in tuple → untouched");
        assert_eq!(views, 50);
    }
}

/// Sanity — the typed tuple's `rust_field_names()` returns the Rust-side
/// field names, the same shape the string API takes. This pins the
/// trait contract so a future macro change can't silently emit SQL
/// column names instead.
#[test]
fn typed_field_list_returns_rust_field_names() {
    use rustango::core::TypedFieldList;
    let names = (Post::title, Post::views).rust_field_names();
    assert_eq!(names, vec!["title", "views"]);
    let one = (Post::status,).rust_field_names();
    assert_eq!(one, vec!["status"]);
}

// Cross-model tuples are a compile error — `Author::name`'s
// `Column::Model = Author`, but `Post::save_partial_typed<L>` is
// bounded `L: TypedFieldList<Post>`, which requires every tuple slot
// to be `Column<Model = Post>`. Asserting "this code shouldn't
// compile" needs `trybuild`; pulling that in just for one check is
// heavier than the doc-only note here.
//
// If you want to confirm the invariant manually, drop this into the
// test body and rebuild — it should fail with `the trait bound
// Author_cols::name_col: Column<Model = Post> is not satisfied`:
//
//     row.save_partial_typed((Post::title, Author::name), &pool).await
//
// The presence of the working `multi_field_tuple_narrows_update`
// test above proves the trait bound IS in place; if it were missing
// or relaxed, that test still passes but cross-model wouldn't error.
