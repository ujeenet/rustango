#![cfg(feature = "sqlite")]
//! Live SQLite tests for the macro-emitted
//! `Model::first_where_pool(col, val, pool)` shortcut — Eloquent
//! `Model::firstWhere($col, $val)` / Django
//! `Model.objects.filter(col=val).first()` parity.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mfw_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 80)]
    pub status: String,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE mfw_post (
            id     INTEGER PRIMARY KEY AUTOINCREMENT,
            title  TEXT NOT NULL,
            status TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool) {
    for (title, status) in [
        ("draft post", "draft"),
        ("published a", "published"),
        ("published b", "published"),
    ] {
        let mut p = Post {
            id: Auto::default(),
            title: title.into(),
            status: status.into(),
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn first_where_pool_returns_some_for_match() {
    let pool = make_pool().await;
    seed(&pool).await;
    let row = Post::first_where_pool("status", "draft", &pool)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.title, "draft post");
}

#[tokio::test]
async fn first_where_pool_picks_first_of_multiple_matches() {
    let pool = make_pool().await;
    seed(&pool).await;
    // Two rows with status="published"; first() defaults to PK ASC.
    let row = Post::first_where_pool("status", "published", &pool)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.title, "published a", "PK-ASC tiebreak");
}

#[tokio::test]
async fn first_where_pool_returns_none_for_no_match() {
    let pool = make_pool().await;
    seed(&pool).await;
    let row = Post::first_where_pool("status", "archived", &pool)
        .await
        .unwrap();
    assert!(row.is_none());
}
