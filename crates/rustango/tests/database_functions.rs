//! Tri-dialect emission tests for the database functions DSL (issue
//! #2). Pins the SQL each backend emits for every function in the v1
//! set and the divergent shapes:
//!
//! - `Concat` → `CONCAT(a, b)` on PG/MySQL, `(a || b)` on SQLite.
//! - `Substr` → `SUBSTRING(s FROM start FOR len)` on PG,
//!   `SUBSTRING(s, start, len)` on MySQL, `SUBSTR(...)` on SQLite.
//! - `Greatest` / `Least` → `MAX` / `MIN` scalar on SQLite, native
//!   keyword on PG / MySQL.

use rustango::core::funcs::{
    abs, ceil, coalesce, concat, floor, greatest, least, length, lower, ltrim, nullif, replace,
    round, round_to, rtrim, substr, trim, upper,
};
use rustango::core::{
    Assignment, ColumnFilter, Expr, Filter, Model as _, Op, SqlValue, UpdateQuery, WhereExpr, F,
};
use rustango::sql::{Dialect, MySql, Postgres, SqlError, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "fnq")]
#[allow(dead_code)]
pub struct Row {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    name: String,
    score: i64,
}

// ---------- Helper: assert UPDATE SET emits an expr literally ----------

fn update_set_expr(value: Expr) -> UpdateQuery {
    UpdateQuery {
        model: Row::SCHEMA,
        set: vec![Assignment {
            column: "name",
            value,
        }],
        where_clause: WhereExpr::Predicate(Filter {
            column: "id",
            op: Op::Eq,
            value: SqlValue::I64(1),
        }),
    }
}

// ---------- Text: simple unary ----------

#[test]
fn lower_emits_lower_fn_in_all_dialects() {
    let q = update_set_expr(lower(F("name")));
    assert!(Postgres
        .compile_update(&q)
        .unwrap()
        .sql
        .contains(r#"SET "name" = LOWER("name")"#));
    assert!(MySql
        .compile_update(&q)
        .unwrap()
        .sql
        .contains("SET `name` = LOWER(`name`)"));
    assert!(Sqlite
        .compile_update(&q)
        .unwrap()
        .sql
        .contains(r#"SET "name" = LOWER("name")"#));
}

#[test]
fn upper_emits_upper_fn() {
    let q = update_set_expr(upper(F("name")));
    assert!(Postgres
        .compile_update(&q)
        .unwrap()
        .sql
        .contains(r#"= UPPER("name")"#));
}

#[test]
fn length_emits_length_fn() {
    let q = update_set_expr(length(F("name")));
    assert!(Postgres
        .compile_update(&q)
        .unwrap()
        .sql
        .contains(r#"= LENGTH("name")"#));
}

#[test]
fn trim_variants() {
    let pg = Postgres;
    let q1 = update_set_expr(trim(F("name")));
    let q2 = update_set_expr(ltrim(F("name")));
    let q3 = update_set_expr(rtrim(F("name")));
    assert!(pg.compile_update(&q1).unwrap().sql.contains("TRIM("));
    assert!(pg.compile_update(&q2).unwrap().sql.contains("LTRIM("));
    assert!(pg.compile_update(&q3).unwrap().sql.contains("RTRIM("));
}

// ---------- Concat: PG/MySQL CONCAT vs SQLite || ----------

#[test]
fn pg_concat_emits_native_call() {
    let q = update_set_expr(concat([F("name").into(), " ".into(), F("name").into()]));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"= CONCAT("name", $1, "name")"#),
        "got: {}",
        stmt.sql
    );
    assert_eq!(
        stmt.params,
        vec![SqlValue::String(" ".into()), SqlValue::I64(1)]
    );
}

#[test]
fn mysql_concat_emits_native_call() {
    let q = update_set_expr(concat([F("name").into(), " ".into(), F("name").into()]));
    let stmt = MySql.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains("= CONCAT(`name`, ?, `name`)"),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn sqlite_concat_falls_back_to_double_pipe() {
    let q = update_set_expr(concat([F("name").into(), " ".into(), F("name").into()]));
    let stmt = Sqlite.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"= ("name" || ? || "name")"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn empty_concat_returns_arity_error() {
    let q = update_set_expr(Expr::Function {
        kind: rustango::core::ScalarFn::Concat,
        args: vec![],
    });
    let err = Postgres.compile_update(&q).unwrap_err();
    assert!(
        matches!(err, SqlError::FunctionArityMismatch { func: "CONCAT", .. }),
        "expected arity error, got {err:?}"
    );
}

// ---------- Substr: PG FROM…FOR vs MySQL/SQLite commas ----------

#[test]
fn pg_substr_emits_from_for_form() {
    let q = update_set_expr(substr(F("name"), 1_i64, 5_i64));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"= SUBSTRING("name" FROM $1 FOR $2)"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn mysql_substr_emits_comma_form_with_substring() {
    let q = update_set_expr(substr(F("name"), 1_i64, 5_i64));
    let stmt = MySql.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains("= SUBSTRING(`name`, ?, ?)"),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn sqlite_substr_emits_comma_form_with_substr() {
    let q = update_set_expr(substr(F("name"), 1_i64, 5_i64));
    let stmt = Sqlite.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"= SUBSTR("name", ?, ?)"#),
        "got: {}",
        stmt.sql
    );
}

// ---------- Replace ----------

#[test]
fn replace_emits_3_arg_call() {
    let q = update_set_expr(replace(F("name"), "foo", "bar"));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"= REPLACE("name", $1, $2)"#),
        "got: {}",
        stmt.sql
    );
    assert_eq!(
        stmt.params,
        vec![
            SqlValue::String("foo".into()),
            SqlValue::String("bar".into()),
            SqlValue::I64(1),
        ],
    );
}

// ---------- Math ----------

#[test]
fn abs_emits_abs_fn() {
    let q = update_set_expr(abs(F("score")));
    assert!(Postgres
        .compile_update(&q)
        .unwrap()
        .sql
        .contains(r#"= ABS("score")"#));
}

#[test]
fn ceil_floor_emit_correct_tokens() {
    let q1 = update_set_expr(ceil(F("score")));
    let q2 = update_set_expr(floor(F("score")));
    assert!(Postgres.compile_update(&q1).unwrap().sql.contains("CEIL("));
    assert!(Postgres.compile_update(&q2).unwrap().sql.contains("FLOOR("));
}

#[test]
fn round_one_arg_works() {
    let q = update_set_expr(round(F("score")));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"= ROUND("score")"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn round_two_arg_works() {
    let q = update_set_expr(round_to(F("score"), 2_i32));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"= ROUND("score", $1)"#),
        "got: {}",
        stmt.sql
    );
}

// ---------- Coalesce / Greatest / Least / NullIf ----------

#[test]
fn coalesce_emits_all_args() {
    let q = update_set_expr(coalesce([F("name").into(), "fallback".into()]));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"= COALESCE("name", $1)"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn greatest_on_pg_mysql_emits_native_keyword() {
    let q = update_set_expr(greatest([F("score").into(), 5_i64.into(), 10_i64.into()]));
    assert!(Postgres
        .compile_update(&q)
        .unwrap()
        .sql
        .contains("GREATEST("));
    assert!(MySql.compile_update(&q).unwrap().sql.contains("GREATEST("));
}

#[test]
fn greatest_on_sqlite_falls_back_to_max_scalar() {
    let q = update_set_expr(greatest([F("score").into(), 5_i64.into(), 10_i64.into()]));
    let stmt = Sqlite.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"= MAX("score", ?, ?)"#),
        "SQLite should use scalar MAX, got: {}",
        stmt.sql
    );
}

#[test]
fn least_on_sqlite_falls_back_to_min_scalar() {
    let q = update_set_expr(least([F("score").into(), 5_i64.into()]));
    let stmt = Sqlite.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"= MIN("score", ?)"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn nullif_emits_2_arg_call() {
    let q = update_set_expr(nullif(F("name"), ""));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"= NULLIF("name", $1)"#),
        "got: {}",
        stmt.sql
    );
}

// ---------- Arity error paths ----------

#[test]
fn substr_with_wrong_arity_errors() {
    let q = update_set_expr(Expr::Function {
        kind: rustango::core::ScalarFn::Substr,
        args: vec![F("name").into(), 1_i64.into()], // only 2 args
    });
    let err = Postgres.compile_update(&q).unwrap_err();
    assert!(matches!(
        err,
        SqlError::FunctionArityMismatch {
            func: "SUBSTRING",
            ..
        }
    ));
}

#[test]
fn nullif_with_wrong_arity_errors() {
    let q = update_set_expr(Expr::Function {
        kind: rustango::core::ScalarFn::NullIf,
        args: vec![F("name").into()], // only 1
    });
    let err = Postgres.compile_update(&q).unwrap_err();
    assert!(matches!(
        err,
        SqlError::FunctionArityMismatch { func: "NULLIF", .. }
    ));
}

// ---------- Composition ----------

#[test]
fn functions_compose_with_arithmetic() {
    // upper(concat([F("name"), " ", F("name")])) + literal score
    // → wrapped properly with the BinOp parens.
    let inner = upper(concat([F("name").into(), " ".into(), F("name").into()]));
    let q = update_set_expr(inner);
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"= UPPER(CONCAT("name", $1, "name"))"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn function_in_where_column_compare() {
    // Build a SELECT … WHERE name = LOWER("other_col"). Use the
    // QuerySet helper to get a SelectQuery, swap the where clause.
    let mut q = Row::objects().compile().unwrap();
    q.where_clause = WhereExpr::ColumnCompare(ColumnFilter {
        column: "name",
        op: Op::Eq,
        rhs: lower(F("name")),
    });
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"WHERE "name" = LOWER("name")"#),
        "got: {}",
        stmt.sql
    );
}

// ---------- Schema validation: column refs inside function args ----------

// ---------- Arity enforcement on unary functions (post-review fix A) ----------
//
// The public builder API (`lower(arg)`, `upper(arg)`, …) is type-
// locked to one argument, but the underlying `Expr::Function` variant
// is permissive — anyone constructing the IR by hand (proc-macro
// codegen, future feature combos) could pass 0 or 2+ args. The writer
// catches that with `FunctionArityMismatch` rather than emitting
// malformed SQL.

#[test]
fn unary_function_with_zero_args_errors() {
    for (name, kind) in [
        ("LOWER", rustango::core::ScalarFn::Lower),
        ("UPPER", rustango::core::ScalarFn::Upper),
        ("LENGTH", rustango::core::ScalarFn::Length),
        ("TRIM", rustango::core::ScalarFn::Trim),
        ("LTRIM", rustango::core::ScalarFn::LTrim),
        ("RTRIM", rustango::core::ScalarFn::RTrim),
        ("ABS", rustango::core::ScalarFn::Abs),
        ("CEIL", rustango::core::ScalarFn::Ceil),
        ("FLOOR", rustango::core::ScalarFn::Floor),
    ] {
        let q = update_set_expr(Expr::Function { kind, args: vec![] });
        let err = Postgres.compile_update(&q).unwrap_err();
        assert!(
            matches!(err, SqlError::FunctionArityMismatch { func, expected: "1", got: 0 } if func == name),
            "expected arity-1 error for {name}, got {err:?}",
        );
    }
}

#[test]
fn unary_function_with_two_args_errors() {
    let q = update_set_expr(Expr::Function {
        kind: rustango::core::ScalarFn::Lower,
        args: vec![F("name").into(), F("name").into()],
    });
    let err = Postgres.compile_update(&q).unwrap_err();
    assert!(
        matches!(
            err,
            SqlError::FunctionArityMismatch {
                func: "LOWER",
                expected: "1",
                got: 2
            }
        ),
        "expected arity error, got {err:?}",
    );
}

// ---------- SQLite Greatest/Least with 1 arg (post-review fix B) ----------
//
// SQLite's `MAX`/`MIN` are overloaded: 2+ args is the scalar form
// (semantically equivalent to PG `GREATEST`/`LEAST`); a single arg
// switches to the aggregate form, which is wrong in `UPDATE SET` and
// in non-aggregating `WHERE` predicates. The writer rejects the 1-arg
// SQLite case rather than emit `MAX(x)` and surprise the user with an
// aggregate-misuse error at execution time.

#[test]
fn sqlite_greatest_with_one_arg_returns_op_not_supported() {
    let q = update_set_expr(greatest([F("score").into()]));
    let err = Sqlite.compile_update(&q).unwrap_err();
    assert!(
        matches!(
            err,
            SqlError::OpNotSupportedInDialect {
                dialect: "sqlite",
                ..
            }
        ),
        "expected OpNotSupportedInDialect on SQLite, got {err:?}",
    );
}

#[test]
fn sqlite_least_with_one_arg_returns_op_not_supported() {
    let q = update_set_expr(least([F("score").into()]));
    let err = Sqlite.compile_update(&q).unwrap_err();
    assert!(
        matches!(
            err,
            SqlError::OpNotSupportedInDialect {
                dialect: "sqlite",
                ..
            }
        ),
        "expected OpNotSupportedInDialect on SQLite, got {err:?}",
    );
}

#[test]
fn pg_mysql_greatest_with_one_arg_still_emits() {
    // PG/MySQL treat `GREATEST(x)` as a legal no-op returning x. Only
    // SQLite has the aggregate collision — keep the cross-dialect
    // behaviour asymmetric only where the underlying engine forces it.
    let q = update_set_expr(greatest([F("score").into()]));
    assert!(Postgres.compile_update(&q).is_ok());
    assert!(MySql.compile_update(&q).is_ok());
}

#[test]
fn unknown_column_inside_function_is_caught_at_compile() {
    let err = Row::objects()
        .update()
        .set_expr("name", upper(F("nope_nope")))
        .compile()
        .unwrap_err();
    assert!(
        matches!(err, rustango::core::QueryError::UnknownField { ref field, .. }
                 if field == "nope_nope"),
        "expected UnknownField for F() arg inside function, got: {err:?}"
    );
}
