#![cfg(all(feature = "postgres", feature = "tenancy"))]
//! Live PG regression for `crate::sql::explain_pool` — closes #272 / T1.10.
//!
//! Mirrors `tests/explain_live.rs` (the PG-typed `QuerySet::explain_on`
//! coverage) but exercises the tri-dialect `_pool` variant through
//! `Pool::Postgres`. Same `SelectQuery` shape as the sqlite / mysql
//! live tests — the issue requires identical IR produces non-empty
//! output across all three backends.

use rustango::core::Column as _;
use rustango::sql::{explain_pool, sqlx, Auto, ExplainFormat, ExplainOptions, Pool};
use rustango::Model;
use tokio::sync::Mutex;

#[derive(Model, Debug, Clone)]
#[rustango(table = "_explain_pool_demo")]
#[allow(dead_code)]
pub struct Demo {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub label: String,
}

fn live_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<Pool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pg = sqlx::PgPool::connect(&url).await.ok()?;
    Some(Pool::Postgres(pg))
}

async fn fresh(pool: &Pool) {
    let Pool::Postgres(pg) = pool else {
        unreachable!()
    };
    sqlx::query(r#"DROP TABLE IF EXISTS "_explain_pool_demo" CASCADE"#)
        .execute(pg)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "_explain_pool_demo" (
            "id"    BIGSERIAL    PRIMARY KEY,
            "label" VARCHAR(64)  NOT NULL
        )"#,
    )
    .execute(pg)
    .await
    .unwrap();
    for label in ["alpha", "beta", "gamma"] {
        sqlx::query(r#"INSERT INTO "_explain_pool_demo" ("label") VALUES ($1)"#)
            .bind(label)
            .execute(pg)
            .await
            .unwrap();
    }
}

fn select_query() -> rustango::core::SelectQuery {
    Demo::objects()
        .where_(Demo::label.eq("alpha"))
        .compile()
        .expect("compile")
}

#[tokio::test]
async fn explain_pool_returns_plan_text_on_pg() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    let q = select_query();
    let plan = explain_pool(&pool, &q, ExplainOptions::default())
        .await
        .expect("explain text");
    assert!(!plan.is_empty(), "EXPLAIN should return non-empty output");
    assert!(
        plan.contains("Scan") || plan.contains("Filter") || plan.contains("Plan"),
        "expected planner output, got:\n{plan}"
    );
}

#[tokio::test]
async fn explain_pool_returns_plan_json_on_pg() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    let q = select_query();
    let plan = explain_pool(
        &pool,
        &q,
        ExplainOptions {
            format: ExplainFormat::Json,
            ..Default::default()
        },
    )
    .await
    .expect("explain json");
    assert!(!plan.is_empty(), "expected non-empty plan");
    // PG `FORMAT JSON` returns a single-row JSON array string.
    let parsed: serde_json::Value =
        serde_json::from_str(&plan).expect("EXPLAIN(FORMAT JSON) output should parse");
    assert!(parsed.is_array(), "expected JSON array, got: {parsed}");
}

#[tokio::test]
async fn explain_pool_with_analyze_reports_actual_timings_on_pg() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    let q = select_query();
    let plan = explain_pool(
        &pool,
        &q,
        ExplainOptions {
            analyze: true,
            buffers: true,
            verbose: false,
            format: ExplainFormat::Text,
        },
    )
    .await
    .expect("explain analyze");
    assert!(
        plan.contains("actual time="),
        "ANALYZE output should include actual-timing column:\n{plan}"
    );
}
