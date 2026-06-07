#![cfg(feature = "sqlite")]
//! Live SQLite tests for `Model::find_or_pool` / `first_or_pool` /
//! `sole_pool`. Eloquent `findOr` / `firstOr` / `sole` parity.

use rustango::sql::{sqlx, Auto, ExecError, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mfs_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 80)]
    pub status: String,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE mfs_post (
            id     INTEGER PRIMARY KEY AUTOINCREMENT,
            title  TEXT NOT NULL,
            status TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

fn anon() -> Post {
    Post {
        id: Auto::default(),
        title: "anonymous".into(),
        status: "ghost".into(),
    }
}

#[tokio::test]
async fn find_or_pool_returns_row_when_found() {
    let pool = make_pool().await;
    let mut p = Post {
        id: Auto::default(),
        title: "real".into(),
        status: "live".into(),
    };
    p.save_pool(&pool).await.unwrap();
    let r = Post::find_or(1_i64, &pool, anon).await.unwrap();
    assert_eq!(r.title, "real");
}

#[tokio::test]
async fn find_or_pool_returns_fallback_when_missing() {
    let pool = make_pool().await;
    let r = Post::find_or(999_i64, &pool, anon).await.unwrap();
    assert_eq!(r.title, "anonymous");
    assert_eq!(r.status, "ghost");
}

#[tokio::test]
async fn first_or_pool_returns_first_when_present() {
    let pool = make_pool().await;
    let mut p = Post {
        id: Auto::default(),
        title: "first".into(),
        status: "live".into(),
    };
    p.save_pool(&pool).await.unwrap();
    let r = Post::first_or(&pool, anon).await.unwrap();
    assert_eq!(r.title, "first");
}

#[tokio::test]
async fn first_or_pool_returns_fallback_on_empty() {
    let pool = make_pool().await;
    let r = Post::first_or(&pool, anon).await.unwrap();
    assert_eq!(r.title, "anonymous");
}

#[tokio::test]
async fn sole_pool_returns_row_on_unique_match() {
    let pool = make_pool().await;
    for (t, s) in [("a", "draft"), ("b", "published")] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
            status: s.into(),
        };
        p.save_pool(&pool).await.unwrap();
    }
    let r = Post::sole("status", "published", &pool).await.unwrap();
    assert_eq!(r.title, "b");
}

#[tokio::test]
async fn sole_pool_errors_on_zero_match() {
    let pool = make_pool().await;
    let err = Post::sole("status", "nope", &pool).await.unwrap_err();
    assert!(matches!(err, ExecError::Driver(sqlx::Error::RowNotFound)));
}

#[tokio::test]
async fn sole_pool_errors_on_multi_match() {
    let pool = make_pool().await;
    for t in ["a", "b", "c"] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
            status: "draft".into(),
        };
        p.save_pool(&pool).await.unwrap();
    }
    let err = Post::sole("status", "draft", &pool).await.unwrap_err();
    match err {
        ExecError::MultipleRowsReturned { op, count, .. } => {
            assert_eq!(op, "sole");
            assert_eq!(count, 3);
        }
        other => panic!("expected MultipleRowsReturned, got {other:?}"),
    }
}
