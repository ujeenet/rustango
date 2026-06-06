#![cfg(feature = "sqlite")]
//! Live SQLite tests for the macro-emitted
//! `Model::find_many_pool(pks, pool)` shortcut — Eloquent
//! `Model::find([1, 2, 3])` (list arg) / Django
//! `Model.objects.filter(pk__in=[...])` parity.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mfm_post")]
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
        "CREATE TABLE mfm_post (
            id    INTEGER PRIMARY KEY AUTOINCREMENT,
            title TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed_five(pool: &Pool) -> Vec<i64> {
    let mut pks = Vec::new();
    for t in ["a", "b", "c", "d", "e"] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
        };
        p.save_pool(pool).await.unwrap();
        pks.push(p.id.get().copied().unwrap());
    }
    pks
}

#[tokio::test]
async fn find_many_pool_returns_listed_rows() {
    let pool = make_pool().await;
    let pks = seed_five(&pool).await;
    let want = vec![pks[0], pks[2], pks[4]];
    let rows = Post::find_many_pool(want, &pool).await.unwrap();
    assert_eq!(rows.len(), 3);
    let titles: std::collections::HashSet<&str> = rows.iter().map(|p| p.title.as_str()).collect();
    assert!(titles.contains("a"));
    assert!(titles.contains("c"));
    assert!(titles.contains("e"));
}

#[tokio::test]
async fn find_many_pool_skips_missing_pks() {
    let pool = make_pool().await;
    let pks = seed_five(&pool).await;
    let rows = Post::find_many_pool(vec![pks[0], 99999_i64, pks[1]], &pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2, "missing PK silently dropped");
}

#[tokio::test]
async fn find_many_pool_empty_input_returns_empty_vec() {
    let pool = make_pool().await;
    seed_five(&pool).await;
    let rows = Post::find_many_pool(Vec::<i64>::new(), &pool)
        .await
        .unwrap();
    assert!(rows.is_empty());
}
