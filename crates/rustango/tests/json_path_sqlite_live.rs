#![cfg(feature = "sqlite")]
//! Live SQLite regression for JSON path lookups — issue #296 / T2.3.
//!
//! Runs the emitted `json_extract(<col>, '$.path...')` SQL against an
//! in-memory SQLite database to prove the writer's emission is
//! actually executable, not just syntactically plausible.

use rustango::core::funcs::{json_path, json_path_indexed};
use rustango::core::{Expr, JsonPathStep, Op, SqlValue, WhereExpr, F};
use rustango::sql::{explain_pool, sqlx, Auto, ExplainOptions, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "json_path_demo")]
#[rustango(app = "json_path_sqlite_live")]
#[allow(dead_code)]
pub struct Demo {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub data: serde_json::Value,
}

async fn pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE json_path_demo (
            id   INTEGER PRIMARY KEY AUTOINCREMENT,
            data TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    sqlx::query(r#"INSERT INTO json_path_demo (data) VALUES ('{"address":{"city":"NYC"},"items":[{"name":"x"}]}')"#)
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
        rhs: Expr::Literal(SqlValue::String("NYC".into())),
    });
    let q = qs.compile().unwrap();
    let plan = explain_pool(p, &q, ExplainOptions::default())
        .await
        .expect("explain SHOULD parse");
    assert!(!plan.is_empty(), "expected non-empty plan");
}

#[tokio::test]
async fn single_key_path_parses_on_sqlite() {
    let p = pool().await;
    assert_expr_parses(&p, json_path(F("data"), &["city"], true)).await;
}

#[tokio::test]
async fn nested_key_path_parses_on_sqlite() {
    let p = pool().await;
    assert_expr_parses(&p, json_path(F("data"), &["address", "city"], true)).await;
}

#[tokio::test]
async fn array_indexed_path_parses_on_sqlite() {
    let p = pool().await;
    assert_expr_parses(
        &p,
        json_path_indexed(
            F("data"),
            [
                JsonPathStep::Key("items".into()),
                JsonPathStep::Index(0),
                JsonPathStep::Key("name".into()),
            ],
            true,
        ),
    )
    .await;
}
