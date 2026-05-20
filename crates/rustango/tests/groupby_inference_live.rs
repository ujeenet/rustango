#![cfg(feature = "postgres")]
//! Live PG end-to-end tests for GROUP BY auto-inference (issue #75).
//! The emission tests pin the SQL strings; this confirms the database
//! actually groups rows the way we expect.
//!
//! Skips silently when `DATABASE_URL` is unset.

use std::collections::HashMap;
use std::sync::OnceLock;

use rustango::core::aggregates::{count_all, sum};
use rustango::core::SqlValue;
use rustango::sql::{fetch_aggregate_on, sqlx, Auto};
use rustango::Model;
use tokio::sync::Mutex;

fn live_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "gbi_sale")]
#[allow(dead_code)]
pub struct Sale {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub author_id: i64,
    #[rustango(max_length = 10)]
    pub month: String,
    pub amount: i64,
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

async fn fresh(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "gbi_sale" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "gbi_sale" (
            "id" BIGSERIAL PRIMARY KEY,
            "author_id" BIGINT NOT NULL,
            "month" VARCHAR(10) NOT NULL,
            "amount" BIGINT NOT NULL
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO "gbi_sale" ("author_id", "month", "amount") VALUES
            (1, '2026-01',  100),
            (1, '2026-01',   50),
            (1, '2026-02',  200),
            (2, '2026-01',  300),
            (2, '2026-02',  400),
            (2, '2026-02',   50),
            (3, '2026-01',  999)"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn cleanup(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "gbi_sale" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
}

fn get_i64(row: &HashMap<String, SqlValue>, key: &str) -> i64 {
    match row.get(key).unwrap_or(&SqlValue::Null) {
        SqlValue::I64(n) => *n,
        other => panic!("expected i64 at `{key}`, got {other:?}"),
    }
}

fn get_string<'r>(row: &'r HashMap<String, SqlValue>, key: &str) -> &'r str {
    match row.get(key).unwrap_or(&SqlValue::Null) {
        SqlValue::String(s) => s.as_str(),
        other => panic!("expected String at `{key}`, got {other:?}"),
    }
}

/// Shape 2 — `.values("author_id").annotate("n", count(*))`.
/// "Sales per author" — the Django canonical example.
#[tokio::test]
async fn shape2_posts_per_author() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let q = Sale::objects()
        .values(&["author_id"])
        .annotate("n", count_all().into())
        .compile()
        .unwrap();
    let rows = fetch_aggregate_on(&q, &pool).await.unwrap();
    assert_eq!(rows.len(), 3, "three distinct authors");
    let mut by_author: HashMap<i64, i64> = rows
        .iter()
        .map(|r| (get_i64(r, "author_id"), get_i64(r, "n")))
        .collect();
    assert_eq!(by_author.remove(&1), Some(3), "author 1 → 3 sales");
    assert_eq!(by_author.remove(&2), Some(3), "author 2 → 3 sales");
    assert_eq!(by_author.remove(&3), Some(1), "author 3 → 1 sale");

    cleanup(&pool).await;
}

/// Shape 2 multi-column — `.values("author_id", "month").annotate("total", sum)`.
/// "Monthly revenue per author".
#[tokio::test]
async fn shape2_monthly_revenue_per_author() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let q = Sale::objects()
        .values(&["author_id", "month"])
        .annotate("total", sum("amount").into())
        .compile()
        .unwrap();
    let rows = fetch_aggregate_on(&q, &pool).await.unwrap();
    // Distinct (author, month) pairs: (1,'01'), (1,'02'), (2,'01'),
    // (2,'02'), (3,'01') = 5 rows.
    assert_eq!(rows.len(), 5, "5 (author, month) buckets");

    // Spot-check author 1 / month 2026-01 totals 100 + 50 = 150.
    let row = rows
        .iter()
        .find(|r| get_i64(r, "author_id") == 1 && get_string(r, "month") == "2026-01")
        .expect("author=1, month=2026-01 row");
    assert_eq!(get_i64(row, "total"), 150);

    // Author 2 / month 2026-02 totals 400 + 50 = 450.
    let row = rows
        .iter()
        .find(|r| get_i64(r, "author_id") == 2 && get_string(r, "month") == "2026-02")
        .expect("author=2, month=2026-02 row");
    assert_eq!(get_i64(row, "total"), 450);

    cleanup(&pool).await;
}

/// Shape 3 — bare `.annotate(...)` without `.values(...)`. Django's
/// implicit "GROUP BY every selected non-aggregate column" rule. The
/// test runs the query and proves the database accepts the inferred
/// SELECT-all-cols + GROUP-BY-all-cols shape.
#[tokio::test]
async fn shape3_bare_annotate_runs_with_group_by_all() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let q = Sale::objects()
        .annotate("n", count_all().into())
        .compile()
        .unwrap();
    let rows = fetch_aggregate_on(&q, &pool).await.unwrap();
    // Every row has a unique (id, author_id, month, amount) tuple,
    // so GROUP-BY-all-cols collapses to no-op — 7 rows in, 7 rows out.
    assert_eq!(rows.len(), 7, "GROUP BY every col → row count unchanged");
    // Every result row should carry n=1 (each input row is its own group).
    for r in &rows {
        assert_eq!(get_i64(r, "n"), 1, "GROUP BY all cols → COUNT = 1 per row");
    }

    cleanup(&pool).await;
}
