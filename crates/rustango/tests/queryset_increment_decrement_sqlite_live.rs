#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet::increment(col, by, &pool)` /
//! `QuerySet::decrement(col, by, &pool)` — Eloquent
//! `Builder::increment` / `decrement` parity. The QuerySet-filtered
//! variant complements `Model::increment_each` (table-wide) and
//! `model.increment` (single row) by letting the queryset's
//! accumulated WHERE narrow which rows get the bump.

use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "inc_post")]
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
        "CREATE TABLE inc_post (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            views     INTEGER NOT NULL DEFAULT 0,
            published INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    p.into()
}

async fn seed(pool: &Pool) -> (Vec<i64>, Vec<i64>) {
    let mut pub_ids = Vec::new();
    let mut draft_ids = Vec::new();
    for &published in &[true, false, true, false, true] {
        let mut row = Post {
            id: Auto::default(),
            views: 10,
            published,
        };
        row.save_pool(pool).await.unwrap();
        let pk = *row.id.get().unwrap();
        if published {
            pub_ids.push(pk);
        } else {
            draft_ids.push(pk);
        }
    }
    (pub_ids, draft_ids)
}

async fn views_for(pool: &Pool, id: i64) -> i64 {
    Post::objects()
        .filter("id", id)
        .fetch(pool)
        .await
        .unwrap()
        .pop()
        .unwrap()
        .views
}

#[tokio::test]
async fn increment_only_touches_filtered_rows() {
    let pool = make_pool().await;
    let (pub_ids, draft_ids) = seed(&pool).await;
    let n = Post::objects()
        .filter("published", true)
        .increment("views", 5, &pool)
        .await
        .unwrap();
    assert_eq!(n, pub_ids.len() as u64);
    for id in pub_ids {
        assert_eq!(views_for(&pool, id).await, 15);
    }
    for id in draft_ids {
        assert_eq!(views_for(&pool, id).await, 10);
    }
}

#[tokio::test]
async fn decrement_subtracts_on_filtered_rows() {
    let pool = make_pool().await;
    let (_pub_ids, draft_ids) = seed(&pool).await;
    let n = Post::objects()
        .filter("published", false)
        .decrement("views", 4, &pool)
        .await
        .unwrap();
    assert_eq!(n, draft_ids.len() as u64);
    for id in draft_ids {
        assert_eq!(views_for(&pool, id).await, 6);
    }
}

#[tokio::test]
async fn increment_with_no_match_returns_zero() {
    let pool = make_pool().await;
    seed(&pool).await;
    let n = Post::objects()
        .filter("views", 999_999_i64)
        .increment("views", 1, &pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn increment_unknown_column_errors() {
    let pool = make_pool().await;
    seed(&pool).await;
    let err = Post::objects()
        .increment("nope", 1, &pool)
        .await
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("nope"),
        "expected unknown-field error mentioning column, got: {msg}"
    );
}
