//! Live SQLite test for `QuerySet::exclude` (Django `.exclude()`,
//! #1030). Covers the filter/exclude inverse pair, `__lookup` suffixes
//! through exclude, chained-excludes-AND, and the NULL-row semantics of
//! `NOT (col = v)`.

#![cfg(all(feature = "sqlite", feature = "tenancy"))]

use rustango::core::Model as _;
use rustango::sql::{raw_execute_pool, sqlx, FetcherPool as _, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "qex_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 20)]
    pub status: String,
    pub views: i64,
    #[rustango(max_length = 20)]
    pub category: Option<String>,
}

async fn seeded() -> Pool {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite");
    raw_execute_pool(
        &Pool::Sqlite(sq.clone()),
        "CREATE TABLE qex_post (id INTEGER PRIMARY KEY AUTOINCREMENT, \
         status TEXT NOT NULL, views INTEGER NOT NULL, category TEXT)",
        vec![],
    )
    .await
    .expect("create");
    let pool = Pool::Sqlite(sq);
    for sql in [
        "INSERT INTO qex_post (id, status, views, category) VALUES (1, 'draft', 10, 'a')",
        "INSERT INTO qex_post (id, status, views, category) VALUES (2, 'published', 200, 'b')",
        "INSERT INTO qex_post (id, status, views, category) VALUES (3, 'archived', 50, NULL)",
        "INSERT INTO qex_post (id, status, views, category) VALUES (4, 'published', 100, 'a')",
    ] {
        raw_execute_pool(&pool, sql, vec![]).await.expect("seed");
    }
    pool
}

async fn ids(pool: &Pool, qs: rustango::query::QuerySet<Post>) -> Vec<i64> {
    let mut v: Vec<i64> = qs
        .fetch(pool)
        .await
        .expect("fetch")
        .into_iter()
        .map(|p| p.id)
        .collect();
    v.sort_unstable();
    v
}

#[tokio::test]
async fn exclude_is_the_inverse_of_filter() {
    let pool = seeded().await;

    let drafts = ids(&pool, Post::objects().filter("status", "draft")).await;
    assert_eq!(drafts, vec![1]);

    let non_drafts = ids(&pool, Post::objects().exclude("status", "draft")).await;
    assert_eq!(non_drafts, vec![2, 3, 4]);

    // filter ∪ exclude = every row (status is NOT NULL here).
    let mut union = drafts;
    union.extend(non_drafts);
    union.sort_unstable();
    assert_eq!(union, vec![1, 2, 3, 4]);
}

#[tokio::test]
async fn exclude_supports_lookup_suffixes() {
    let pool = seeded().await;
    // NOT (views < 100) → views >= 100 → ids 2 (200) + 4 (100).
    let got = ids(&pool, Post::objects().exclude("views__lt", 100_i64)).await;
    assert_eq!(got, vec![2, 4]);
}

#[tokio::test]
async fn chained_excludes_and_together() {
    let pool = seeded().await;
    // NOT draft AND NOT archived → published rows.
    let got = ids(
        &pool,
        Post::objects()
            .exclude("status", "draft")
            .exclude("status", "archived"),
    )
    .await;
    assert_eq!(got, vec![2, 4]);
}

#[tokio::test]
async fn exclude_drops_null_rows() {
    let pool = seeded().await;
    // NOT (category = 'a') excludes the 'a' rows (1, 4) AND the NULL-row
    // (3) — `NOT (NULL = 'a')` is NULL, which fails the WHERE. Matches
    // Django's emission. Only the 'b' row (2) survives.
    let got = ids(&pool, Post::objects().exclude("category", "a")).await;
    assert_eq!(got, vec![2], "NULL category row excluded by NOT(=)");
}
