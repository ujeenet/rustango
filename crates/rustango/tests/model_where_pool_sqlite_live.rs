#![cfg(feature = "sqlite")]
//! Live SQLite tests for the macro-emitted
//! `Model::where_pool(col, val, pool)` shortcut — Eloquent
//! `Model::where($col, $val)->get()` / Django
//! `Model.objects.filter(col=val).all()` parity.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mwp_post")]
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
        "CREATE TABLE mwp_post (
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
        ("draft a", "draft"),
        ("draft b", "draft"),
        ("published a", "published"),
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
async fn where_pool_returns_matching_rows() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = Post::where_pool("status", "draft", &pool).await.unwrap();
    assert_eq!(rows.len(), 2);
    for r in &rows {
        assert_eq!(r.status, "draft");
    }
}

#[tokio::test]
async fn where_pool_returns_empty_for_no_match() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = Post::where_pool("status", "archived", &pool).await.unwrap();
    assert!(rows.is_empty());
}
