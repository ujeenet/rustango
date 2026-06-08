#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet::pks::<K>(&pool)` — Eloquent
//! `Collection::modelKeys()` / `$query->pluck($model->getKeyName())`
//! shortcut that plucks the model's PK column without spelling it.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "pk_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub published: bool,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE pk_post (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            published INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    p.into()
}

async fn seed(pool: &Pool) -> (Vec<i64>, Vec<i64>) {
    let mut pubs = Vec::new();
    let mut drafts = Vec::new();
    for &published in &[true, false, true, true, false] {
        let mut row = Post {
            id: Auto::default(),
            published,
        };
        row.save_pool(pool).await.unwrap();
        let pk = *row.id.get().unwrap();
        if published {
            pubs.push(pk);
        } else {
            drafts.push(pk);
        }
    }
    (pubs, drafts)
}

#[tokio::test]
async fn pks_returns_filtered_primary_keys() {
    let pool = make_pool().await;
    let (pubs, _drafts) = seed(&pool).await;
    let mut got: Vec<i64> = Post::objects()
        .filter("published", true)
        .pks::<i64>(&pool)
        .await
        .unwrap();
    got.sort_unstable();
    let mut expected = pubs.clone();
    expected.sort_unstable();
    assert_eq!(got, expected);
}

#[tokio::test]
async fn pks_on_empty_queryset_returns_empty_vec() {
    let pool = make_pool().await;
    seed(&pool).await;
    let got: Vec<i64> = Post::objects()
        .filter("id", 999_999_i64)
        .pks::<i64>(&pool)
        .await
        .unwrap();
    assert!(got.is_empty());
}

#[tokio::test]
async fn pks_unfiltered_returns_every_row() {
    let pool = make_pool().await;
    let (pubs, drafts) = seed(&pool).await;
    let mut got: Vec<i64> = Post::objects().pks::<i64>(&pool).await.unwrap();
    got.sort_unstable();
    let mut expected: Vec<i64> = pubs.into_iter().chain(drafts).collect();
    expected.sort_unstable();
    assert_eq!(got, expected);
}
