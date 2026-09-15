#![cfg(feature = "sqlite")]
//! Live SQLite regression for `crate::sql::explain_pool` — closes #272 / T1.10.
//!
//! SQLite emits `EXPLAIN QUERY PLAN`, which is plan-only (never runs the
//! query). Text-format output joins the per-row `detail` column with
//! newlines; JSON-format emits `[{id, parent, detail}, ...]`.

use rustango::core::Column as _;
use rustango::sql::{explain_pool, sqlx, Auto, ExplainFormat, ExplainOptions, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "explain_sqlite_demo")]
#[rustango(app = "explain_pool_sqlite_live")]
#[allow(dead_code)]
pub struct Demo {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub label: String,
}

async fn pool_with_schema() -> Pool {
    let pool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory pool");
    sqlx::query(
        r#"CREATE TABLE explain_sqlite_demo (
            id    INTEGER PRIMARY KEY AUTOINCREMENT,
            label TEXT NOT NULL
        )"#,
    )
    .execute(&pool)
    .await
    .expect("create table");
    Pool::Sqlite(pool)
}

fn select_query() -> rustango::core::SelectQuery {
    Demo::objects()
        .where_(Demo::label.eq("alpha"))
        .compile()
        .expect("compile")
}

#[tokio::test]
async fn explain_pool_returns_plan_text() {
    let pool = pool_with_schema().await;
    let q = select_query();
    let plan = explain_pool(&pool, &q, ExplainOptions::default())
        .await
        .expect("explain text");
    // SQLite's plan detail mentions the source table — proof the plan was
    // emitted against our query, not just a no-op string.
    assert!(!plan.is_empty(), "expected non-empty plan");
    assert!(
        plan.to_ascii_uppercase().contains("EXPLAIN_SQLITE_DEMO"),
        "plan should reference our table, got:\n{plan}"
    );
}

#[tokio::test]
async fn explain_pool_returns_plan_json() {
    let pool = pool_with_schema().await;
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
    // JSON shape — first byte is `[`, content includes our table.
    assert!(plan.starts_with('['), "expected JSON array, got: {plan}");
    assert!(
        plan.to_ascii_uppercase().contains("EXPLAIN_SQLITE_DEMO"),
        "JSON plan should reference our table, got:\n{plan}"
    );
    // Validate it's parseable JSON.
    let v: serde_json::Value = serde_json::from_str(&plan).expect("parse JSON");
    assert!(v.is_array(), "expected JSON array");
}

#[tokio::test]
async fn explain_pool_silently_no_ops_analyze_flag_on_sqlite() {
    let pool = pool_with_schema().await;
    let q = select_query();
    // analyze=true + buffers=true are PG-only knobs. SQLite must NOT
    // error on them — it silently ignores per the issue spec.
    let plan = explain_pool(
        &pool,
        &q,
        ExplainOptions {
            analyze: true,
            buffers: true,
            verbose: true,
            format: ExplainFormat::Text,
        },
    )
    .await
    .expect("explain analyze+buffers should silently no-op on sqlite");
    assert!(
        !plan.is_empty(),
        "expected non-empty plan even with no-op flags"
    );
}
