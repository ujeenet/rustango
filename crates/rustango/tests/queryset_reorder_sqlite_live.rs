#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet::reorder(&[(col, asc)])` —
//! Eloquent `Builder::reorder` parity. Verifies the new ordering
//! REPLACES any prior `order_by` keys instead of appending.

use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "ro_post", default_order = "title")]
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
        "CREATE TABLE ro_post (
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
    for (t, v) in [("c", 30), ("a", 10), ("b", 20)] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
            views: v,
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn order_by_appends_secondary_sort_key() {
    // Sanity check — `order_by` keeps prior sort keys.
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = Post::objects()
        .order_by(&[("title", false)])
        .order_by(&[("views", false)])
        .fetch(&pool)
        .await
        .unwrap();
    let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
    // title ASC then views ASC (tiebreaker) — alphabetical wins.
    assert_eq!(titles, vec!["a", "b", "c"]);
}

#[tokio::test]
async fn reorder_replaces_prior_order_by_keys() {
    // The headline: `reorder` wipes the inherited sort key.
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = Post::objects()
        .with_default_order() // inherits `title ASC` from schema
        .reorder(&[("views", true)]) // replaces with views DESC
        .fetch(&pool)
        .await
        .unwrap();
    let views: Vec<i64> = rows.iter().map(|r| r.views).collect();
    assert_eq!(views, vec![30, 20, 10]);
}

#[tokio::test]
async fn reorder_with_empty_slice_clears_sort() {
    // `.reorder(&[])` matches Eloquent `reorder()` with no args —
    // clears every sort key.
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = Post::objects()
        .order_by(&[("title", true)])
        .reorder(&[])
        .order_by(&[("id", false)])
        .fetch(&pool)
        .await
        .unwrap();
    // After reorder(&[]) the prior title-DESC is gone; only id-ASC
    // survives, so insertion order returns: c (id=1), a (id=2),
    // b (id=3).
    let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(titles, vec!["c", "a", "b"]);
}
