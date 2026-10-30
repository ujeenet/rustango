//! Runs representative queries from each section of `docs/orm.md`
//! against a real database. Set `DATABASE_URL` to a throwaway database
//! (the suite drops and recreates the model tables on every run).
//!
//! One test function on purpose: the sections share a freshly-seeded
//! database, and a single test keeps that setup deterministic instead
//! of racing parallel drops.

use chrono::{TimeZone, Utc};
use orm_cookbook::{Author, Post};
use rustango::core::aggregates::count_all;
use rustango::core::Column as _;
use rustango::core::{Op, SqlValue, WhereExpr};
use rustango::sql::{CounterPool as _, FetcherPool as _, Pool};
use rustango::Auto;

fn url() -> String {
    std::env::var("DATABASE_URL").expect("DATABASE_URL must point at a throwaway database")
}

/// Recreate the model tables and seed a handful of posts.
async fn setup() -> Pool {
    let pool = Pool::connect(&url()).await.expect("connect");
    rustango::migrate::drop_all_pool(&pool)
        .await
        .expect("drop tables");
    rustango::migrate::apply_all_pool(&pool)
        .await
        .expect("apply_all (create tables)");

    // Seed: 3 published + 1 draft.
    for (title, status, author_id, views, active, price, pages) in [
        ("Alpha", "published", 1, 150, true, 10, 100),
        ("Beta", "published", 1, 50, true, 20, 200),
        ("Gamma", "published", 2, 300, true, 30, 300),
        ("Delta", "draft", 2, 5, false, 0, 50),
    ] {
        let mut p = Post {
            id: Auto::default(),
            title: title.into(),
            body: format!("body of {title}"),
            status: status.into(),
            author_id,
            view_count: views,
            is_active: active,
            price,
            pages,
            published_at: Auto::default(),
            created_at: Auto::default(),
            deleted_at: None,
        };
        p.save_pool(&pool).await.expect("seed post");
    }
    let mut a = Author { id: Auto::default(), name: "Ada".into() };
    a.save_pool(&pool).await.expect("seed author");

    pool
}

#[tokio::test]
async fn orm_cookbook_recipes() {
    let pool = setup().await;

    // ---- Querying ----
    let all = Post::objects().fetch(&pool).await.unwrap();
    assert_eq!(all.len(), 4);

    let drafts = Post::objects()
        .where_(Post::status.eq("draft"))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(drafts.len(), 1);

    let recent = Post::objects()
        .where_(Post::status.eq("published"))
        .where_(Post::author_id.eq(1))
        .order_by(&[("view_count", true)]) // true = DESC
        .limit(20)
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].title, "Alpha"); // 150 > 50

    let by_status = Post::objects()
        .filter_op("status", Op::Eq, SqlValue::String("published".into()))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(by_status.len(), 3);

    let either = Post::objects()
        .where_raw(WhereExpr::Or(vec![
            Post::status.eq("draft").into(),
            Post::status.eq("published").into(),
        ]))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(either.len(), 4);

    // ---- Comparison filters ----
    let hot = Post::objects()
        .where_(Post::view_count.gt(100))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(hot.len(), 2); // Alpha 150, Gamma 300

    let in_ids = Post::objects()
        .where_(Post::author_id.is_in([1, 2]))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(in_ids.len(), 4);

    let not_archived = Post::objects()
        .where_(Post::status.ne("archived"))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(not_archived.len(), 4);

    let titled = Post::objects()
        .where_(Post::title.ilike("alp%"))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(titled.len(), 1);

    let live = Post::objects()
        .where_(Post::deleted_at.is_null())
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(live.len(), 4);

    let start = Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap();
    let end = Utc.with_ymd_and_hms(2100, 1, 1, 0, 0, 0).unwrap();
    let in_range = Post::objects()
        .where_(Post::published_at.between(start, end))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(in_range.len(), 4);

    // ---- Aggregations ----
    let n = Post::objects()
        .where_(Post::status.eq("published"))
        .count(&pool)
        .await
        .unwrap();
    assert_eq!(n, 3);

    let total_views: Option<i64> = Post::objects().sum::<i64>("view_count", &pool).await.unwrap();
    assert_eq!(total_views, Some(150 + 50 + 300 + 5));

    let avg_views: Option<f64> = Post::objects().avg::<f64>("view_count", &pool).await.unwrap();
    assert!(avg_views.unwrap() > 0.0);

    let max_views: Option<i64> = Post::objects().max::<i64>("view_count", &pool).await.unwrap();
    assert_eq!(max_views, Some(300));

    // GROUP BY: posts per author
    let by_author = Post::objects()
        .values(&["author_id"])
        .annotate("n", count_all().into())
        .compile()
        .unwrap();
    let rows = rustango::sql::fetch_aggregate_dict(&pool, &by_author)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2); // authors 1 and 2
}
