#![cfg(feature = "sqlite")]
//! Live SQLite tests for the macro-emitted NOT-filter shortcuts +
//! schema-meta helpers:
//! `where_not_like_pool` / `where_not_ilike_pool` /
//! `where_not_between_pool` / `table_name` / `primary_key_column`
//! / `get_key`. Eloquent `whereNotLike` / `whereNotILike` /
//! `whereNotBetween` / `getTable` / `getKeyName` / `getKey` parity.

use rustango::core::SqlValue;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mnf_post")]
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
        "CREATE TABLE mnf_post (
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
    for (t, v) in [
        ("alpha", 10_i64),
        ("beta", 20),
        ("gamma", 30),
        ("delta", 40),
    ] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
            views: v,
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn where_not_like_pool_excludes_pattern() {
    let pool = make_pool().await;
    seed(&pool).await;
    let r = Post::where_not_like("title", "a%", &pool).await.unwrap();
    // SQLite LIKE is case-insensitive — alpha excluded, the rest kept.
    let titles: Vec<&str> = r.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(titles.len(), 3);
    assert!(!titles.contains(&"alpha"));
}

#[tokio::test]
async fn where_not_ilike_pool_excludes_pattern_case_insensitive() {
    let pool = make_pool().await;
    seed(&pool).await;
    let r = Post::where_not_ilike("title", "ALPHA", &pool)
        .await
        .unwrap();
    assert_eq!(r.len(), 3);
}

#[tokio::test]
async fn where_not_between_pool_excludes_range() {
    let pool = make_pool().await;
    seed(&pool).await;
    let r = Post::where_not_between("views", 20_i64, 30_i64, &pool)
        .await
        .unwrap();
    let titles: Vec<&str> = r.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(titles.len(), 2);
    assert!(titles.contains(&"alpha"));
    assert!(titles.contains(&"delta"));
}

#[tokio::test]
async fn schema_meta_helpers_return_expected_values() {
    assert_eq!(Post::table_name(), "mnf_post");
    assert_eq!(Post::primary_key_column(), Some("id"));
}

#[tokio::test]
async fn get_key_returns_pk_value_as_sqlvalue() {
    let pool = make_pool().await;
    seed(&pool).await;
    let row = Post::find(2_i64, &pool).await.unwrap().unwrap();
    match row.get_key() {
        SqlValue::I64(2) => {}
        other => panic!("expected SqlValue::I64(2), got {other:?}"),
    }
}
