//! JSON path lookups — closes #296 / T2.3.
//!
//! Per-dialect emission snapshots for `Expr::JsonPath` /
//! `funcs::json_path` / `funcs::json_path_indexed`. Same IR produces
//! per-dialect-correct SQL on PG / MySQL / SQLite:
//!
//!   PG     — `<source> -> 'k1' -> 'k2' ->> 'k3'`
//!   MySQL  — `JSON_UNQUOTE(JSON_EXTRACT(<source>, '$.k1.k2.k3'))`
//!   SQLite — `json_extract(<source>, '$.k1.k2.k3')`

use rustango::core::funcs::{json_path, json_path_indexed};
use rustango::core::{Expr, JsonPathStep, F};
use rustango::sql::{Dialect, MySql, Postgres, SqlError, Sqlite};

fn pg(e: &Expr) -> Result<String, SqlError> {
    write_for_test(&Postgres, e)
}

fn my(e: &Expr) -> Result<String, SqlError> {
    write_for_test(&MySql, e)
}

fn sqlite(e: &Expr) -> Result<String, SqlError> {
    write_for_test(&Sqlite, e)
}

fn write_for_test(dialect: &dyn Dialect, e: &Expr) -> Result<String, SqlError> {
    use rustango::core::{Op, SqlValue, WhereExpr};
    let qs = rustango::query::QuerySet::<NoModel>::default().where_raw(WhereExpr::ExprCompare {
        lhs: e.clone(),
        op: Op::Eq,
        rhs: Expr::Literal(SqlValue::Bool(true)),
    });
    let select = qs.compile().unwrap();
    Ok(dialect.compile_select(&select)?.sql)
}

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "json_path_demo")]
#[allow(dead_code)]
pub struct NoModel {
    #[rustango(primary_key)]
    id: i64,
    data: serde_json::Value,
}

// ---------- Single-key path ----------

#[test]
fn single_key_as_text_emits_double_arrow_on_pg() {
    let e = json_path(F("data"), &["city"], true);
    let pg = pg(&e).unwrap();
    assert!(pg.contains(r#""data" ->> $1"#), "PG `->>` form: {pg}");
}

#[test]
fn single_key_json_typed_emits_single_arrow_on_pg() {
    let e = json_path(F("data"), &["address"], false);
    let pg = pg(&e).unwrap();
    assert!(
        pg.contains(r#""data" -> $1"#),
        "PG `->` JSON-typed form: {pg}"
    );
    assert!(!pg.contains("->>"), "must not use `->>` form: {pg}");
}

#[test]
fn single_key_emits_json_extract_on_sqlite_and_mysql() {
    let e = json_path(F("data"), &["city"], true);
    let my = my(&e).unwrap();
    let lite = sqlite(&e).unwrap();
    assert!(
        my.contains("JSON_UNQUOTE(JSON_EXTRACT(`data`, '$.city'))"),
        "MySQL JSON_UNQUOTE form: {my}"
    );
    assert!(
        lite.contains(r#"json_extract("data", '$.city')"#),
        "SQLite json_extract: {lite}"
    );
}

// ---------- Multi-key chain ----------

#[test]
fn multi_key_chain_emits_arrow_chain_on_pg() {
    let e = json_path(F("data"), &["address", "city"], true);
    let pg = pg(&e).unwrap();
    // Last hop uses `->>`, prior hops use `->`.
    assert!(pg.contains(r#""data" -> $1 ->> $2"#), "PG chain: {pg}");
}

#[test]
fn multi_key_chain_emits_dotted_path_on_mysql_and_sqlite() {
    let e = json_path(F("data"), &["address", "city"], true);
    let my = my(&e).unwrap();
    let lite = sqlite(&e).unwrap();
    assert!(
        my.contains("JSON_UNQUOTE(JSON_EXTRACT(`data`, '$.address.city'))"),
        "MySQL: {my}"
    );
    assert!(
        lite.contains(r#"json_extract("data", '$.address.city')"#),
        "SQLite: {lite}"
    );
}

// ---------- Array index step ----------

#[test]
fn array_index_step_emits_per_dialect_form() {
    let e = json_path_indexed(
        F("data"),
        [
            JsonPathStep::Key("items".into()),
            JsonPathStep::Index(0),
            JsonPathStep::Key("name".into()),
        ],
        true,
    );
    let pg = pg(&e).unwrap();
    let my = my(&e).unwrap();
    let lite = sqlite(&e).unwrap();
    // PG: keys bind as params, indices inline.
    assert!(
        pg.contains(r#""data" -> $1 -> 0 ->> $2"#),
        "PG indexed: {pg}"
    );
    // MySQL/SQLite use `$.items[0].name`.
    assert!(my.contains("'$.items[0].name'"), "MySQL indexed: {my}");
    assert!(lite.contains("'$.items[0].name'"), "SQLite indexed: {lite}");
}

#[test]
fn negative_index_pg_and_sqlite() {
    let e = json_path_indexed(F("data"), [JsonPathStep::Index(-1)], false);
    // PG accepts negative indices natively.
    let pg = pg(&e).unwrap();
    assert!(pg.contains(r#""data" -> -1"#), "PG negative index: {pg}");
    // SQLite uses the `$[#-1]` from-the-end anchor (#1027).
    let sq = sqlite(&e).unwrap();
    assert!(sq.contains("[#-1]"), "SQLite from-end anchor: {sq}");
    // MySQL's `$[N]` grammar has no negative form — still rejected.
    assert!(matches!(
        my(&e).unwrap_err(),
        SqlError::OpNotSupportedInDialect { .. }
    ));
}

// ---------- Safety guards ----------

#[test]
fn unsafe_key_chars_rejected_on_every_dialect() {
    // Even on PG, where keys bind as parameters, we reject keys with
    // characters outside `[A-Za-z0-9_]` for consistency. The point is
    // to keep the MySQL/SQLite inline-path safe; uniform rejection
    // avoids the footgun of "works on PG, breaks on MySQL".
    let e = json_path(F("data"), &["bad'; DROP TABLE x;--"], true);
    for err in [
        pg(&e).unwrap_err(),
        my(&e).unwrap_err(),
        sqlite(&e).unwrap_err(),
    ] {
        assert!(matches!(err, SqlError::OpNotSupportedInDialect { .. }));
    }
}

#[test]
fn empty_path_is_rejected() {
    let e = json_path(F("data"), &[], false);
    for err in [
        pg(&e).unwrap_err(),
        my(&e).unwrap_err(),
        sqlite(&e).unwrap_err(),
    ] {
        assert!(matches!(err, SqlError::OpNotSupportedInDialect { .. }));
    }
}

// ---------- as_text noop on SQLite ----------

#[test]
fn as_text_is_a_noop_on_sqlite_json_extract_returns_scalars_unquoted() {
    let with_text = json_path(F("data"), &["city"], true);
    let without_text = json_path(F("data"), &["city"], false);
    let lite_text = sqlite(&with_text).unwrap();
    let lite_json = sqlite(&without_text).unwrap();
    assert_eq!(
        lite_text, lite_json,
        "SQLite emits identical SQL regardless of as_text"
    );
}
