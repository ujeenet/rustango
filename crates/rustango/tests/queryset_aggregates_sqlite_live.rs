#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet::sum / avg / min / max` —
//! Eloquent aggregate builders on a filtered queryset. Differ
//! from `Model::sum / avg / min / max` (table-wide) in that the
//! queryset's accumulated filters narrow the aggregate's input.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "qa_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub views: i64,
    pub published: bool,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE qa_post (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            views     INTEGER NOT NULL,
            published INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool) {
    for (v, pub_) in [
        (10_i64, true),
        (20, true),
        (30, true),
        (1000, false), // outlier — only present in unfiltered queries
    ] {
        let mut p = Post {
            id: Auto::default(),
            views: v,
            published: pub_,
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn sum_on_filtered_queryset_excludes_unmatched_rows() {
    let pool = make_pool().await;
    seed(&pool).await;
    let s: Option<i64> = Post::objects()
        .filter("published", true)
        .sum::<i64>("views", &pool)
        .await
        .unwrap();
    // 10 + 20 + 30 = 60. The 1000-view outlier is excluded.
    assert_eq!(s, Some(60));
}

#[tokio::test]
async fn min_max_avg_on_filtered_queryset() {
    let pool = make_pool().await;
    seed(&pool).await;
    let min: Option<i64> = Post::objects()
        .filter("published", true)
        .min::<i64>("views", &pool)
        .await
        .unwrap();
    let max: Option<i64> = Post::objects()
        .filter("published", true)
        .max::<i64>("views", &pool)
        .await
        .unwrap();
    let avg: Option<f64> = Post::objects()
        .filter("published", true)
        .avg::<f64>("views", &pool)
        .await
        .unwrap();
    assert_eq!(min, Some(10));
    assert_eq!(max, Some(30));
    assert!((avg.unwrap() - 20.0).abs() < 0.001);
}

#[tokio::test]
async fn sum_on_empty_filtered_set_returns_none() {
    let pool = make_pool().await;
    seed(&pool).await;
    let s: Option<i64> = Post::objects()
        .filter("views__gt", 9999_i64)
        .sum::<i64>("views", &pool)
        .await
        .unwrap();
    assert_eq!(s, None);
}
