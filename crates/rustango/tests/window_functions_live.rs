#![cfg(feature = "postgres")]
//! Live PG tests for window functions (issue #7). The emission tests
//! pin the SQL string; this confirms the database actually returns
//! the per-window-function values we expect.
//!
//! Skips silently when `DATABASE_URL` is unset.

use std::sync::OnceLock;

use rustango::core::window::{dense_rank, lag, rank, row_number};
use rustango::core::SqlValue;
use rustango::sql::__macro_internals::fetch_aggregate_on;
use rustango::sql::{sqlx, Auto};
use rustango::Model;
use tokio::sync::Mutex;

fn live_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "wfl_user")]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub tenant_id: i64,
    #[rustango(max_length = 100)]
    pub name: String,
    pub score: i64,
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

async fn fresh(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "wfl_user" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "wfl_user" (
            "id" BIGSERIAL PRIMARY KEY,
            "tenant_id" BIGINT NOT NULL,
            "name" VARCHAR(100) NOT NULL,
            "score" BIGINT NOT NULL
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    // Two tenants: tenant 1 has scores 30/20/10; tenant 2 has 100/50.
    sqlx::query(
        r#"INSERT INTO "wfl_user" ("tenant_id", "name", "score") VALUES
            (1, 'Alice',   30),
            (1, 'Bob',     20),
            (1, 'Carol',   10),
            (2, 'Dave',   100),
            (2, 'Eve',     50)"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

fn get_i64(row: &std::collections::HashMap<String, SqlValue>, key: &str) -> i64 {
    match row.get(key).unwrap_or(&SqlValue::Null) {
        SqlValue::I64(n) => *n,
        SqlValue::I32(n) => i64::from(*n),
        other => panic!("expected i64 at `{key}`, got {other:?}"),
    }
}

/// Rank users by score within each tenant — the issue #7 integration
/// target. Tenant 1: Alice=1, Bob=2, Carol=3. Tenant 2: Dave=1, Eve=2.
#[tokio::test]
async fn rank_by_score_within_each_tenant() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    // Use a raw SELECT to project both the row + the window — the
    // aggregate path returns rows keyed by ("id", "name", "r"). To
    // make this work with `fetch_aggregate_on`, group by every column
    // we want to project (PG accepts the equivalent of "GROUP BY
    // ALL" via the trivial group on the PK).
    use rustango::core::aggregates::max;
    let q = User::objects()
        .aggregate()
        .group_by("id")
        .group_by("tenant_id")
        .group_by("name")
        .group_by("score")
        .annotate("max_id", max("id").into())
        .annotate(
            "r",
            rank()
                .partition_by("tenant_id")
                .order_by(&[("score", true)])
                .into(),
        )
        .order_by(&[("tenant_id", false), ("score", true)])
        .compile()
        .unwrap();
    let rows = fetch_aggregate_on(&q, &pool).await.unwrap();
    assert_eq!(rows.len(), 5);
    // Order is tenant_id asc, score desc → Alice(t1,30,1), Bob(t1,20,2),
    // Carol(t1,10,3), Dave(t2,100,1), Eve(t2,50,2).
    let ranks: Vec<i64> = rows.iter().map(|r| get_i64(r, "r")).collect();
    assert_eq!(ranks, vec![1, 2, 3, 1, 2]);
    let names: Vec<String> = rows
        .iter()
        .map(|r| match r.get("name") {
            Some(SqlValue::String(s)) => s.clone(),
            other => panic!("expected string name, got {other:?}"),
        })
        .collect();
    assert_eq!(names, vec!["Alice", "Bob", "Carol", "Dave", "Eve"]);

    cleanup(&pool).await;
}

/// Dense-rank: ties don't skip ranks. With a tied score, ranks should
/// be 1,2,2,3 instead of RANK's 1,2,2,4.
#[tokio::test]
async fn dense_rank_does_not_skip_on_ties() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;
    // Add two rows tied at score=50 in tenant 2.
    sqlx::query(r#"INSERT INTO "wfl_user" ("tenant_id", "name", "score") VALUES (2, 'Frank', 50), (2, 'Grace', 50)"#).execute(&pool)
        .await
        .unwrap();

    use rustango::core::aggregates::max;
    let q = User::objects()
        .aggregate()
        .group_by("id")
        .group_by("tenant_id")
        .group_by("name")
        .group_by("score")
        .annotate("_a", max("id").into())
        .annotate(
            "dr",
            dense_rank()
                .partition_by("tenant_id")
                .order_by(&[("score", true)])
                .into(),
        )
        .order_by(&[("tenant_id", false), ("score", true), ("id", false)])
        .compile()
        .unwrap();
    let rows = fetch_aggregate_on(&q, &pool).await.unwrap();
    // Tenant 2 rows in score-desc order: Dave (100), Eve/Frank/Grace
    // (all 50). Dense ranks: 1, 2, 2, 2. Sorted as appended in fresh()
    // + the two ties, the tenant_id=2 segment is the last 4 rows.
    let tenant2: Vec<(String, i64)> = rows
        .iter()
        .filter_map(|r| {
            let t = get_i64(r, "tenant_id");
            if t != 2 {
                return None;
            }
            let name = match r.get("name") {
                Some(SqlValue::String(s)) => s.clone(),
                _ => return None,
            };
            Some((name, get_i64(r, "dr")))
        })
        .collect();
    assert_eq!(tenant2.len(), 4);
    assert_eq!(tenant2[0], ("Dave".into(), 1));
    // Eve/Frank/Grace all share dense-rank 2.
    for (_, dr) in &tenant2[1..] {
        assert_eq!(*dr, 2, "tied rows should share dense-rank 2");
    }

    cleanup(&pool).await;
}

/// Row-number: sequential 1-based index within the partition,
/// regardless of ties.
#[tokio::test]
async fn row_number_returns_sequential_index() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    use rustango::core::aggregates::max;
    let q = User::objects()
        .aggregate()
        .group_by("id")
        .group_by("tenant_id")
        .group_by("score")
        .annotate("_a", max("id").into())
        .annotate(
            "rn",
            row_number()
                .partition_by("tenant_id")
                .order_by(&[("score", true), ("id", false)])
                .into(),
        )
        .order_by(&[("tenant_id", false), ("score", true), ("id", false)])
        .compile()
        .unwrap();
    let rows = fetch_aggregate_on(&q, &pool).await.unwrap();
    // Tenant 1 has 3 rows → row numbers 1, 2, 3.
    let tenant1: Vec<i64> = rows
        .iter()
        .filter(|r| get_i64(r, "tenant_id") == 1)
        .map(|r| get_i64(r, "rn"))
        .collect();
    assert_eq!(tenant1, vec![1, 2, 3]);

    cleanup(&pool).await;
}

/// Lag: previous-row value within the partition, default substituted
/// when out of range. For the first row of each partition (no prior
/// row), `LAG(score, 1, 0)` should return 0.
#[tokio::test]
async fn lag_returns_prior_row_value_with_default_on_edge() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    use rustango::core::aggregates::max;
    let q = User::objects()
        .aggregate()
        .group_by("id")
        .group_by("tenant_id")
        .group_by("score")
        .annotate("_a", max("id").into())
        .annotate(
            "prev_score",
            lag("score", 1, Some(SqlValue::I64(0)))
                .partition_by("tenant_id")
                .order_by(&[("score", true)])
                .into(),
        )
        .order_by(&[("tenant_id", false), ("score", true), ("id", false)])
        .compile()
        .unwrap();
    let rows = fetch_aggregate_on(&q, &pool).await.unwrap();
    // Tenant 1 in score-desc: Alice(30) → prev=0 (no prior),
    // Bob(20)   → prev=30, Carol(10) → prev=20.
    let tenant1: Vec<i64> = rows
        .iter()
        .filter(|r| get_i64(r, "tenant_id") == 1)
        .map(|r| get_i64(r, "prev_score"))
        .collect();
    assert_eq!(tenant1, vec![0, 30, 20]);

    cleanup(&pool).await;
}

async fn cleanup(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "wfl_user" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
}
