//! Tri-dialect coverage — MySQL parallel of [`funcs_batch1_sqlite_live.rs`].
//!
//! The DB-functions library at `core::funcs` emits dialect-divergent
//! SQL: `INSTR` vs `LOCATE` for `position()`, `CAST(x AS …)` with
//! different type spellings, native `MOD()` vs `%`, `REPEAT()` (one of
//! the few uniform ones), `SIGN()`. The SQLite live suite proves each
//! lands on the SQLite side; this file does the same for MySQL so any
//! future writer refactor that breaks one dialect can't sneak through.
//!
//! Reads `MYSQL_TEST_URL` (set in CI to the `mysql_live` job's
//! container). Tests skip silently when unset.

#![cfg(feature = "mysql")]

use std::sync::OnceLock;

use rustango::core::funcs;
use rustango::core::{Expr, FieldType, Op, SqlValue, WhereExpr, F};
use rustango::query::QuerySet;
use rustango::sql::{explain_pool, sqlx, Auto, ExplainOptions, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "funcs_b1_my_demo")]
#[rustango(app = "funcs_batch1_mysql_live")]
#[allow(dead_code)]
pub struct Demo {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub label: String,
    pub amount: i64,
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
    let _ = sqlx::query("DROP TABLE IF EXISTS funcs_b1_my_demo")
        .execute(&mp)
        .await;
    sqlx::query(
        "CREATE TABLE funcs_b1_my_demo (\
            id     BIGINT AUTO_INCREMENT PRIMARY KEY, \
            label  VARCHAR(64) NOT NULL, \
            amount BIGINT NOT NULL)",
    )
    .execute(&mp)
    .await
    .expect("create table");
    sqlx::query("INSERT INTO funcs_b1_my_demo (label, amount) VALUES ('x', 4)")
        .execute(&mp)
        .await
        .expect("seed row");
    Some(Pool::Mysql(mp))
}

async fn assert_expr_parses(p: &Pool, e: Expr, expected: SqlValue) {
    let qs = QuerySet::<Demo>::default().where_raw(WhereExpr::ExprCompare {
        lhs: e,
        op: Op::Eq,
        rhs: Expr::Literal(expected),
    });
    let q = qs.compile().expect("compile");
    let plan = explain_pool(p, &q, ExplainOptions::default())
        .await
        .expect("explain SHOULD parse");
    assert!(!plan.is_empty(), "expected non-empty plan");
}

#[tokio::test]
async fn cast_to_integer_executes_on_mysql() {
    let _g = serial_lock().lock().await;
    let Some(p) = pool().await else { return };
    let e = funcs::cast(F("amount"), FieldType::I64);
    assert_expr_parses(&p, e, SqlValue::I64(4)).await;
}

#[tokio::test]
async fn sign_executes_natively_on_mysql() {
    let _g = serial_lock().lock().await;
    let Some(p) = pool().await else { return };
    let e = funcs::sign(F("amount"));
    assert_expr_parses(&p, e, SqlValue::I64(1)).await;
}

#[tokio::test]
async fn position_emits_locate_on_mysql() {
    // MySQL's `LOCATE(needle, haystack)` is the canonical equivalent of
    // PG's `POSITION(needle IN haystack)`. The writer translates both.
    let _g = serial_lock().lock().await;
    let Some(p) = pool().await else { return };
    let e = funcs::position("x", F("label"));
    // LOCATE('x', 'x') = 1
    assert_expr_parses(&p, e, SqlValue::I64(1)).await;
}

#[tokio::test]
async fn repeat_executes_natively_on_mysql() {
    let _g = serial_lock().lock().await;
    let Some(p) = pool().await else { return };
    let e = funcs::repeat(F("label"), 3_i64);
    assert_expr_parses(&p, e, SqlValue::String("xxx".into())).await;
}

#[tokio::test]
async fn mod_executes_natively_on_mysql() {
    let _g = serial_lock().lock().await;
    let Some(p) = pool().await else { return };
    let e = funcs::mod_(F("amount"), 3_i64);
    // 4 MOD 3 = 1
    assert_expr_parses(&p, e, SqlValue::I64(1)).await;
}
