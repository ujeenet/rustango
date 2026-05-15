#![cfg(feature = "postgres")]
//! Live PG tests for filtered aggregates + COALESCE-on-empty +
//! StdDev (issue #6). The emission tests pin the SQL strings; this
//! confirms the database actually returns the conditional + stat
//! values we expect.
//!
//! Skips silently when `DATABASE_URL` is unset.

use std::sync::OnceLock;

use rustango::core::aggregates::{count, count_all, stddev, sum};
use rustango::core::{Column as _, SqlValue};
use rustango::sql::{fetch_aggregate, sqlx, Auto};
use rustango::Model;
use tokio::sync::Mutex;

fn live_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "afl_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 20)]
    pub status: String,
    pub is_active: bool,
    pub price: i64,
    pub pages: i64,
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

async fn fresh(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "afl_post" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "afl_post" (
            "id" BIGSERIAL PRIMARY KEY,
            "status" VARCHAR(20) NOT NULL,
            "is_active" BOOLEAN NOT NULL,
            "price" BIGINT NOT NULL,
            "pages" BIGINT NOT NULL
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO "afl_post" ("status", "is_active", "price", "pages") VALUES
            ('published', TRUE,  100, 50),
            ('published', TRUE,  200, 100),
            ('published', FALSE, 300, 200),
            ('draft',     TRUE,  400, 25)"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

fn get_i64(rows: &[std::collections::HashMap<String, SqlValue>], key: &str) -> i64 {
    let row = rows.first().expect("at least one aggregate row");
    match row.get(key).unwrap_or(&SqlValue::Null) {
        SqlValue::I64(n) => *n,
        other => panic!("expected i64 at `{key}`, got {other:?}"),
    }
}

fn get_f64(rows: &[std::collections::HashMap<String, SqlValue>], key: &str) -> f64 {
    let row = rows.first().expect("at least one aggregate row");
    match row.get(key).unwrap_or(&SqlValue::Null) {
        SqlValue::F64(f) => *f,
        SqlValue::F32(f) => f64::from(*f),
        other => panic!("expected float at `{key}`, got {other:?}"),
    }
}

/// Filtered count: "active published posts" — 2 of 4 rows match.
#[tokio::test]
async fn filtered_count_matches_predicate() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let q = Post::objects()
        .aggregate()
        .values(&[])
        .annotate(
            "active_published",
            count_all()
                .filter(Post::is_active.eq(true).and(Post::status.eq("published")))
                .into(),
        )
        .compile()
        .unwrap();
    let rows = fetch_aggregate(&q, &pool).await.unwrap();
    assert_eq!(get_i64(&rows, "active_published"), 2);

    cleanup(&pool).await;
}

/// Filtered SUM with COALESCE default — happy path: 100+200=300 for
/// active+published. Empty-result path: predicate excludes every
/// row → inner SUM is NULL → COALESCE substitutes 0.
#[tokio::test]
async fn filtered_sum_with_default_falls_back_when_empty() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let q = Post::objects()
        .aggregate()
        .values(&[])
        .annotate(
            "revenue",
            sum("price")
                .filter(Post::is_active.eq(true).and(Post::status.eq("published")))
                .default(0_i64)
                .into(),
        )
        .compile()
        .unwrap();
    let rows = fetch_aggregate(&q, &pool).await.unwrap();
    assert_eq!(get_i64(&rows, "revenue"), 300);

    let q = Post::objects()
        .aggregate()
        .values(&[])
        .annotate(
            "revenue",
            sum("price")
                .filter(Post::status.eq("__never_matches__"))
                .default(0_i64)
                .into(),
        )
        .compile()
        .unwrap();
    let rows = fetch_aggregate(&q, &pool).await.unwrap();
    assert_eq!(
        get_i64(&rows, "revenue"),
        0,
        "empty SUM should COALESCE to the default"
    );

    cleanup(&pool).await;
}

/// `COUNT(col) FILTER (WHERE …)` column-arg path — confirms the
/// non-`COUNT(*)` shape executes end-to-end.
#[tokio::test]
async fn filtered_count_column_arg_executes() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let q = Post::objects()
        .aggregate()
        .values(&[])
        .annotate("n", count("price").filter(Post::pages.gt(75_i64)).into())
        .compile()
        .unwrap();
    let rows = fetch_aggregate(&q, &pool).await.unwrap();
    // pages > 75: rows with pages=100, pages=200 → 2.
    assert_eq!(get_i64(&rows, "n"), 2);

    cleanup(&pool).await;
}

/// Native PG `STDDEV_SAMP(col)` executes end-to-end. Exact stddev
/// is brittle to assert; check the result is finite and positive
/// (the four pages values 25/50/100/200 give a real stddev around 75).
#[tokio::test]
async fn stddev_returns_a_finite_positive_number() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let q = Post::objects()
        .aggregate()
        .values(&[])
        .annotate("sd", stddev("pages").into())
        .compile()
        .unwrap();
    let rows = fetch_aggregate(&q, &pool).await.unwrap();
    let sd = get_f64(&rows, "sd");
    assert!(
        sd.is_finite() && sd > 0.0,
        "stddev should be a finite positive number: {sd}"
    );

    cleanup(&pool).await;
}

async fn cleanup(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "afl_post" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
}
