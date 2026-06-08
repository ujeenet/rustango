#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet::pluck<U>(col, &pool)` —
//! Eloquent `Builder::pluck($col)` parity on filtered querysets.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "qp_post")]
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
        "CREATE TABLE qp_post (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            title     TEXT NOT NULL,
            published INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool) {
    for (t, pub_) in [
        ("alpha", true),
        ("beta", false),
        ("gamma", true),
        ("delta", true),
    ] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
            published: pub_,
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn pluck_on_filtered_queryset_returns_only_matching_column() {
    let pool = make_pool().await;
    seed(&pool).await;
    let mut titles: Vec<String> = Post::objects()
        .filter("published", true)
        .pluck::<String>("title", &pool)
        .await
        .unwrap();
    titles.sort();
    assert_eq!(titles, vec!["alpha", "delta", "gamma"]);
}

#[tokio::test]
async fn pluck_decodes_into_any_scalar_type() {
    let pool = make_pool().await;
    seed(&pool).await;
    // i64 column round-trip.
    let mut ids: Vec<i64> = Post::objects()
        .filter("published", true)
        .pluck::<i64>("id", &pool)
        .await
        .unwrap();
    ids.sort();
    assert_eq!(ids.len(), 3);
}

#[tokio::test]
async fn pluck_on_empty_queryset_returns_empty_vec() {
    let pool = make_pool().await;
    seed(&pool).await;
    let titles: Vec<String> = Post::objects()
        .filter("published", false)
        .filter("title", "alpha")
        .pluck::<String>("title", &pool)
        .await
        .unwrap();
    assert!(titles.is_empty());
}
