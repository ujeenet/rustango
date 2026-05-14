#![cfg(feature = "postgres")]
//! Live PG tests for ORDER BY enhancements (issue #76). The emission
//! tests pin the SQL strings; this confirms NULLs land where the
//! `NullsOrder` says they should and Expr items sort the way the
//! expression dictates.
//!
//! Skips silently when `DATABASE_URL` is unset.

use std::sync::OnceLock;

use rustango::core::funcs::lower;
use rustango::core::{Column as _, NullsOrder, F};
use rustango::sql::{sqlx, Auto, Fetcher};
use rustango::Model;
use tokio::sync::Mutex;

fn live_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "obl_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub score: Option<i64>,
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

async fn fresh(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "obl_post" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "obl_post" (
            "id" BIGSERIAL PRIMARY KEY,
            "title" VARCHAR(200) NOT NULL,
            "score" BIGINT
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    // Mix of NULL + non-NULL scores; mixed-case titles.
    sqlx::query(
        r#"INSERT INTO "obl_post" ("title", "score") VALUES
        ('alpha', 10),
        ('Bravo', NULL),
        ('charlie', 30),
        ('Delta', 20),
        ('echo', NULL)"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

/// `NULLS LAST` on `ASC` — NULLs land after every non-NULL.
#[tokio::test]
async fn nulls_last_on_asc_groups_nulls_after_non_nulls() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let rows: Vec<Post> = Post::objects()
        .order_by_with_nulls(&[("score", false, NullsOrder::Last)])
        .order_by(&[("id", false)])
        .fetch(&pool)
        .await
        .unwrap();
    // Scores ASC NULLS LAST → 10, 20, 30, NULL, NULL.
    let scores: Vec<Option<i64>> = rows.iter().map(|p| p.score).collect();
    assert_eq!(
        scores,
        vec![Some(10), Some(20), Some(30), None, None],
        "got: {scores:?}",
    );

    cleanup(&pool).await;
}

/// `NULLS FIRST` on `DESC` — NULLs land before the largest value.
#[tokio::test]
async fn nulls_first_on_desc_groups_nulls_first() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let rows: Vec<Post> = Post::objects()
        .order_by_with_nulls(&[("score", true, NullsOrder::First)])
        .order_by(&[("id", false)])
        .fetch(&pool)
        .await
        .unwrap();
    // Scores DESC NULLS FIRST → NULL, NULL, 30, 20, 10.
    let scores: Vec<Option<i64>> = rows.iter().map(|p| p.score).collect();
    assert_eq!(
        scores,
        vec![None, None, Some(30), Some(20), Some(10)],
        "got: {scores:?}",
    );

    cleanup(&pool).await;
}

/// Expr item: `ORDER BY LOWER(title)` — sorts case-insensitively.
/// Without `LOWER`, uppercase comes before lowercase in PG default
/// collation; with it, the order is alphabetic regardless of case.
#[tokio::test]
async fn order_by_expr_lower_title_sorts_case_insensitively() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let rows: Vec<Post> = Post::objects()
        .order_by_expr(lower(F("title")), false)
        .fetch(&pool)
        .await
        .unwrap();
    let titles: Vec<&str> = rows.iter().map(|p| p.title.as_str()).collect();
    assert_eq!(
        titles,
        vec!["alpha", "Bravo", "charlie", "Delta", "echo"],
        "case-insensitive ascending: {titles:?}",
    );

    cleanup(&pool).await;
}

async fn cleanup(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "obl_post" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
}
