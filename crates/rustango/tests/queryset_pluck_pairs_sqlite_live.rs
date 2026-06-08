#![cfg(feature = "sqlite")]
//! Live SQLite test for
//! `QuerySet::pluck_pairs::<K, V>(key_col, value_col, &pool)` —
//! Eloquent `Builder::pluck($value, $key)` parity. Returns
//! `Vec<(K, V)>` so the caller can collect into any map shape.

use std::collections::BTreeMap;

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "pp_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub title: String,
    pub published: bool,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE pp_post (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            title     TEXT NOT NULL,
            published INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    p.into()
}

async fn seed(pool: &Pool) -> Vec<(i64, String)> {
    let mut out = Vec::new();
    for (title, published) in [("a", true), ("b", false), ("c", true), ("d", true)] {
        let mut row = Post {
            id: Auto::default(),
            title: title.into(),
            published,
        };
        row.save_pool(pool).await.unwrap();
        if published {
            out.push((*row.id.get().unwrap(), title.to_string()));
        }
    }
    out.sort_by_key(|(k, _)| *k);
    out
}

#[tokio::test]
async fn pluck_pairs_projects_two_columns_for_filtered_queryset() {
    let pool = make_pool().await;
    let expected = seed(&pool).await;
    let mut got: Vec<(i64, String)> = Post::objects()
        .filter("published", true)
        .pluck_pairs::<i64, String>("id", "title", &pool)
        .await
        .unwrap();
    got.sort_by_key(|(k, _)| *k);
    assert_eq!(got, expected);
}

#[tokio::test]
async fn pluck_pairs_collects_into_btreemap() {
    let pool = make_pool().await;
    seed(&pool).await;
    let map: BTreeMap<i64, String> = Post::objects()
        .filter("published", true)
        .pluck_pairs::<i64, String>("id", "title", &pool)
        .await
        .unwrap()
        .into_iter()
        .collect();
    // Every value should be a published row's title.
    for (_id, title) in &map {
        assert!(matches!(title.as_str(), "a" | "c" | "d"));
    }
    assert_eq!(map.len(), 3);
}

#[tokio::test]
async fn pluck_pairs_on_empty_queryset_returns_empty() {
    let pool = make_pool().await;
    seed(&pool).await;
    let got: Vec<(i64, String)> = Post::objects()
        .filter("id", 999_999_i64)
        .pluck_pairs::<i64, String>("id", "title", &pool)
        .await
        .unwrap();
    assert!(got.is_empty());
}
