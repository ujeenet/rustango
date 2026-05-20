//! DB functions batch 2 — Log / LogWithBase / Exp / Pi / Random /
//! MakeInterval / Age / TruncWithTz. Closes #294 / T2.7.
//!
//! Per-dialect emission snapshots, plus negative tests pinning the
//! `OpNotSupportedInDialect` error path where a backend genuinely
//! lacks the function (Log/Exp on default-build SQLite, MakeInterval
//! on MySQL/SQLite).

use rustango::core::funcs;
use rustango::core::{Expr, F};
use rustango::sql::{Dialect, MySql, Postgres, SqlError, Sqlite};

fn pg(e: &Expr) -> Result<String, SqlError> {
    write_expr_for_test(&Postgres, e)
}

fn my(e: &Expr) -> Result<String, SqlError> {
    write_expr_for_test(&MySql, e)
}

fn sqlite(e: &Expr) -> Result<String, SqlError> {
    write_expr_for_test(&Sqlite, e)
}

fn write_expr_for_test(dialect: &dyn Dialect, e: &Expr) -> Result<String, SqlError> {
    use rustango::core::{Op, WhereExpr};
    let qs = rustango::query::QuerySet::<NoModel>::default().where_raw(WhereExpr::ExprCompare {
        lhs: e.clone(),
        op: Op::Eq,
        rhs: Expr::Literal(rustango::core::SqlValue::Bool(true)),
    });
    let select = qs.compile().unwrap();
    Ok(dialect.compile_select(&select)?.sql)
}

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "fb2_demo")]
#[allow(dead_code)]
pub struct NoModel {
    #[rustango(primary_key)]
    id: i64,
    amount: i64,
    ts: chrono::DateTime<chrono::Utc>,
}

// ---------- Log / LogWithBase / Exp ----------

#[test]
fn log_emits_ln_on_pg_and_mysql_errors_on_sqlite() {
    let e = funcs::log(F("amount"));
    assert!(pg(&e).unwrap().contains(r#"LN("amount")"#));
    assert!(my(&e).unwrap().contains("LN(`amount`)"));
    let err = sqlite(&e).unwrap_err();
    assert!(matches!(
        err,
        SqlError::OpNotSupportedInDialect {
            dialect: "sqlite",
            ..
        }
    ));
}

#[test]
fn log_with_base_emits_log_base_x_form_on_pg_and_mysql() {
    let e = funcs::log_with_base(10_i64, F("amount"));
    assert!(
        pg(&e).unwrap().contains(r#"LOG($1, "amount")"#),
        "PG log: {}",
        pg(&e).unwrap()
    );
    assert!(my(&e).unwrap().contains("LOG(?, `amount`)"));
    assert!(matches!(
        sqlite(&e).unwrap_err(),
        SqlError::OpNotSupportedInDialect {
            dialect: "sqlite",
            ..
        }
    ));
}

#[test]
fn exp_native_on_pg_and_mysql_errors_on_sqlite() {
    let e = funcs::exp(F("amount"));
    assert!(pg(&e).unwrap().contains(r#"EXP("amount")"#));
    assert!(my(&e).unwrap().contains("EXP(`amount`)"));
    assert!(matches!(
        sqlite(&e).unwrap_err(),
        SqlError::OpNotSupportedInDialect {
            dialect: "sqlite",
            ..
        }
    ));
}

// ---------- Pi / Random ----------

#[test]
fn pi_native_on_pg_and_mysql_inline_literal_on_sqlite() {
    let e = funcs::pi();
    assert!(pg(&e).unwrap().contains("PI()"));
    assert!(my(&e).unwrap().contains("PI()"));
    // SQLite has no native pi() — inline the constant.
    let lite = sqlite(&e).unwrap();
    assert!(
        lite.contains("3.141592653589793"),
        "SQLite pi inline: {lite}"
    );
    assert!(!lite.contains("PI()"), "SQLite must not emit PI(): {lite}");
}

#[test]
fn random_emits_native_per_dialect() {
    let e = funcs::random();
    assert!(pg(&e).unwrap().contains("random()"));
    assert!(my(&e).unwrap().contains("RAND()"));
    assert!(sqlite(&e).unwrap().contains("random()"));
}

// ---------- MakeInterval ----------

#[test]
fn make_interval_is_pg_only() {
    let e = funcs::make_interval(1_i64, 0_i64, 0_i64, 0_i64, 0_i64, 0_i64);
    let pg = pg(&e).unwrap();
    assert!(
        pg.contains("make_interval(years => $1"),
        "PG keyword form: {pg}"
    );
    // MySQL and SQLite must error.
    for err_dialect in [my(&e).unwrap_err(), sqlite(&e).unwrap_err()] {
        assert!(matches!(
            err_dialect,
            SqlError::OpNotSupportedInDialect { .. }
        ));
    }
}

// ---------- Age ----------

#[test]
fn age_pg_emits_interval_mysql_emits_seconds_sqlite_emits_julianday() {
    let e = funcs::age(F("ts"), F("ts"));
    let pg = pg(&e).unwrap();
    let my = my(&e).unwrap();
    let lite = sqlite(&e).unwrap();
    assert!(pg.contains(r#"age("ts", "ts")"#), "PG: {pg}");
    // MySQL swaps args so the result is ts1 - ts2.
    assert!(
        my.contains("TIMESTAMPDIFF(SECOND, `ts`, `ts`)"),
        "MySQL: {my}"
    );
    assert!(
        lite.contains("julianday(\"ts\")") && lite.contains("* 86400.0"),
        "SQLite julianday form: {lite}"
    );
}

// ---------- TruncWithTz ----------

#[test]
fn trunc_with_tz_pg_uses_date_trunc_at_time_zone() {
    let e = funcs::trunc_with_tz(F("ts"), "day", "America/New_York");
    let pg = pg(&e).unwrap();
    assert!(
        pg.contains(r#"date_trunc('day', "ts" AT TIME ZONE 'America/New_York')"#),
        "PG: {pg}"
    );
}

#[test]
fn trunc_with_tz_mysql_uses_convert_tz_and_date_format() {
    let e = funcs::trunc_with_tz(F("ts"), "hour", "America/New_York");
    let my = my(&e).unwrap();
    assert!(
        my.contains("CONVERT_TZ(`ts`, '+00:00', 'America/New_York')"),
        "MySQL CONVERT_TZ: {my}"
    );
    assert!(
        my.contains("DATE_FORMAT(") && my.contains("%Y-%m-%d %H:00:00"),
        "MySQL DATE_FORMAT mask: {my}"
    );
}

#[test]
fn trunc_with_tz_sqlite_uses_strftime_with_modifier() {
    let e = funcs::trunc_with_tz(F("ts"), "day", "+00:00");
    let lite = sqlite(&e).unwrap();
    assert!(
        lite.contains("strftime('%Y-%m-%d 00:00:00', \"ts\", '+00:00')"),
        "SQLite strftime: {lite}"
    );
}

#[test]
fn trunc_with_tz_rejects_bogus_unit() {
    let e = funcs::trunc_with_tz(F("ts"), "millennium", "UTC");
    assert!(matches!(
        pg(&e).unwrap_err(),
        SqlError::OpNotSupportedInDialect { .. }
    ));
}

#[test]
fn trunc_with_tz_rejects_unsafe_tz_chars() {
    let e = funcs::trunc_with_tz(F("ts"), "day", "UTC'; DROP TABLE x;--");
    assert!(matches!(
        pg(&e).unwrap_err(),
        SqlError::OpNotSupportedInDialect { .. }
    ));
}
