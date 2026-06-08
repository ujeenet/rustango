#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet::paginate(page, per_page, &pool)
//! -> (Vec<T>, total)` — filtered counterpart of Model::paginate.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "qpag_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub status: String,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE qpag_post (
            id     INTEGER PRIMARY KEY AUTOINCREMENT,
            status TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool) {
    for i in 0..30 {
        let mut p = Post {
            id: Auto::default(),
            status: if i % 3 == 0 { "draft" } else { "published" }.into(),
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn paginate_on_filtered_queryset_uses_narrowed_total() {
    let pool = make_pool().await;
    seed(&pool).await;
    let (rows, total) = Post::objects()
        .filter("status", "published")
        .paginate(1, 10, &pool)
        .await
        .unwrap();
    // 30 posts; 1/3 are drafts, so 20 are published. Filtered total
    // is 20, NOT 30.
    assert_eq!(total, 20);
    assert_eq!(rows.len(), 10);
    for r in &rows {
        assert_eq!(r.status, "published");
    }
}

#[tokio::test]
async fn paginate_last_page_returns_partial_on_filtered() {
    let pool = make_pool().await;
    seed(&pool).await;
    let (rows, total) = Post::objects()
        .filter("status", "published")
        .paginate(2, 15, &pool)
        .await
        .unwrap();
    // 20 published. Page 2 of 15 = rows 15..20 = 5 rows.
    assert_eq!(total, 20);
    assert_eq!(rows.len(), 5);
}
