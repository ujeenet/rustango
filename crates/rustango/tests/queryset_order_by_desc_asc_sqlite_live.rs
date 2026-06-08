#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet::order_by_desc(col)` /
//! `QuerySet::order_by_asc(col)` — single-column ORDER BY shortcuts
//! (chainable Eloquent `Builder::latest(col)` / `Builder::oldest(col)`
//! shape; rustango's `latest` / `earliest` are async fetchers, so the
//! chainable form lives under the explicit names).

use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "lo_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub seq: i64,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE lo_post (
            id  INTEGER PRIMARY KEY AUTOINCREMENT,
            seq INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    p.into()
}

async fn seed(pool: &Pool, n: i64) {
    for i in 0..n {
        let mut row = Post {
            id: Auto::default(),
            seq: i,
        };
        row.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn order_by_desc_appends_descending_order() {
    let pool = make_pool().await;
    seed(&pool, 5).await;
    let rows = Post::objects()
        .order_by_desc("seq")
        .fetch_pool(&pool)
        .await
        .unwrap();
    let ns: Vec<i64> = rows.iter().map(|r| r.seq).collect();
    assert_eq!(ns, vec![4, 3, 2, 1, 0]);
}

#[tokio::test]
async fn order_by_asc_appends_ascending_order() {
    let pool = make_pool().await;
    seed(&pool, 5).await;
    let rows = Post::objects()
        .order_by_asc("seq")
        .fetch_pool(&pool)
        .await
        .unwrap();
    let ns: Vec<i64> = rows.iter().map(|r| r.seq).collect();
    assert_eq!(ns, vec![0, 1, 2, 3, 4]);
}
