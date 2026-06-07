#![cfg(feature = "sqlite")]
//! Live SQLite tests for the macro-emitted `Model::fresh(&pool)`
//! shortcut — Eloquent `Model::fresh()` parity. Non-mutating
//! counterpart of `refresh_from_db_pool`.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mfr_post")]
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
        "CREATE TABLE mfr_post (
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

#[tokio::test]
async fn fresh_pool_returns_fresh_instance_without_mutating_self() {
    let pool = make_pool().await;
    let mut p1 = Post {
        id: Auto::default(),
        title: "stale".into(),
        views: 1,
    };
    p1.save_pool(&pool).await.unwrap();
    let pk = p1.id.get().copied().unwrap();

    // External update.
    let Pool::Sqlite(raw) = &pool else {
        unreachable!()
    };
    sqlx::query("UPDATE mfr_post SET title = ?, views = ? WHERE id = ?")
        .bind("fresh-title")
        .bind(99_i64)
        .bind(pk)
        .execute(raw)
        .await
        .unwrap();

    let fresh = p1.fresh(&pool).await.unwrap().unwrap();
    assert_eq!(fresh.title, "fresh-title");
    assert_eq!(fresh.views, 99);

    // p1 itself is NOT mutated — that's the contract vs refresh_from_db.
    assert_eq!(p1.title, "stale");
    assert_eq!(p1.views, 1);
}

#[tokio::test]
async fn fresh_pool_returns_none_when_row_deleted() {
    let pool = make_pool().await;
    let mut p = Post {
        id: Auto::default(),
        title: "doomed".into(),
        views: 0,
    };
    p.save_pool(&pool).await.unwrap();
    let pk = p.id.get().copied().unwrap();

    // Delete the row out from under us.
    let Pool::Sqlite(raw) = &pool else {
        unreachable!()
    };
    sqlx::query("DELETE FROM mfr_post WHERE id = ?")
        .bind(pk)
        .execute(raw)
        .await
        .unwrap();

    let res = p.fresh(&pool).await.unwrap();
    assert!(res.is_none(), "deleted row → None (no RowNotFound error)");

    // p itself is unchanged.
    assert_eq!(p.title, "doomed");
}
