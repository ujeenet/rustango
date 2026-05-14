#![cfg(feature = "postgres")]
//! Live PG end-to-end sanity test for Django-shape `.filter("field__lookup", value)`
//! (issue #71). Emission already covered by [`filter_lookup.rs`]; this file
//! proves that each major suffix family actually round-trips against a real
//! Postgres backend.
//!
//! Skips silently when `DATABASE_URL` is unset.

use std::sync::OnceLock;

use rustango::core::SqlValue;
use rustango::sql::{sqlx, Auto, Fetcher};
use rustango::Model;
use tokio::sync::Mutex;

fn live_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "fll_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 20)]
    pub status: String,
    pub views: i64,
    pub author_id: i64,
    pub deleted_at: Option<i64>,
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

async fn fresh(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "fll_post" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "fll_post" (
            "id" BIGSERIAL PRIMARY KEY,
            "title" VARCHAR(200) NOT NULL,
            "status" VARCHAR(20) NOT NULL,
            "views" BIGINT NOT NULL,
            "author_id" BIGINT NOT NULL,
            "deleted_at" BIGINT
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO "fll_post" ("title", "status", "views", "author_id", "deleted_at") VALUES
            ('Hello Rust',     'published', 100,   1, NULL),
            ('Hello World',    'published',  50,   2, NULL),
            ('Goodbye Rust',   'draft',      10,   1, 1700000000),
            ('Crab Cakes',     'published', 250,   3, NULL),
            ('Rusty Spoon',    'draft',       5,   2, 1700000001)"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn cleanup(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "fll_post" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn live_bare_field_eq() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let rows: Vec<Post> = Post::objects()
        .filter("status", "published")
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3, "three published rows");
    assert!(rows.iter().all(|r| r.status == "published"));

    cleanup(&pool).await;
}

#[tokio::test]
async fn live_gt_lt_comparisons() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let rows: Vec<Post> = Post::objects()
        .filter("views__gt", 50_i64)
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2, "two rows with views > 50");
    assert!(rows.iter().all(|r| r.views > 50));

    let rows: Vec<Post> = Post::objects()
        .filter("views__lte", 10_i64)
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2, "two rows with views <= 10");

    cleanup(&pool).await;
}

#[tokio::test]
async fn live_icontains_matches_case_insensitively() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    // Lowercase "rust" must match titles containing "Rust" / "Rusty"
    // → "Hello Rust", "Goodbye Rust", "Rusty Spoon".
    let rows: Vec<Post> = Post::objects()
        .filter("title__icontains", "rust")
        .fetch(&pool)
        .await
        .unwrap();
    let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(rows.len(), 3, "expected 3 hits for 'rust': got {titles:?}");
    assert!(titles.contains(&"Hello Rust"));
    assert!(titles.contains(&"Goodbye Rust"));
    assert!(titles.contains(&"Rusty Spoon"));

    cleanup(&pool).await;
}

#[tokio::test]
async fn live_startswith_endswith() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let rows: Vec<Post> = Post::objects()
        .filter("title__startswith", "Hello")
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2, "two titles start with 'Hello'");
    assert!(rows.iter().all(|r| r.title.starts_with("Hello")));

    let rows: Vec<Post> = Post::objects()
        .filter("title__endswith", "Rust")
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2, "two titles end with 'Rust'");
    assert!(rows.iter().all(|r| r.title.ends_with("Rust")));

    cleanup(&pool).await;
}

#[tokio::test]
async fn live_in_list_filters_match() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let rows: Vec<Post> = Post::objects()
        .filter(
            "author_id__in",
            SqlValue::List(vec![SqlValue::I64(1), SqlValue::I64(3)]),
        )
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3, "authors 1 and 3 own three rows total");
    assert!(rows.iter().all(|r| r.author_id == 1 || r.author_id == 3));

    cleanup(&pool).await;
}

#[tokio::test]
async fn live_isnull_true_and_false() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let live_rows: Vec<Post> = Post::objects()
        .filter("deleted_at__isnull", true)
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(live_rows.len(), 3, "three non-deleted rows");
    assert!(live_rows.iter().all(|r| r.deleted_at.is_none()));

    let dead_rows: Vec<Post> = Post::objects()
        .filter("deleted_at__isnull", false)
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(dead_rows.len(), 2, "two soft-deleted rows");
    assert!(dead_rows.iter().all(|r| r.deleted_at.is_some()));

    cleanup(&pool).await;
}

#[tokio::test]
async fn live_between_range_alias() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    // __between
    let rows: Vec<Post> = Post::objects()
        .filter(
            "views__between",
            SqlValue::List(vec![SqlValue::I64(10), SqlValue::I64(100)]),
        )
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3, "views in [10,100] hits 3 rows");
    assert!(rows.iter().all(|r| (10..=100).contains(&r.views)));

    // __range (Django alias)
    let rows: Vec<Post> = Post::objects()
        .filter(
            "views__range",
            SqlValue::List(vec![SqlValue::I64(10), SqlValue::I64(100)]),
        )
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);

    cleanup(&pool).await;
}

#[tokio::test]
async fn live_multi_filter_and_chain() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let rows: Vec<Post> = Post::objects()
        .filter("status", "published")
        .filter("views__gt", 50_i64)
        .filter("title__icontains", "rust")
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "only 'Hello Rust' is published, has views > 50, and matches 'rust'"
    );
    assert_eq!(rows[0].title, "Hello Rust");

    cleanup(&pool).await;
}
