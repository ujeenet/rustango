//! Tri-dialect coverage — MySQL parallel of [`json_path_sqlite_live.rs`].
//!
//! `json_path` / `json_path_indexed` emit `JSON_EXTRACT(<col>, '$.…')`
//! on MySQL + SQLite and `<col> -> '…'` / `<col> ->> '…'` on PG. The
//! SQLite suite proves the path lands on SQLite; this file proves it
//! lands on MySQL too. Without this gap-fill, an emit-time bug that
//! breaks one dialect's quote-shape can slip past CI when the other
//! dialect's parser is more permissive.
//!
//! Reads `MYSQL_TEST_URL`. Tests skip silently when unset.

#![cfg(feature = "mysql")]

use std::sync::OnceLock;

use rustango::core::funcs::{json_path, json_path_indexed};
use rustango::core::{Expr, JsonPathStep, Op, SqlValue, WhereExpr, F};
use rustango::query::QuerySet;
use rustango::sql::{explain_pool, sqlx, Auto, ExplainOptions, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "json_path_my_demo")]
#[rustango(app = "json_path_mysql_live")]
#[allow(dead_code)]
pub struct Demo {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub data: serde_json::Value,
}

fn serial_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

async fn pool() -> Option<Pool> {
    let url = std::env::var("MYSQL_TEST_URL").ok()?;
    let mp = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .ok()?;
    let _ = sqlx::query("DROP TABLE IF EXISTS json_path_my_demo")
        .execute(&mp)
        .await;
    sqlx::query(
        "CREATE TABLE json_path_my_demo (\
            id   BIGINT AUTO_INCREMENT PRIMARY KEY, \
            data JSON NOT NULL)",
    )
    .execute(&mp)
    .await
    .expect("create table");
    sqlx::query(
        r#"INSERT INTO json_path_my_demo (data) VALUES ('{"address":{"city":"NYC"},"items":[{"name":"x"}]}')"#,
    )
    .execute(&mp)
    .await
    .expect("seed row");
    Some(Pool::Mysql(mp))
}

async fn assert_expr_parses(p: &Pool, e: Expr) {
    let qs = QuerySet::<Demo>::default().where_raw(WhereExpr::ExprCompare {
        lhs: e,
        op: Op::Eq,
        rhs: Expr::Literal(SqlValue::String("NYC".into())),
    });
    let q = qs.compile().expect("compile");
    let plan = explain_pool(p, &q, ExplainOptions::default())
        .await
        .expect("explain SHOULD parse");
    assert!(!plan.is_empty(), "expected non-empty plan");
}

#[tokio::test]
async fn single_key_path_parses_on_mysql() {
    let _g = serial_lock().lock().await;
    let Some(p) = pool().await else { return };
    assert_expr_parses(&p, json_path(F("data"), &["city"], true)).await;
}

#[tokio::test]
async fn nested_key_path_parses_on_mysql() {
    let _g = serial_lock().lock().await;
    let Some(p) = pool().await else { return };
    assert_expr_parses(&p, json_path(F("data"), &["address", "city"], true)).await;
}

#[tokio::test]
async fn array_indexed_path_parses_on_mysql() {
    let _g = serial_lock().lock().await;
    let Some(p) = pool().await else { return };
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
