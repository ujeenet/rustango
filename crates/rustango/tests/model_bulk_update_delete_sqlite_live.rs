#![cfg(feature = "sqlite")]
//! Live SQLite tests for the macro-emitted bulk-update + bulk-delete
//! shortcuts: `update_where_pool` / `delete_where_pool` /
//! `update_all_pool`. Eloquent
//! `Model::where($col, $val)->update([$col2 => $val2])` /
//! `Model::where($col, $val)->delete()` /
//! `Model::query()->update([$col => $val])` parity.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mbu_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 80)]
    pub status: String,
    pub views: i64,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE mbu_post (
            id     INTEGER PRIMARY KEY AUTOINCREMENT,
            title  TEXT NOT NULL,
            status TEXT NOT NULL,
            views  INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool) {
    for (t, s, v) in [
        ("a", "draft", 10_i64),
        ("b", "draft", 20),
        ("c", "published", 30),
        ("d", "published", 40),
    ] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
            status: s.into(),
            views: v,
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn update_where_pool_updates_matching_rows() {
    let pool = make_pool().await;
    seed(&pool).await;
    let n = Post::update_where("status", "draft", "status", "archived", &pool)
        .await
        .unwrap();
    assert_eq!(n, 2);
    let drafts = Post::where_("status", "draft", &pool).await.unwrap();
    assert_eq!(drafts.len(), 0);
    let archived = Post::where_("status", "archived", &pool).await.unwrap();
    assert_eq!(archived.len(), 2);
}

#[tokio::test]
async fn update_where_pool_returns_zero_when_no_match() {
    let pool = make_pool().await;
    seed(&pool).await;
    let n = Post::update_where("status", "nope", "status", "x", &pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn delete_where_pool_removes_matching_rows() {
    let pool = make_pool().await;
    seed(&pool).await;
    let n = Post::delete_where("status", "draft", &pool).await.unwrap();
    assert_eq!(n, 2);
    let remaining = Post::all(&pool).await.unwrap();
    assert_eq!(remaining.len(), 2);
    for r in &remaining {
        assert_eq!(r.status, "published");
    }
}

#[tokio::test]
async fn delete_where_pool_errors_on_unknown_field() {
    let pool = make_pool().await;
    seed(&pool).await;
    let err = Post::delete_where("nope", "x", &pool).await.unwrap_err();
    assert!(err.to_string().contains("nope"));
}

#[tokio::test]
async fn update_all_pool_updates_every_row() {
    let pool = make_pool().await;
    seed(&pool).await;
    let n = Post::update_all("status", "frozen", &pool).await.unwrap();
    assert_eq!(n, 4);
    let frozen = Post::where_("status", "frozen", &pool).await.unwrap();
    assert_eq!(frozen.len(), 4);
}
