#![cfg(feature = "sqlite")]
//! Smoke tests for the bare-name Eloquent aliases. Confirms that
//! every common shortcut now works WITHOUT the `_pool` suffix —
//! `Post::find(id, &pool)` instead of `Post::find_pool(id, &pool)`,
//! `post.increment("views", 1, &pool)` instead of
//! `post.increment_pool(...)`, etc.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mba_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 80)]
    pub status: String,
    pub views: i64,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE mba_post (
            id     INTEGER PRIMARY KEY AUTOINCREMENT,
            title  TEXT NOT NULL,
            status TEXT NOT NULL,
            views  INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool) {
    for (t, s, v) in [
        ("a", "draft", 10_i64),
        ("b", "draft", 20),
        ("c", "published", 30),
    ] {
        let mut p = Post {
            id: Auto::default(),
            title: t.into(),
            status: s.into(),
            views: v,
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn bare_name_static_methods_work() {
    let pool = make_pool().await;
    seed(&pool).await;
    // find / first / all / count / exists / doesnt_exist.
    assert!(Post::find(1_i64, &pool).await.unwrap().is_some());
    assert!(Post::find_or_fail(1_i64, &pool).await.is_ok());
    assert_eq!(
        Post::find_many([1_i64, 2, 3], &pool).await.unwrap().len(),
        3
    );
    assert!(Post::first(&pool).await.unwrap().is_some());
    assert!(Post::first_or_fail(&pool).await.is_ok());
    assert_eq!(Post::all(&pool).await.unwrap().len(), 3);
    assert_eq!(Post::count(&pool).await.unwrap(), 3);
    assert!(Post::exists(&pool).await.unwrap());
    assert!(!Post::doesnt_exist(&pool).await.unwrap());

    // sole.
    assert!(Post::sole("status", "published", &pool).await.is_ok());

    // first_where.
    assert!(Post::first_where("status", "draft", &pool)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn bare_name_filter_methods_work() {
    let pool = make_pool().await;
    seed(&pool).await;
    assert_eq!(
        Post::where_("status", "draft", &pool).await.unwrap().len(),
        2
    );
    assert_eq!(
        Post::where_in("status", ["draft", "published"], &pool)
            .await
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        Post::where_not_in("status", ["draft"], &pool)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        Post::where_between("views", 15_i64, 25_i64, &pool)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        Post::where_not_between("views", 15_i64, 25_i64, &pool)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        Post::where_gt("views", 15_i64, &pool).await.unwrap().len(),
        2
    );
    assert_eq!(
        Post::where_gte("views", 20_i64, &pool).await.unwrap().len(),
        2
    );
    assert_eq!(
        Post::where_lt("views", 20_i64, &pool).await.unwrap().len(),
        1
    );
    assert_eq!(
        Post::where_lte("views", 20_i64, &pool).await.unwrap().len(),
        2
    );
    assert_eq!(
        Post::where_ne("status", "draft", &pool)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn bare_name_like_methods_work() {
    let pool = make_pool().await;
    seed(&pool).await;
    let _ = Post::where_like("title", "a%", &pool).await.unwrap();
    let _ = Post::where_ilike("title", "A%", &pool).await.unwrap();
    let _ = Post::where_not_like("title", "z%", &pool).await.unwrap();
    let _ = Post::where_not_ilike("title", "Z%", &pool).await.unwrap();
    let _ = Post::where_starts_with("title", "a", &pool).await.unwrap();
    let _ = Post::where_ends_with("title", "c", &pool).await.unwrap();
    let _ = Post::where_contains("title", "b", &pool).await.unwrap();
}

#[tokio::test]
async fn bare_name_aggregate_and_ordering_work() {
    let pool = make_pool().await;
    seed(&pool).await;
    assert_eq!(Post::sum::<i64>("views", &pool).await.unwrap(), Some(60));
    assert_eq!(Post::min::<i64>("views", &pool).await.unwrap(), Some(10));
    assert_eq!(Post::max::<i64>("views", &pool).await.unwrap(), Some(30));
    assert!(Post::avg::<f64>("views", &pool).await.unwrap().is_some());
    assert!(Post::random(&pool).await.unwrap().is_some());
    assert_eq!(Post::random_n(2, &pool).await.unwrap().len(), 2);
    assert_eq!(Post::oldest("views", &pool).await.unwrap()[0].views, 10);
    assert_eq!(Post::newest("views", &pool).await.unwrap()[0].views, 30);
    assert!(Post::latest("views", &pool).await.unwrap().is_some());
    assert!(Post::earliest("views", &pool).await.unwrap().is_some());
    assert_eq!(Post::take(2, &pool).await.unwrap().len(), 2);
    assert_eq!(Post::for_page(2, 2, &pool).await.unwrap().len(), 1);
    assert_eq!(
        Post::value::<String>("title", &pool).await.unwrap(),
        Some("a".into())
    );
    assert_eq!(
        Post::pluck::<String>("title", &pool).await.unwrap().len(),
        3
    );
}

#[tokio::test]
async fn bare_name_instance_methods_work() {
    let pool = make_pool().await;
    seed(&pool).await;
    let p = Post::find(1_i64, &pool).await.unwrap().unwrap();
    let _ = p.fresh(&pool).await.unwrap();
    let _ = p.increment("views", 1, &pool).await.unwrap();
    let _ = p.decrement("views", 1, &pool).await.unwrap();
    let mut p2 = Post::find(1_i64, &pool).await.unwrap().unwrap();
    p2.refresh_from_db(&pool).await.unwrap();
}

#[tokio::test]
async fn bare_name_bulk_methods_work() {
    let pool = make_pool().await;
    seed(&pool).await;
    let n = Post::update_where("status", "draft", "status", "archived", &pool)
        .await
        .unwrap();
    assert_eq!(n, 2);
    let n = Post::delete_where("status", "published", &pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
    let n = Post::update_all("title", "stamped", &pool).await.unwrap();
    assert_eq!(n, 2);
    let _ = Post::truncate(&pool).await.unwrap();
    assert!(Post::doesnt_exist(&pool).await.unwrap());
}

#[tokio::test]
async fn bare_name_date_part_methods_work() {
    // Type-check only — no rows seeded with datetime cols in this test;
    // we just confirm the aliases compile + reach the DB.
    let pool = make_pool().await;
    let _ = Post::where_year("views", 2024, &pool).await;
    let _ = Post::where_month("views", 1, &pool).await;
    let _ = Post::where_day("views", 1, &pool).await;
    let _ = Post::where_hour("views", 1, &pool).await;
    let _ = Post::where_minute("views", 1, &pool).await;
}
