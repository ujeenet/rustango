#![cfg(feature = "sqlite")]
//! Live SQLite tests for the macro-emitted comparison + pagination
//! shortcuts: `where_gt_pool` / `where_gte_pool` / `where_lt_pool` /
//! `where_lte_pool` / `where_ne_pool` / `take_pool` /
//! `for_page_pool`. Eloquent `where(col, ">", val)` / `take(n)` /
//! `forPage(p, pp)` parity.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mct_post")]
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
        "CREATE TABLE mct_post (
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
    for (t, v) in [("a", 10_i64), ("b", 20), ("c", 30), ("d", 40), ("e", 50)] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
            views: v,
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn where_gt_pool_filters_strict_greater() {
    let pool = make_pool().await;
    seed(&pool).await;
    let r = Post::where_gt("views", 30_i64, &pool).await.unwrap();
    assert_eq!(r.len(), 2);
}

#[tokio::test]
async fn where_gte_pool_filters_inclusive() {
    let pool = make_pool().await;
    seed(&pool).await;
    let r = Post::where_gte("views", 30_i64, &pool).await.unwrap();
    assert_eq!(r.len(), 3);
}

#[tokio::test]
async fn where_lt_pool_filters_strict_less() {
    let pool = make_pool().await;
    seed(&pool).await;
    let r = Post::where_lt("views", 30_i64, &pool).await.unwrap();
    assert_eq!(r.len(), 2);
}

#[tokio::test]
async fn where_lte_pool_filters_inclusive() {
    let pool = make_pool().await;
    seed(&pool).await;
    let r = Post::where_lte("views", 30_i64, &pool).await.unwrap();
    assert_eq!(r.len(), 3);
}

#[tokio::test]
async fn where_ne_pool_excludes_value() {
    let pool = make_pool().await;
    seed(&pool).await;
    let r = Post::where_ne("views", 30_i64, &pool).await.unwrap();
    assert_eq!(r.len(), 4);
}

#[tokio::test]
async fn take_pool_caps_at_n() {
    let pool = make_pool().await;
    seed(&pool).await;
    let r = Post::take(2, &pool).await.unwrap();
    assert_eq!(r.len(), 2);
}

#[tokio::test]
async fn for_page_pool_pages_correctly() {
    let pool = make_pool().await;
    seed(&pool).await;
    let p1 = Post::for_page(1, 2, &pool).await.unwrap();
    let p2 = Post::for_page(2, 2, &pool).await.unwrap();
    let p3 = Post::for_page(3, 2, &pool).await.unwrap();
    assert_eq!(p1.len(), 2);
    assert_eq!(p2.len(), 2);
    assert_eq!(p3.len(), 1);
    // No overlap between pages — by PK ordering insertion order is preserved.
    let p1_ids: Vec<i64> = p1.iter().map(|r| *r.id.get().unwrap()).collect();
    let p2_ids: Vec<i64> = p2.iter().map(|r| *r.id.get().unwrap()).collect();
    assert!(p1_ids.iter().all(|id| !p2_ids.contains(id)));
}
