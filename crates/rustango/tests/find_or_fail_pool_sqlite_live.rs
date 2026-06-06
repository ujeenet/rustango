#![cfg(feature = "sqlite")]
//! Live SQLite tests for the macro-emitted
//! `Model::find_or_fail_pool(pk, pool)` shortcut — Eloquent
//! `Model::findOrFail()` / Django `objects.get(pk=)` (raising)
//! parity.

use rustango::sql::{sqlx, Auto, ExecError, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "fof_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE fof_post (
            id    INTEGER PRIMARY KEY AUTOINCREMENT,
            title TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

#[tokio::test]
async fn find_or_fail_returns_ok_for_existing_pk() {
    let pool = make_pool().await;
    let mut p1 = Post {
        id: Auto::default(),
        title: "first".into(),
    };
    p1.save_pool(&pool).await.unwrap();
    let pk = p1.id.get().copied().unwrap();

    let row = Post::find_or_fail_pool(pk, &pool).await.unwrap();
    assert_eq!(row.title, "first");
}

#[tokio::test]
async fn find_or_fail_errors_for_missing_pk() {
    let pool = make_pool().await;
    let err = Post::find_or_fail_pool(9999_i64, &pool).await;
    match err {
        Err(ExecError::Driver(rustango::sql::sqlx::Error::RowNotFound)) => {} // expected
        other => panic!("expected RowNotFound, got: {other:?}"),
    }
}
