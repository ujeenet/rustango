#![cfg(feature = "sqlite")]
//! Live SQLite tests for the macro-emitted WHERE-predicate
//! shortcuts: `where_in_pool` / `where_not_in_pool` /
//! `where_null_pool` / `where_not_null_pool` /
//! `where_between_pool`. Eloquent `whereIn` / `whereNotIn` /
//! `whereNull` / `whereNotNull` / `whereBetween` parity.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mwp2_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 80)]
    pub status: Option<String>,
    pub views: i64,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE mwp2_post (
            id     INTEGER PRIMARY KEY AUTOINCREMENT,
            title  TEXT NOT NULL,
            status TEXT NULL,
            views  INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool) {
    for (title, status, views) in [
        ("a", Some("draft"), 10_i64),
        ("b", Some("published"), 20),
        ("c", Some("archived"), 30),
        ("d", None, 40),
    ] {
        let mut p = Post {
            id: Auto::default(),
            title: title.into(),
            status: status.map(str::to_string),
            views,
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn where_in_pool_filters_matching_rows() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = Post::where_in("status", ["draft", "published"], &pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
}

#[tokio::test]
async fn where_in_pool_empty_returns_no_rows() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows: Vec<Post> = Post::where_in::<&str>("status", Vec::<&str>::new(), &pool)
        .await
        .unwrap();
    assert!(rows.is_empty());
}

#[tokio::test]
async fn where_not_in_pool_excludes_listed_values() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = Post::where_not_in("status", ["archived"], &pool)
        .await
        .unwrap();
    // 'archived' excluded; NULL doesn't match NOT IN under SQL semantics
    // → expect rows with 'draft' and 'published' only (2).
    let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
    assert!(titles.contains(&"a"));
    assert!(titles.contains(&"b"));
    assert!(!titles.contains(&"c"));
}

#[tokio::test]
async fn where_not_in_pool_empty_returns_all_rows() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows: Vec<Post> = Post::where_not_in::<&str>("status", Vec::<&str>::new(), &pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 4);
}

#[tokio::test]
async fn where_null_and_not_null_split_rows() {
    let pool = make_pool().await;
    seed(&pool).await;
    let nulls = Post::where_null("status", &pool).await.unwrap();
    assert_eq!(nulls.len(), 1);
    assert_eq!(nulls[0].title, "d");
    let non_nulls = Post::where_not_null("status", &pool).await.unwrap();
    assert_eq!(non_nulls.len(), 3);
}

#[tokio::test]
async fn where_between_pool_inclusive_bounds() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = Post::where_between("views", 20_i64, 40_i64, &pool)
        .await
        .unwrap();
    let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(titles.len(), 3);
    assert!(titles.contains(&"b"));
    assert!(titles.contains(&"c"));
    assert!(titles.contains(&"d"));
}
