#![cfg(feature = "sqlite")]
//! Live SQLite test for `Model::find_many_or_fail(pks, &pool)` —
//! Eloquent `Model::findOrFail([1, 2, 3])` parity.

use rustango::sql::{sqlx, Auto, ExecError, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "fmf_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub title: String,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE fmf_post (
            id    INTEGER PRIMARY KEY AUTOINCREMENT,
            title TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool) -> Vec<i64> {
    let mut ids = Vec::new();
    for t in ["alpha", "beta", "gamma"] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
        };
        p.save_pool(pool).await.unwrap();
        ids.push(p.id.get().copied().unwrap());
    }
    ids
}

#[tokio::test]
async fn find_many_or_fail_returns_all_requested_rows() {
    let pool = make_pool().await;
    let ids = seed(&pool).await;
    let rows = Post::find_many_or_fail(ids.clone(), &pool).await.unwrap();
    assert_eq!(rows.len(), 3);
}

#[tokio::test]
async fn find_many_or_fail_errors_when_any_pk_missing() {
    let pool = make_pool().await;
    let ids = seed(&pool).await;
    // Add a non-existent PK; should fail.
    let mut request = ids;
    request.push(999_999);
    let err = Post::find_many_or_fail(request, &pool).await.unwrap_err();
    matches!(err, ExecError::Driver(sqlx::Error::RowNotFound));
}

#[tokio::test]
async fn find_many_or_fail_empty_input_returns_empty() {
    let pool = make_pool().await;
    let empty: Vec<i64> = Vec::new();
    let rows = Post::find_many_or_fail(empty, &pool).await.unwrap();
    assert!(rows.is_empty());
}

#[tokio::test]
async fn find_many_or_fail_dedups_duplicate_pks() {
    // Passing the same PK twice should not double-count the
    // expected row count.
    let pool = make_pool().await;
    let ids = seed(&pool).await;
    let dup_request = vec![ids[0], ids[0], ids[1]];
    let rows = Post::find_many_or_fail(dup_request, &pool).await.unwrap();
    // Two distinct PKs → two distinct rows back, no error.
    assert_eq!(rows.len(), 2);
}
