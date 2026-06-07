#![cfg(feature = "sqlite")]
//! Live SQLite tests for the macro-emitted LIKE-pattern WHERE
//! shortcuts: `where_like_pool` / `where_ilike_pool` /
//! `where_starts_with_pool` / `where_ends_with_pool` /
//! `where_contains_pool`. Eloquent `whereLike` /
//! `whereLikeI` / `whereLike("col", "$prefix%")` /
//! `whereLike("col", "%$suffix")` / `whereLike("col", "%$mid%")`
//! parity.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mwl_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE mwl_post (
            id    INTEGER PRIMARY KEY AUTOINCREMENT,
            title TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool) {
    for t in [
        "Rust at scale",
        "rust for beginners",
        "Postgres tips",
        "Database design",
        "Rust async runtimes",
    ] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn where_like_pool_matches_explicit_wildcards() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = Post::where_like("title", "Rust%", &pool).await.unwrap();
    let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
    // SQLite's LIKE is case-insensitive by default — matches both
    // 'Rust*' and 'rust*' rows.
    assert_eq!(titles.len(), 3);
    assert!(titles.contains(&"Rust at scale"));
    assert!(titles.contains(&"Rust async runtimes"));
    assert!(titles.contains(&"rust for beginners"));
}

#[tokio::test]
async fn where_ilike_pool_is_case_insensitive() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = Post::where_ilike("title", "rust%", &pool).await.unwrap();
    assert_eq!(rows.len(), 3);
}

#[tokio::test]
async fn where_starts_with_pool_auto_appends_percent() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = Post::where_starts_with("title", "Rust", &pool)
        .await
        .unwrap();
    // SQLite LIKE is case-insensitive → matches both Rust* + rust*.
    assert_eq!(rows.len(), 3);
}

#[tokio::test]
async fn where_ends_with_pool_auto_prepends_percent() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = Post::where_ends_with("title", "runtimes", &pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, "Rust async runtimes");
}

#[tokio::test]
async fn where_contains_pool_wraps_both_sides() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = Post::where_contains("title", "ips", &pool).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, "Postgres tips");
}
