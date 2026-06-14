#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet::in_random_order()` — Eloquent
//! `Builder::inRandomOrder()` alias for `order_random()`.

use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "ir_post")]
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
        "CREATE TABLE ir_post (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            n  INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    p.into()
}

#[tokio::test]
async fn in_random_order_returns_every_row() {
    let pool = make_pool().await;
    for i in 0..10 {
        let mut p = Post {
            id: Auto::default(),
            n: i,
        };
        p.save_pool(&pool).await.unwrap();
    }
    let rows = Post::objects()
        .in_random_order()
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 10);
    let mut ns: Vec<i64> = rows.iter().map(|r| r.n).collect();
    ns.sort_unstable();
    assert_eq!(ns, (0..10).collect::<Vec<_>>());
}

#[tokio::test]
async fn in_random_order_with_take_caps_result() {
    let pool = make_pool().await;
    for i in 0..20 {
        let mut p = Post {
            id: Auto::default(),
            n: i,
        };
        p.save_pool(&pool).await.unwrap();
    }
    let rows = Post::objects()
        .in_random_order()
        .take(5)
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 5);
}
