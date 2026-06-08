#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet::sole(&pool)` — Eloquent
//! `Builder::sole()` parity: exactly one match required, else error.

use rustango::sql::{sqlx, Auto, ExecError, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "sl_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub slug: String,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE sl_post (
            id   INTEGER PRIMARY KEY AUTOINCREMENT,
            slug TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    p.into()
}

async fn insert(pool: &Pool, slug: &str) {
    let mut row = Post {
        id: Auto::default(),
        slug: slug.into(),
    };
    row.save_pool(pool).await.unwrap();
}

#[tokio::test]
async fn sole_returns_single_match() {
    let pool = make_pool().await;
    insert(&pool, "hello").await;
    insert(&pool, "world").await;
    let row = Post::objects()
        .filter("slug", "hello".to_string())
        .sole(&pool)
        .await
        .unwrap();
    assert_eq!(row.slug, "hello");
}

#[tokio::test]
async fn sole_errors_on_empty_match_with_row_not_found() {
    let pool = make_pool().await;
    insert(&pool, "hello").await;
    let err = Post::objects()
        .filter("slug", "missing".to_string())
        .sole(&pool)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ExecError::Driver(sqlx::Error::RowNotFound)),
        "expected RowNotFound, got: {err}"
    );
}

#[tokio::test]
async fn sole_errors_on_multiple_matches_with_multiple_rows_returned() {
    let pool = make_pool().await;
    insert(&pool, "dup").await;
    insert(&pool, "dup").await;
    insert(&pool, "dup").await;
    let err = Post::objects()
        .filter("slug", "dup".to_string())
        .sole(&pool)
        .await
        .unwrap_err();
    match err {
        ExecError::MultipleRowsReturned { op, count, .. } => {
            assert_eq!(op, "sole");
            // Limit 2 caps the count at 2 — that's enough to flag.
            assert_eq!(count, 2);
        }
        other => panic!("expected MultipleRowsReturned, got: {other}"),
    }
}
