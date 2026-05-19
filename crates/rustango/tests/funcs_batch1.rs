//! DB functions batch 1 — Cast / LPad / RPad / MD5 / SHA1 / SHA256 /
//! Position / Repeat / Reverse / Sign / Mod / Power / Sqrt. Closes
//! #266 / T1.4.
//!
//! Per-dialect emission snapshots — each test pins the SQL token shape
//! for each of PG / MySQL / SQLite. Where a backend genuinely lacks the
//! function (MD5/SHA1/SHA256/Reverse on SQLite) the writer must error
//! with `OpNotSupportedInDialect`, not silently emit wrong SQL.
//!
//! Live regression: covered by `funcs_batch1_sqlite_live.rs` (sqlite
//! tier) + the existing PG/MySQL live tests pick this up via the
//! `--all-features` `postgres_test` job.

use rustango::core::funcs;
use rustango::core::{Expr, FieldType, F};
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

/// Wraps the Expr in a `WHERE <expr> = TRUE` clause so the writer
/// renders it through `write_expr`. The compiled SQL contains the
/// dialect-specific token; tests assert on substring matches.
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
#[rustango(table = "funcs_batch1_demo")]
#[allow(dead_code)]
pub struct NoModel {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 80)]
    name: String,
    amount: i64,
}

// ---------- Cast ----------

#[test]
fn cast_emits_dialect_specific_type_token() {
    let e = funcs::cast(F("amount"), FieldType::I64);
    let pg = pg(&e).unwrap();
    let my = my(&e).unwrap();
    let lite = sqlite(&e).unwrap();
    // PG: standard `BIGINT`. MySQL: `SIGNED` (5.7+8.x portable).
    // SQLite: `INTEGER` (its affinity name).
    assert!(pg.contains(r#"CAST("amount" AS BIGINT)"#), "PG cast: {pg}");
    assert!(my.contains("CAST(`amount` AS SIGNED)"), "MySQL cast: {my}");
    assert!(
        lite.contains(r#"CAST("amount" AS INTEGER)"#),
        "SQLite cast: {lite}"
    );
}

#[test]
fn cast_to_json_errors_on_mysql() {
    let e = funcs::cast(F("name"), FieldType::Json);
    // MySQL has no `CAST AS JSON` form — must error.
    let err = my(&e).unwrap_err();
    assert!(matches!(
        err,
        SqlError::OpNotSupportedInDialect {
            dialect: "mysql",
            ..
        }
    ));
}

// ---------- LPad / RPad ----------

#[test]
fn lpad_native_on_pg_mysql_workaround_on_sqlite() {
    let e = funcs::lpad(F("name"), 10_i64, " ");
    let pg = pg(&e).unwrap();
    let my = my(&e).unwrap();
    let lite = sqlite(&e).unwrap();
    assert!(pg.contains(r#"LPAD("name""#), "PG LPAD: {pg}");
    assert!(my.contains("LPAD(`name`"), "MySQL LPAD: {my}");
    // SQLite workaround uses substr + replace(printf).
    assert!(
        lite.contains("substr(replace(printf"),
        "SQLite LPAD workaround: {lite}"
    );
}

#[test]
fn rpad_native_on_pg_mysql_workaround_on_sqlite() {
    let e = funcs::rpad(F("name"), 10_i64, " ");
    let pg = pg(&e).unwrap();
    let my = my(&e).unwrap();
    let lite = sqlite(&e).unwrap();
    assert!(pg.contains(r#"RPAD("name""#), "PG RPAD: {pg}");
    assert!(my.contains("RPAD(`name`"), "MySQL RPAD: {my}");
    // SQLite RPad concats string first then pads.
    assert!(
        lite.contains("substr(\"name\" || replace(printf"),
        "SQLite RPAD workaround: {lite}"
    );
}

// ---------- MD5 / SHA1 / SHA256 ----------

#[test]
fn md5_pg_native_mysql_native_sqlite_errors() {
    let e = funcs::md5(F("name"));
    assert!(pg(&e).unwrap().contains(r#"md5("name")"#));
    assert!(my(&e).unwrap().contains("MD5(`name`)"));
    let err = sqlite(&e).unwrap_err();
    assert!(
        matches!(
            err,
            SqlError::OpNotSupportedInDialect {
                dialect: "sqlite",
                ..
            }
        ),
        "expected SQLite error, got: {err:?}"
    );
}

#[test]
fn sha1_pg_pgcrypto_mysql_native_sqlite_errors() {
    let e = funcs::sha1(F("name"));
    assert!(pg(&e)
        .unwrap()
        .contains(r#"encode(digest("name", 'sha1'), 'hex')"#));
    assert!(my(&e).unwrap().contains("SHA1(`name`)"));
    assert!(sqlite(&e).is_err());
}

#[test]
fn sha256_pg_pgcrypto_mysql_sha2_sqlite_errors() {
    let e = funcs::sha256(F("name"));
    assert!(pg(&e)
        .unwrap()
        .contains(r#"encode(digest("name", 'sha256'), 'hex')"#));
    assert!(my(&e).unwrap().contains("SHA2(`name`, 256)"));
    assert!(sqlite(&e).is_err());
}

// ---------- Position ----------

#[test]
fn position_divergent_per_dialect() {
    let e = funcs::position("@", F("name"));
    let pg = pg(&e).unwrap();
    let my = my(&e).unwrap();
    let lite = sqlite(&e).unwrap();
    assert!(
        pg.contains(r#"POSITION($1 IN "name")"#),
        "PG POSITION: {pg}"
    );
    assert!(my.contains("LOCATE(?, `name`)"), "MySQL LOCATE: {my}");
    // SQLite INSTR swaps the argument order.
    assert!(lite.contains(r#"INSTR("name", ?)"#), "SQLite INSTR: {lite}");
}

// ---------- Repeat ----------

#[test]
fn repeat_native_on_pg_mysql_workaround_on_sqlite() {
    let e = funcs::repeat(F("name"), 3_i64);
    let pg = pg(&e).unwrap();
    let my = my(&e).unwrap();
    let lite = sqlite(&e).unwrap();
    assert!(pg.contains(r#"REPEAT("name""#), "PG REPEAT: {pg}");
    assert!(my.contains("REPEAT(`name`"), "MySQL REPEAT: {my}");
    assert!(
        lite.contains("replace(printf"),
        "SQLite REPEAT workaround: {lite}"
    );
}

// ---------- Reverse ----------

#[test]
fn reverse_pg_native_mysql_native_sqlite_errors() {
    let e = funcs::reverse(F("name"));
    assert!(pg(&e).unwrap().contains(r#"REVERSE("name")"#));
    assert!(my(&e).unwrap().contains("REVERSE(`name`)"));
    let err = sqlite(&e).unwrap_err();
    assert!(matches!(
        err,
        SqlError::OpNotSupportedInDialect {
            dialect: "sqlite",
            ..
        }
    ));
}

// ---------- Sign ----------

#[test]
fn sign_native_on_pg_mysql_case_expansion_on_sqlite() {
    let e = funcs::sign(F("amount"));
    let pg = pg(&e).unwrap();
    let my = my(&e).unwrap();
    let lite = sqlite(&e).unwrap();
    assert!(pg.contains(r#"SIGN("amount")"#), "PG SIGN: {pg}");
    assert!(my.contains("SIGN(`amount`)"), "MySQL SIGN: {my}");
    assert!(
        lite.contains("CASE WHEN \"amount\" > 0 THEN 1"),
        "SQLite SIGN CASE: {lite}"
    );
}

// ---------- Mod ----------

#[test]
fn mod_lowers_to_modulo_operator_uniformly() {
    let e = funcs::mod_(F("amount"), 100_i64);
    let pg = pg(&e).unwrap();
    let my = my(&e).unwrap();
    let lite = sqlite(&e).unwrap();
    assert!(pg.contains(r#"("amount" % $1)"#), "PG mod: {pg}");
    assert!(my.contains("(`amount` % ?)"), "MySQL mod: {my}");
    assert!(lite.contains(r#"("amount" % ?)"#), "SQLite mod: {lite}");
}

// ---------- Power ----------

#[test]
fn power_native_on_pg_mysql_sqlite_errors() {
    let e = funcs::power(F("amount"), 2_i64);
    let pg = pg(&e).unwrap();
    let my = my(&e).unwrap();
    assert!(pg.contains(r#"POWER("amount""#), "PG POWER: {pg}");
    assert!(my.contains("POWER(`amount`"), "MySQL POWER: {my}");
    // SQLite needs `SQLITE_ENABLE_MATH_FUNCTIONS` at build time; sqlx
    // doesn't enable it. Writer surfaces the limitation at emit time.
    let err = sqlite(&e).unwrap_err();
    assert!(matches!(
        err,
        SqlError::OpNotSupportedInDialect {
            dialect: "sqlite",
            ..
        }
    ));
}

// ---------- Sqrt ----------

#[test]
fn sqrt_native_on_pg_mysql_sqlite_errors() {
    let e = funcs::sqrt(F("amount"));
    assert!(pg(&e).unwrap().contains(r#"SQRT("amount")"#));
    assert!(my(&e).unwrap().contains("SQRT(`amount`)"));
    let err = sqlite(&e).unwrap_err();
    assert!(matches!(
        err,
        SqlError::OpNotSupportedInDialect {
            dialect: "sqlite",
            ..
        }
    ));
}
