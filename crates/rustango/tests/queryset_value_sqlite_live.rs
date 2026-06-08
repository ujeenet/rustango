#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet::value<U>(col, &pool)` — Eloquent
//! `Builder::value($col)` parity.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "qv_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub title: String,
    pub views: i64,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE qv_post (
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
    for (t, v) in [("alpha", 10_i64), ("beta", 20), ("gamma", 30)] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
            views: v,
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn value_returns_first_row_column_on_filtered_queryset() {
    let pool = make_pool().await;
    seed(&pool).await;
    let title: Option<String> = Post::objects()
        .filter("views", 20_i64)
        .value::<String>("title", &pool)
        .await
        .unwrap();
    assert_eq!(title, Some("beta".to_string()));
}

#[tokio::test]
async fn value_returns_none_on_empty_queryset() {
    let pool = make_pool().await;
    seed(&pool).await;
    let v: Option<i64> = Post::objects()
        .filter("views__gt", 9999_i64)
        .value::<i64>("views", &pool)
        .await
        .unwrap();
    assert_eq!(v, None);
}

#[tokio::test]
async fn value_honors_order_by_for_first_row_selection() {
    let pool = make_pool().await;
    seed(&pool).await;
    let highest: Option<i64> = Post::objects()
        .order_by(&[("views", true)])
        .value::<i64>("views", &pool)
        .await
        .unwrap();
    assert_eq!(highest, Some(30));
}
