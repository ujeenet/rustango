#![cfg(feature = "postgres")]
//! Live PG tests for ad-hoc joins (issue #80). Pin the runtime
//! semantics — the emission tests pin the SQL strings, this confirms
//! the database actually returns the rows we expect.
//!
//! Skips silently when `DATABASE_URL` is unset.

use std::sync::OnceLock;

use rustango::core::joins::aliased;
use rustango::core::{Filter, Join, JoinKind, Model as _, Op, SqlValue, WhereExpr};
use rustango::sql::{sqlx, Auto, Fetcher};
use rustango::Model;
use tokio::sync::Mutex;

fn live_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "ajl_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 20)]
    pub status: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "ajl_comment")]
#[allow(dead_code)]
pub struct Comment {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub post_id: i64,
    #[rustango(max_length = 500)]
    pub body: String,
    pub is_approved: bool,
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

async fn fresh(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "ajl_comment" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "ajl_post" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "ajl_post" (
            "id" BIGSERIAL PRIMARY KEY,
            "title" VARCHAR(200) NOT NULL,
            "status" VARCHAR(20) NOT NULL
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"CREATE TABLE "ajl_comment" (
            "id" BIGSERIAL PRIMARY KEY,
            "post_id" BIGINT NOT NULL REFERENCES "ajl_post"("id"),
            "body" VARCHAR(500) NOT NULL,
            "is_approved" BOOLEAN NOT NULL DEFAULT FALSE
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();

    // 3 posts: P1 has approved comment, P2 has only unapproved, P3 has none.
    sqlx::query(r#"INSERT INTO "ajl_post" ("id", "title", "status") VALUES (1, 'P1', 'published'), (2, 'P2', 'published'), (3, 'P3', 'draft')"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(r#"INSERT INTO "ajl_comment" ("post_id", "body", "is_approved") VALUES (1, 'approved comment on P1', TRUE), (2, 'pending comment on P2', FALSE)"#)
        .execute(pool)
        .await
        .unwrap();
}

/// INNER JOIN with extra predicate — "posts that have at least one
/// approved comment." Only P1 should survive.
#[tokio::test]
async fn inner_join_with_extra_predicate_filters_outer_rows() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let join = Join {
        target: Comment::SCHEMA,
        alias: "c",
        kind: JoinKind::Inner,
        on: WhereExpr::And(vec![
            WhereExpr::ExprCompare {
                lhs: aliased("c", "post_id"),
                op: Op::Eq,
                rhs: aliased("ajl_post", "id"),
            },
            // Bare Filter — qualifies to `c` because the writer
            // passes `qualify_with: Some(join.alias)`.
            WhereExpr::Predicate(Filter {
                column: "is_approved",
                op: Op::Eq,
                value: SqlValue::Bool(true),
            }),
        ]),
        project: vec![],
    };

    let posts: Vec<Post> = Post::objects()
        .join(join)
        .order_by(&[("id", false)])
        .fetch(&pool)
        .await
        .unwrap();

    let titles: Vec<&str> = posts.iter().map(|p| p.title.as_str()).collect();
    assert_eq!(
        titles,
        vec!["P1"],
        "only P1 has an approved comment: {titles:?}",
    );

    cleanup(&pool).await;
}

/// LEFT JOIN with predicate moved to the ON clause — keeps every
/// outer row (3 posts) even when the joined-side predicate is false.
/// Distinguishes from INNER which drops outer rows on no-match.
#[tokio::test]
async fn left_join_preserves_outer_rows_without_match() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let join = Join {
        target: Comment::SCHEMA,
        alias: "c",
        kind: JoinKind::Left,
        on: WhereExpr::ExprCompare {
            lhs: aliased("c", "post_id"),
            op: Op::Eq,
            rhs: aliased("ajl_post", "id"),
        },
        project: vec![],
    };

    let posts: Vec<Post> = Post::objects()
        .join(join)
        .order_by(&[("id", false)])
        .fetch(&pool)
        .await
        .unwrap();

    // P1 has 1 comment, P2 has 1 comment, P3 has 0 — LEFT JOIN
    // returns 3 outer rows; however P1+P2 each appear once and P3
    // once = 3 rows because each post matches once OR no match (still
    // emits one row via LEFT JOIN). With `select_related`-style
    // duplicate handling there'd be 1 row per post-comment pair.
    // Here we expect: P1 once, P2 once, P3 once = 3 rows.
    let titles: Vec<&str> = posts.iter().map(|p| p.title.as_str()).collect();
    assert_eq!(
        titles,
        vec!["P1", "P2", "P3"],
        "LEFT JOIN keeps every outer row: {titles:?}",
    );

    cleanup(&pool).await;
}

async fn cleanup(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "ajl_comment" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "ajl_post" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
}
