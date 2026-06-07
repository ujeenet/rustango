#![cfg(feature = "sqlite")]
//! Live SQLite tests for `ValuesFlatQuerySet::first` —
//! one-row-one-column shortcut. Eloquent `Builder::value()` parity.

use rustango::query::QuerySet;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "vff_post")]
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
        "CREATE TABLE vff_post (
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
    for (t, v) in [("alpha", 10), ("beta", 20), ("gamma", 30)] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
            views: v,
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn first_returns_first_cell_as_string() {
    let pool = make_pool().await;
    seed(&pool).await;
    // Order by PK ASC default → "alpha".
    let title: Option<String> = QuerySet::<Post>::default()
        .order_by(&[("id", false)])
        .values_list_flat("title")
        .first::<String>(&pool)
        .await
        .unwrap();
    assert_eq!(title.as_deref(), Some("alpha"));
}

#[tokio::test]
async fn first_returns_first_cell_as_integer() {
    let pool = make_pool().await;
    seed(&pool).await;
    let v: Option<i64> = QuerySet::<Post>::default()
        .order_by(&[("views", true)]) // DESC
        .values_list_flat("views")
        .first::<i64>(&pool)
        .await
        .unwrap();
    assert_eq!(v, Some(30));
}

#[tokio::test]
async fn first_returns_none_for_empty_match() {
    let pool = make_pool().await;
    seed(&pool).await;
    let title: Option<String> = QuerySet::<Post>::default()
        .filter("title", "nope")
        .values_list_flat("title")
        .first::<String>(&pool)
        .await
        .unwrap();
    assert!(title.is_none());
}
