#![cfg(feature = "sqlite")]
//! Live SQLite tests for `Model::destroy(pks, pool)` —
//! Eloquent `Model::destroy([...])` / Django
//! `Model.objects.filter(pk__in=[...]).delete()` parity.

use rustango::query::QuerySet;
use rustango::sql::{sqlx, Auto, FetcherPool, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mdp_post")]
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
        "CREATE TABLE mdp_post (
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
async fn destroy_pool_deletes_listed_rows() {
    let pool = make_pool().await;
    let pks = seed_five(&pool).await;
    let to_delete = vec![pks[0], pks[2], pks[4]]; // 3 rows
    let n = Post::destroy(to_delete, &pool).await.unwrap();
    assert_eq!(n, 3);

    let remaining: Vec<Post> = QuerySet::<Post>::default().fetch_pool(&pool).await.unwrap();
    assert_eq!(remaining.len(), 2);
    let titles: Vec<&str> = remaining.iter().map(|p| p.title.as_str()).collect();
    assert!(titles.contains(&"b"));
    assert!(titles.contains(&"d"));
}

#[tokio::test]
async fn destroy_pool_empty_list_is_noop() {
    let pool = make_pool().await;
    seed_five(&pool).await;
    let n = Post::destroy(Vec::<i64>::new(), &pool).await.unwrap();
    assert_eq!(n, 0);

    let remaining: Vec<Post> = QuerySet::<Post>::default().fetch_pool(&pool).await.unwrap();
    assert_eq!(remaining.len(), 5);
}

#[tokio::test]
async fn destroy_pool_missing_pk_returns_count_of_actually_deleted() {
    let pool = make_pool().await;
    let pks = seed_five(&pool).await;
    let n = Post::destroy(vec![pks[0], 99999_i64], &pool).await.unwrap();
    assert_eq!(n, 1, "only the real row matched");
}
