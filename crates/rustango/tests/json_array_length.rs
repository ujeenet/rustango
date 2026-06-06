//! Tri-dialect emission tests for `funcs::json_array_length` —
//! issue #826 (Eloquent `whereJsonLength` / Django `JSONField`
//! length-lookup parity).

use rustango::core::funcs::json_array_length;
use rustango::core::{Expr, Op, SelectQuery, SqlValue, WhereExpr, F};
use rustango::sql::{Dialect, MySql, Postgres, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "jal_doc")]
#[allow(dead_code)]
pub struct Doc {
    #[rustango(primary_key)]
    id: i64,
    data: serde_json::Value,
}

fn compile_with<D: Dialect>(d: D) -> String {
    let q = SelectQuery {
        where_clause: WhereExpr::ExprCompare {
            lhs: json_array_length(F("data")),
            op: Op::Gt,
            rhs: Expr::Literal(SqlValue::I64(0)),
        },
        ..SelectQuery::new(<Doc as rustango::core::Model>::SCHEMA)
    };
    d.compile_select(&q).unwrap().sql
}

#[test]
fn pg_emits_jsonb_array_length() {
    let sql = compile_with(Postgres);
    assert!(
        sql.contains(r#"jsonb_array_length("data")"#),
        "PG should emit jsonb_array_length, got: {sql}"
    );
    assert!(sql.contains(" > $1"), "got: {sql}");
}

#[test]
fn mysql_emits_json_length() {
    let sql = compile_with(MySql);
    assert!(
        sql.contains("JSON_LENGTH(`data`)"),
        "MySQL should emit JSON_LENGTH, got: {sql}"
    );
}

#[test]
fn sqlite_emits_json_array_length() {
    let sql = compile_with(Sqlite);
    assert!(
        sql.contains(r#"json_array_length("data")"#),
        "SQLite should emit json_array_length, got: {sql}"
    );
}
