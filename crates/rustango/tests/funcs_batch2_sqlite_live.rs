#![cfg(feature = "sqlite")]
//! Live SQLite regression for DB-functions batch 2 — issue #294 / T2.7.
//!
//! Runs each SQLite-compatible function against an in-memory database
//! to prove the emitted SQL is actually executable, not just
//! syntactically plausible. Functions that require
//! `SQLITE_ENABLE_MATH_FUNCTIONS` (Log, LogWithBase, Exp) are skipped
//! because sqlx-sqlite's default build does not enable the flag — the
//! emission tests in `funcs_batch2.rs` pin the `OpNotSupportedInDialect`
//! error path for those.

use rustango::core::funcs;
use rustango::core::{Expr, Op, SqlValue, WhereExpr, F};
use rustango::sql::{explain_pool, sqlx, Auto, ExplainOptions, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "funcs_b2_demo")]
#[rustango(app = "funcs_batch2_sqlite_live")]
#[allow(dead_code)]
pub struct Demo {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub ts: chrono::DateTime<chrono::Utc>,
    pub amount: i64,
}

async fn pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE funcs_b2_demo (
            id     INTEGER PRIMARY KEY AUTOINCREMENT,
            ts     TEXT NOT NULL,
            amount INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    sqlx::query("INSERT INTO funcs_b2_demo (ts, amount) VALUES ('2024-06-15 12:30:45', 1)")
        .execute(&p)
        .await
        .unwrap();
    Pool::Sqlite(p)
}

async fn assert_expr_parses(p: &Pool, e: Expr) {
    use rustango::query::QuerySet;
    let qs = QuerySet::<Demo>::default().where_raw(WhereExpr::ExprCompare {
        lhs: e,
        op: Op::Eq,
        // Compare against a literal `1` — anything that compiles is good;
        // we're just exercising the writer's emission against a live
        // SQLite planner.
        rhs: Expr::Literal(SqlValue::I64(1)),
    });
    let q = qs.compile().unwrap();
    let plan = explain_pool(p, &q, ExplainOptions::default())
        .await
        .expect("explain SHOULD parse");
    assert!(!plan.is_empty(), "expected non-empty plan");
}

#[tokio::test]
async fn pi_inlined_literal_executes_on_sqlite() {
    let p = pool().await;
    assert_expr_parses(&p, funcs::pi()).await;
}

#[tokio::test]
async fn random_executes_on_sqlite() {
    let p = pool().await;
    assert_expr_parses(&p, funcs::random()).await;
}

#[tokio::test]
async fn age_julianday_expansion_executes_on_sqlite() {
    let p = pool().await;
    assert_expr_parses(&p, funcs::age(F("ts"), F("ts"))).await;
}

#[tokio::test]
async fn trunc_with_tz_strftime_form_executes_on_sqlite() {
    let p = pool().await;
    assert_expr_parses(&p, funcs::trunc_with_tz(F("ts"), "day", "+00:00")).await;
}

// Log / LogWithBase / Exp / MakeInterval are intentionally not
// exercised here — Log/LogWithBase/Exp need SQLITE_ENABLE_MATH_FUNCTIONS
// (not enabled in sqlx-sqlite's default build); MakeInterval is PG-only.
// The emission tests in `funcs_batch2.rs` cover the
// `OpNotSupportedInDialect` error path for all four.
