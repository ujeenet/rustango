#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet::skip(n)` / `QuerySet::take(n)` —
//! Eloquent aliases for `offset(n)` / `limit(n)`.

use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "st_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub n: i64,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE st_post (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            n  INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool, n: i64) {
    for i in 0..n {
        let mut p = Post {
            id: Auto::default(),
            n: i,
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn skip_take_chains_correctly() {
    let pool = make_pool().await;
    seed(&pool, 20).await;
    // Eloquent: ->skip(5)->take(3) gives rows 5..8 (0-indexed).
    let rows = Post::objects()
        .order_by(&[("n", false)])
        .skip(5)
        .take(3)
        .fetch_pool(&pool)
        .await
        .unwrap();
    let ns: Vec<i64> = rows.iter().map(|r| r.n).collect();
    assert_eq!(ns, vec![5, 6, 7]);
}

#[tokio::test]
async fn take_alone_caps_result() {
    let pool = make_pool().await;
    seed(&pool, 10).await;
    let rows = Post::objects()
        .order_by(&[("n", false)])
        .take(3)
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
}

#[tokio::test]
async fn skip_alone_with_limit_returns_remainder() {
    // SQLite silently ignores OFFSET without LIMIT (and PG/MySQL
    // semantics differ); the Eloquent shape is `->skip(n)->take(m)`
    // so we test that combination.
    let pool = make_pool().await;
    seed(&pool, 10).await;
    let rows = Post::objects()
        .order_by(&[("n", false)])
        .skip(7)
        .take(100)
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
}
