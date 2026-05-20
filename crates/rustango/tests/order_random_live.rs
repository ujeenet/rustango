#![cfg(feature = "postgres")]
//! Live PG randomness sanity check for `.order_random()` (issue #77).
//! Probabilistic: with 100 rows + LIMIT 10, the probability that two
//! consecutive `.fetch()` calls return identical orderings is
//! C(100,10) ⁻¹ ≈ 5.8e-14. We assert "not identical across two
//! draws" — false-positive rate is roughly impossible.
//!
//! Skips silently when `DATABASE_URL` is unset.

use std::sync::OnceLock;

use rustango::sql::{sqlx, Auto};
use rustango::Model;
use tokio::sync::Mutex;

fn live_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "orl_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 20)]
    pub label: String,
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

async fn fresh(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "orl_post" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "orl_post" (
            "id" BIGSERIAL PRIMARY KEY,
            "label" VARCHAR(20) NOT NULL
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    // 100 rows, label = "n<i>" so we can identify them.
    let mut values = String::new();
    for i in 0..100 {
        if i > 0 {
            values.push_str(", ");
        }
        values.push_str(&format!("('n{i}')"));
    }
    sqlx::query(&format!(
        r#"INSERT INTO "orl_post" ("label") VALUES {values}"#
    ))
    .execute(pool)
    .await
    .unwrap();
}

/// Two consecutive `order_random().limit(10).fetch_on(...)` calls must
/// return different orderings with overwhelming probability. If they
/// match, RANDOM() is broken (or the writer is emitting a sort key
/// other than RANDOM()).
#[tokio::test]
async fn order_random_returns_different_orderings_across_calls() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let draw_a: Vec<Post> = Post::objects()
        .order_random()
        .limit(10)
        .fetch_on(&pool)
        .await
        .unwrap();
    let draw_b: Vec<Post> = Post::objects()
        .order_random()
        .limit(10)
        .fetch_on(&pool)
        .await
        .unwrap();
    assert_eq!(draw_a.len(), 10, "draw a should have 10 rows");
    assert_eq!(draw_b.len(), 10, "draw b should have 10 rows");

    let labels_a: Vec<&str> = draw_a.iter().map(|p| p.label.as_str()).collect();
    let labels_b: Vec<&str> = draw_b.iter().map(|p| p.label.as_str()).collect();
    assert_ne!(
        labels_a, labels_b,
        "two consecutive random draws of 10 from 100 rows should not be identical: {labels_a:?}"
    );

    cleanup(&pool).await;
}

/// LIMIT N returns ≤ N distinct rows from the random sort. Confirm
/// the row count is correct (the random key doesn't drop or
/// duplicate rows).
#[tokio::test]
async fn order_random_with_limit_returns_distinct_rows() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let rows: Vec<Post> = Post::objects()
        .order_random()
        .limit(15)
        .fetch_on(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 15);
    let mut ids: Vec<i64> = rows.iter().map(|p| *p.id.get().unwrap()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(
        ids.len(),
        15,
        "ORDER BY RANDOM() LIMIT 15 should return 15 distinct rows: {ids:?}"
    );

    cleanup(&pool).await;
}

async fn cleanup(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "orl_post" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
}
