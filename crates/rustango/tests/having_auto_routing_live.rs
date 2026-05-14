#![cfg(feature = "postgres")]
//! Live PG test for HAVING auto-routing (issue #74). The
//! integration target from the issue body: "authors with > N
//! published posts" — confirms the routed predicates execute
//! end-to-end and the right row counts come back.
//!
//! Skips silently when `DATABASE_URL` is unset.

use std::sync::OnceLock;

use rustango::core::aggregates::count_all;
use rustango::core::{Op, SqlValue};
use rustango::sql::{fetch_aggregate, sqlx, Auto};
use rustango::Model;
use tokio::sync::Mutex;

fn live_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "harl_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub author_id: i64,
    #[rustango(max_length = 20)]
    pub status: String,
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

async fn fresh(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "harl_post" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "harl_post" (
            "id" BIGSERIAL PRIMARY KEY,
            "author_id" BIGINT NOT NULL,
            "status" VARCHAR(20) NOT NULL
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    // Author 1: 12 published, 3 draft → published_count = 12
    // Author 2: 8  published, 2 draft → published_count = 8
    // Author 3: 15 published, 0 draft → published_count = 15
    // Author 4: 5  draft → published_count = 0
    let mut values = String::new();
    let mut first = true;
    let mut push = |aid: i64, status: &str, n: usize| {
        for _ in 0..n {
            if !first {
                values.push_str(", ");
            }
            first = false;
            values.push_str(&format!("({aid}, '{status}')"));
        }
    };
    push(1, "published", 12);
    push(1, "draft", 3);
    push(2, "published", 8);
    push(2, "draft", 2);
    push(3, "published", 15);
    push(4, "draft", 5);
    sqlx::query(&format!(
        r#"INSERT INTO "harl_post" ("author_id", "status") VALUES {values}"#
    ))
    .execute(pool)
    .await
    .unwrap();
}

fn get_i64(row: &std::collections::HashMap<String, SqlValue>, key: &str) -> i64 {
    match row.get(key).unwrap_or(&SqlValue::Null) {
        SqlValue::I64(n) => *n,
        other => panic!("expected i64 at `{key}`, got {other:?}"),
    }
}

/// Issue #74 integration target: "authors with > 10 published posts".
/// WHERE filters published; HAVING filters by the COUNT alias.
/// Expected: author 1 (12 published) and author 3 (15 published) — 2 rows.
#[tokio::test]
async fn authors_with_gt_10_published_posts_uses_where_plus_having() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("post_count", count_all().into())
        // status filter goes to WHERE (real model column).
        .filter("status", Op::Eq, "published")
        // post_count filter goes to HAVING (annotation alias).
        .filter("post_count", Op::Gt, 10_i64)
        .order_by(&[("author_id", false)])
        .compile()
        .unwrap();
    let rows = fetch_aggregate(&q, &pool).await.unwrap();

    assert_eq!(
        rows.len(),
        2,
        "two authors have > 10 published posts, got {} rows: {rows:?}",
        rows.len()
    );
    let author_ids: Vec<i64> = rows.iter().map(|r| get_i64(r, "author_id")).collect();
    assert_eq!(author_ids, vec![1, 3]);
    let counts: Vec<i64> = rows.iter().map(|r| get_i64(r, "post_count")).collect();
    assert_eq!(counts, vec![12, 15]);

    cleanup(&pool).await;
}

/// HAVING-only path: count all posts per author, keep only ones with > 14 posts total.
/// No WHERE clause — pure aggregate filter. Expected: author 1 (15 total), author 3 (15 total).
#[tokio::test]
async fn having_only_no_where_clause() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let q = Post::objects()
        .aggregate()
        .group_by("author_id")
        .annotate("total_posts", count_all().into())
        .filter("total_posts", Op::Gt, 14_i64)
        .order_by(&[("author_id", false)])
        .compile()
        .unwrap();
    let rows = fetch_aggregate(&q, &pool).await.unwrap();

    let author_ids: Vec<i64> = rows.iter().map(|r| get_i64(r, "author_id")).collect();
    assert_eq!(
        author_ids,
        vec![1, 3],
        "authors 1 + 3 have 15 total posts each; got: {author_ids:?}"
    );

    cleanup(&pool).await;
}

async fn cleanup(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "harl_post" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
}
