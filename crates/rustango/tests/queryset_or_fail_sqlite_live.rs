#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet::first_or_fail(&pool)` /
//! `QuerySet::find_or_fail(pk, &pool)` — Eloquent
//! `Builder::firstOrFail()` / `Builder::findOrFail($pk)` parity that
//! converts `None` to `sqlx::Error::RowNotFound` and honors the
//! queryset's accumulated scope.

use rustango::sql::{sqlx, Auto, ExecError, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "of_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 40)]
    pub title: String,
    pub published: bool,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE of_post (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            title     TEXT NOT NULL,
            published INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    p.into()
}

async fn seed(pool: &Pool) -> (i64, i64) {
    let mut pub_row = Post {
        id: Auto::default(),
        title: "published".into(),
        published: true,
    };
    pub_row.save_pool(pool).await.unwrap();
    let pub_id = *pub_row.id.get().unwrap();
    let mut draft_row = Post {
        id: Auto::default(),
        title: "draft".into(),
        published: false,
    };
    draft_row.save_pool(pool).await.unwrap();
    let draft_id = *draft_row.id.get().unwrap();
    (pub_id, draft_id)
}

fn is_row_not_found(err: &ExecError) -> bool {
    matches!(err, ExecError::Driver(sqlx::Error::RowNotFound))
}

#[tokio::test]
async fn first_or_fail_returns_row_when_match() {
    let pool = make_pool().await;
    seed(&pool).await;
    let row = Post::objects()
        .filter("published", true)
        .first_or_fail(&pool)
        .await
        .unwrap();
    assert_eq!(row.title, "published");
}

#[tokio::test]
async fn first_or_fail_errors_when_scope_is_empty() {
    let pool = make_pool().await;
    seed(&pool).await;
    let err = Post::objects()
        .filter("title", "nope".to_string())
        .first_or_fail(&pool)
        .await
        .unwrap_err();
    assert!(is_row_not_found(&err), "expected RowNotFound, got: {err}");
}

#[tokio::test]
async fn find_or_fail_returns_row_in_scope() {
    let pool = make_pool().await;
    let (pub_id, _draft_id) = seed(&pool).await;
    let row = Post::objects()
        .filter("published", true)
        .find_or_fail(pub_id, &pool)
        .await
        .unwrap();
    assert_eq!(row.title, "published");
}

#[tokio::test]
async fn find_or_fail_errors_when_pk_out_of_scope() {
    let pool = make_pool().await;
    let (_pub_id, draft_id) = seed(&pool).await;
    let err = Post::objects()
        .filter("published", true)
        .find_or_fail(draft_id, &pool)
        .await
        .unwrap_err();
    assert!(is_row_not_found(&err), "expected RowNotFound, got: {err}");
}

#[tokio::test]
async fn find_or_fail_errors_when_pk_missing() {
    let pool = make_pool().await;
    seed(&pool).await;
    let err = Post::objects()
        .find_or_fail(999_999_i64, &pool)
        .await
        .unwrap_err();
    assert!(is_row_not_found(&err), "expected RowNotFound, got: {err}");
}
