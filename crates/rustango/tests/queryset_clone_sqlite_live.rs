#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet<T>: Clone` — Eloquent
//! `Builder::clone()` parity. Verifies a half-built queryset can
//! be reused as a base for divergent branches.

use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "qc_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub title: String,
    pub status: String,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE qc_post (
            id     INTEGER PRIMARY KEY AUTOINCREMENT,
            title  TEXT NOT NULL,
            status TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool) {
    for (t, s) in [
        ("a", "draft"),
        ("b", "draft"),
        ("c", "published"),
        ("d", "published"),
        ("e", "archived"),
    ] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
            status: s.into(),
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn cloning_a_queryset_lets_branches_diverge() {
    let pool = make_pool().await;
    seed(&pool).await;

    // Base queryset — "not archived".
    let base = Post::objects().filter("status__ne", "archived");

    // Two branches that diverge from the same base — each adds its
    // own additional filter without affecting the other.
    let drafts = base.clone().filter("status", "draft");
    let pub_only = base.filter("status", "published");

    let drafts_rows = drafts.fetch(&pool).await.unwrap();
    let pub_rows = pub_only.fetch(&pool).await.unwrap();

    assert_eq!(drafts_rows.len(), 2);
    assert!(drafts_rows.iter().all(|r| r.status == "draft"));
    assert_eq!(pub_rows.len(), 2);
    assert!(pub_rows.iter().all(|r| r.status == "published"));
}

#[tokio::test]
async fn clone_does_not_share_pending_state() {
    // After cloning, mutating one queryset must not affect the
    // other's pending filter list.
    let pool = make_pool().await;
    seed(&pool).await;

    let original = Post::objects();
    let with_filter = original.clone().filter("status", "draft");

    // Original is still unfiltered → returns all 5.
    let all = original.fetch(&pool).await.unwrap();
    assert_eq!(all.len(), 5);

    // Cloned-then-filtered branch → returns only the 2 drafts.
    let drafts = with_filter.fetch(&pool).await.unwrap();
    assert_eq!(drafts.len(), 2);
}
