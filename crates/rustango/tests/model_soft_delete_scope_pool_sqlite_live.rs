#![cfg(feature = "sqlite")]
//! Live SQLite tests for the macro-emitted soft-delete query
//! shortcuts — `Model::active_pool`, `Model::only_trashed_pool`,
//! `Model::with_trashed_pool`. Eloquent
//! `Model::onlyTrashed()->get()` / `Model::withTrashed()->get()`
//! parity. Closes #821 partial.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "sds_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(soft_delete)]
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE sds_post (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            title      TEXT NOT NULL,
            deleted_at TEXT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool) -> (i64, i64, i64) {
    let mut a = Post {
        id: Auto::default(),
        title: "alpha".into(),
        deleted_at: None,
    };
    a.save_pool(pool).await.unwrap();
    let mut b = Post {
        id: Auto::default(),
        title: "beta".into(),
        deleted_at: None,
    };
    b.save_pool(pool).await.unwrap();
    let mut c = Post {
        id: Auto::default(),
        title: "gamma".into(),
        deleted_at: None,
    };
    c.save_pool(pool).await.unwrap();
    // Soft-delete beta.
    b.soft_delete(pool).await.unwrap();
    (
        a.id.get().copied().unwrap(),
        b.id.get().copied().unwrap(),
        c.id.get().copied().unwrap(),
    )
}

#[tokio::test]
async fn active_pool_returns_only_live_rows() {
    let pool = make_pool().await;
    let (a_id, _b_id, c_id) = seed(&pool).await;
    let mut rows = Post::active(&pool).await.unwrap();
    rows.sort_by_key(|r| r.id.get().copied().unwrap());
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id.get().copied().unwrap(), a_id);
    assert_eq!(rows[1].id.get().copied().unwrap(), c_id);
    for r in &rows {
        assert!(r.deleted_at.is_none());
    }
}

#[tokio::test]
async fn only_trashed_pool_returns_only_soft_deleted_rows() {
    let pool = make_pool().await;
    let (_a_id, b_id, _c_id) = seed(&pool).await;
    let rows = Post::only_trashed(&pool).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id.get().copied().unwrap(), b_id);
    assert!(rows[0].deleted_at.is_some());
}

#[tokio::test]
async fn with_trashed_pool_returns_every_row() {
    let pool = make_pool().await;
    let _ids = seed(&pool).await;
    let rows = Post::with_trashed(&pool).await.unwrap();
    assert_eq!(rows.len(), 3);
    let trashed = rows.iter().filter(|r| r.deleted_at.is_some()).count();
    assert_eq!(trashed, 1);
}
