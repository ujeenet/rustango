//! Tri-dialect emission tests for the date/time function DSL (issue
//! #3). Each backend spells these functions very differently — PG
//! uses the SQL-standard `EXTRACT(field FROM x)` + `DATE_TRUNC()`,
//! MySQL has per-field `YEAR()/MONTH()/...` + `DATE_FORMAT()`, and
//! SQLite has only `strftime()` + `date()`. The writer normalizes
//! the return type to integer for `Extract*` so cross-dialect code
//! gets the same value type back.

use rustango::core::funcs::{
    extract_day, extract_hour, extract_minute, extract_month, extract_quarter, extract_second,
    extract_week, extract_weekday, extract_year, now, trunc_date, trunc_day, trunc_month,
    trunc_year,
};
use rustango::core::{
    Assignment, Expr, Filter, Model as _, Op, SqlValue, UpdateQuery, WhereExpr, F,
};
use rustango::sql::{Dialect, MySql, Postgres, SqlError, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "evt")]
#[allow(dead_code)]
pub struct Evt {
    #[rustango(primary_key)]
    id: i64,
    created_at: chrono::DateTime<chrono::Utc>,
    year_out: i64,
}

fn update_set(value: Expr) -> UpdateQuery {
    UpdateQuery {
        model: Evt::SCHEMA,
        set: vec![Assignment {
            column: "year_out",
            value,
        }],
        where_clause: WhereExpr::Predicate(Filter {
            column: "id",
            op: Op::Eq,
            value: SqlValue::I64(1),
        }),
    }
}

// ---------- NOW() ----------

#[test]
fn now_emits_now_on_pg_mysql_current_timestamp_on_sqlite() {
    // NOW assigned to a timestamp column — use a fresh model where
    // the column is timestamp-typed. For dialect-emit testing the
    // column doesn't matter, only the SQL string.
    let q = update_set(now());
    let pg = Postgres.compile_update(&q).unwrap();
    assert!(pg.sql.contains("= NOW()"), "PG: {}", pg.sql);

    let my = MySql.compile_update(&q).unwrap();
    assert!(my.sql.contains("= NOW()"), "MySQL: {}", my.sql);

    let sq = Sqlite.compile_update(&q).unwrap();
    assert!(sq.sql.contains("= CURRENT_TIMESTAMP"), "SQLite: {}", sq.sql);
    // SQLite version emits the keyword without parens.
    assert!(!sq.sql.contains("CURRENT_TIMESTAMP("), "SQLite: {}", sq.sql);
}

#[test]
fn now_with_args_returns_arity_error() {
    let bad = Expr::Function {
        kind: rustango::core::ScalarFn::Now,
        args: vec![SqlValue::I64(0).into()],
    };
    let q = update_set(bad);
    let err = Postgres.compile_update(&q).unwrap_err();
    assert!(matches!(
        err,
        SqlError::FunctionArityMismatch {
            func: "NOW",
            expected: "0",
            got: 1
        }
    ));
}

// ---------- EXTRACT family (universal mapping) ----------

#[test]
fn pg_extract_year_emits_extract_from_with_int_cast() {
    let q = update_set(extract_year(F("created_at")));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql
            .contains(r#"CAST(EXTRACT(YEAR FROM "created_at") AS INTEGER)"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn mysql_extract_year_uses_direct_function() {
    let q = update_set(extract_year(F("created_at")));
    let stmt = MySql.compile_update(&q).unwrap();
    assert!(stmt.sql.contains("YEAR(`created_at`)"), "got: {}", stmt.sql);
}

#[test]
fn sqlite_extract_year_uses_strftime_with_int_cast() {
    let q = update_set(extract_year(F("created_at")));
    let stmt = Sqlite.compile_update(&q).unwrap();
    assert!(
        stmt.sql
            .contains(r#"CAST(strftime('%Y', "created_at") AS INTEGER)"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn extract_month_day_hour_minute_second_week_all_dialects() {
    // Spot-check the table maps each field correctly. Walk the 6
    // straight-shot variants in one test so the file stays terse.
    for (builder, pg_field, mysql_fn, sqlite_token) in [
        (extract_month as fn(_) -> Expr, "MONTH", "MONTH", "%m"),
        (extract_day, "DAY", "DAY", "%d"),
        (extract_hour, "HOUR", "HOUR", "%H"),
        (extract_minute, "MINUTE", "MINUTE", "%M"),
        (extract_second, "SECOND", "SECOND", "%S"),
        (extract_week, "WEEK", "WEEK", "%W"),
    ] {
        let q = update_set(builder(F("created_at")));
        assert!(Postgres
            .compile_update(&q)
            .unwrap()
            .sql
            .contains(&format!("EXTRACT({pg_field} FROM")));
        assert!(MySql
            .compile_update(&q)
            .unwrap()
            .sql
            .contains(&format!("{mysql_fn}(`created_at`)")));
        assert!(Sqlite
            .compile_update(&q)
            .unwrap()
            .sql
            .contains(&format!("strftime('{sqlite_token}'")));
    }
}

// ---------- ExtractWeekDay — normalized to 0 = Sunday everywhere ----------

#[test]
fn pg_extract_weekday_uses_dow_with_int_cast() {
    let q = update_set(extract_weekday(F("created_at")));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql
            .contains(r#"CAST(EXTRACT(DOW FROM "created_at") AS INTEGER)"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn mysql_extract_weekday_subtracts_one_from_dayofweek() {
    // MySQL DAYOFWEEK() is 1=Sun..7=Sat. We normalize to PG's 0=Sun
    // convention by subtracting 1.
    let q = update_set(extract_weekday(F("created_at")));
    let stmt = MySql.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains("(DAYOFWEEK(`created_at`) - 1)"),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn sqlite_extract_weekday_uses_strftime_w() {
    // SQLite strftime('%w') already returns 0=Sun..6=Sat.
    let q = update_set(extract_weekday(F("created_at")));
    let stmt = Sqlite.compile_update(&q).unwrap();
    assert!(
        stmt.sql
            .contains(r#"CAST(strftime('%w', "created_at") AS INTEGER)"#),
        "got: {}",
        stmt.sql
    );
}

// ---------- ExtractQuarter — PG/MySQL work, SQLite errors ----------

#[test]
fn pg_mysql_extract_quarter_works() {
    let q = update_set(extract_quarter(F("created_at")));
    assert!(Postgres
        .compile_update(&q)
        .unwrap()
        .sql
        .contains("EXTRACT(QUARTER FROM"));
    assert!(MySql
        .compile_update(&q)
        .unwrap()
        .sql
        .contains("QUARTER(`created_at`)"));
}

#[test]
fn sqlite_extract_quarter_synthesizes_from_month() {
    // #1037 — SQLite has no quarter token in strftime, so quarter is
    // synthesized from the month: ((month + 2) / 3).
    let q = update_set(extract_quarter(F("created_at")));
    let sql = Sqlite.compile_update(&q).unwrap().sql;
    assert!(
        sql.contains("strftime('%m'") && sql.contains("+ 2) / 3"),
        "expected month-synthesis for quarter, got {sql}",
    );
}

// ---------- TruncDate — same SQL across all dialects ----------

#[test]
#[allow(non_snake_case)] // SQL keyword "DATE" preserved in test name for readability.
fn trunc_date_emits_DATE_call_on_all_dialects() {
    let q = update_set(trunc_date(F("created_at")));
    for dialect in ["pg", "my", "sq"] {
        let stmt = match dialect {
            "pg" => Postgres.compile_update(&q).unwrap(),
            "my" => MySql.compile_update(&q).unwrap(),
            _ => Sqlite.compile_update(&q).unwrap(),
        };
        // PG/SQLite quote with "; MySQL with `. Match the function
        // name + the open paren only.
        assert!(stmt.sql.contains("DATE("), "{dialect}: {}", stmt.sql);
    }
}

// ---------- Trunc family — diverges most ----------

#[test]
fn pg_trunc_year_emits_date_trunc_with_year_unit() {
    let q = update_set(trunc_year(F("created_at")));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"DATE_TRUNC('year', "created_at")"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn mysql_trunc_year_emits_date_format_with_year_template() {
    let q = update_set(trunc_year(F("created_at")));
    let stmt = MySql.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains("DATE_FORMAT(`created_at`, '%Y-01-01')"),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn sqlite_trunc_year_emits_strftime_with_year_template() {
    let q = update_set(trunc_year(F("created_at")));
    let stmt = Sqlite.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"strftime('%Y-01-01', "created_at")"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn pg_trunc_month_emits_date_trunc_with_month_unit() {
    let q = update_set(trunc_month(F("created_at")));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(stmt.sql.contains(r#"DATE_TRUNC('month', "created_at")"#));
}

#[test]
fn mysql_trunc_month_emits_date_format_with_month_template() {
    let q = update_set(trunc_month(F("created_at")));
    let stmt = MySql.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains("DATE_FORMAT(`created_at`, '%Y-%m-01')"),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn sqlite_trunc_month_emits_strftime_with_month_template() {
    let q = update_set(trunc_month(F("created_at")));
    let stmt = Sqlite.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"strftime('%Y-%m-01', "created_at")"#),
        "got: {}",
        stmt.sql
    );
}

#[test]
fn pg_trunc_day_emits_date_trunc() {
    let q = update_set(trunc_day(F("created_at")));
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(stmt.sql.contains(r#"DATE_TRUNC('day', "created_at")"#));
}

#[test]
fn mysql_trunc_day_collapses_to_date_function() {
    // TruncDay on MySQL is identical to DATE(x) — no need for the
    // verbose DATE_FORMAT shape. Same precision, cleaner emission.
    let q = update_set(trunc_day(F("created_at")));
    let stmt = MySql.compile_update(&q).unwrap();
    assert!(stmt.sql.contains("DATE(`created_at`)"), "got: {}", stmt.sql);
    assert!(
        !stmt.sql.contains("DATE_FORMAT"),
        "TruncDay should NOT use DATE_FORMAT on MySQL, got: {}",
        stmt.sql
    );
}

#[test]
fn sqlite_trunc_day_collapses_to_date_function() {
    let q = update_set(trunc_day(F("created_at")));
    let stmt = Sqlite.compile_update(&q).unwrap();
    assert!(
        stmt.sql.contains(r#"date("created_at")"#),
        "got: {}",
        stmt.sql
    );
}

// ---------- Composition with F() arithmetic ----------

#[test]
#[allow(non_snake_case)] // `F` preserved in test name to mirror the builder it tests (`F("created_at")`).
fn extract_year_of_F_column_composes_with_other_funcs() {
    use rustango::core::funcs::{abs, greatest};
    // greatest(extract_year(F("created_at")), 2020)
    let nested = greatest([extract_year(F("created_at")), 2020_i64.into()]);
    let q = update_set(nested);
    let stmt = Postgres.compile_update(&q).unwrap();
    assert!(
        stmt.sql
            .contains(r#"GREATEST(CAST(EXTRACT(YEAR FROM "created_at") AS INTEGER), $1)"#),
        "got: {}",
        stmt.sql
    );

    // Smoke check: abs(extract_year(F("created_at"))) — should also
    // emit cleanly (abs takes any expr, extract returns int).
    let _q = update_set(abs(extract_year(F("created_at"))));
    let _stmt = Postgres.compile_update(&_q).unwrap();
}
