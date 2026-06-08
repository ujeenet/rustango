#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet::find(pk, &pool)` — Eloquent
//! `Builder::find($pk)` on a (possibly-scoped) queryset. Unlike
//! `Model::find` (table-wide lookup), this honors the queryset's
//! pre-applied filters.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "fnd_post")]
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
        "CREATE TABLE fnd_post (
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

#[tokio::test]
async fn find_returns_matching_pk_within_scope() {
    let pool = make_pool().await;
    let (pub_id, _draft_id) = seed(&pool).await;
    let row = Post::objects()
        .filter("published", true)
        .find(pub_id, &pool)
        .await
        .unwrap();
    assert!(row.is_some());
    assert_eq!(row.unwrap().title, "published");
}

#[tokio::test]
async fn find_returns_none_when_pk_is_outside_scope() {
    let pool = make_pool().await;
    let (_pub_id, draft_id) = seed(&pool).await;
    // PK exists but its row is not published — scoped find returns None.
    let row = Post::objects()
        .filter("published", true)
        .find(draft_id, &pool)
        .await
        .unwrap();
    assert!(row.is_none());
}

#[tokio::test]
async fn find_returns_none_when_pk_does_not_exist() {
    let pool = make_pool().await;
    seed(&pool).await;
    let row = Post::objects().find(999_999_i64, &pool).await.unwrap();
    assert!(row.is_none());
}
