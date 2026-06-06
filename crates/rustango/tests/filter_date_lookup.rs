//! Tri-dialect emission tests for the Django `__year` / `__month` /
//! `__day` / `__date` / `__week_day` / `__hour` / `__minute` /
//! `__second` / `__quarter` / `__week` field-lookup transforms on
//! `.filter()` — issue #829.
//!
//! The parser dispatches to the existing `Extract*` / `TruncDate`
//! scalar fns (already tri-dialect via `ScalarFn` and the writer
//! emitters); these tests pin the rendered SQL on PG / MySQL / SQLite
//! so the lookup-parser layer stays a thin wrapper over the existing
//! `Expr::Function` emission.

use chrono::{DateTime, NaiveDate, Utc};
use rustango::core::SqlValue;
use rustango::sql::{Dialect, MySql, Postgres, Sqlite};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "fdl_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    title: String,
    created: DateTime<Utc>,
    published_on: Option<NaiveDate>,
}

// ---------- __year (extract year) ----------

#[test]
fn year_lookup_emits_extract_year_eq() {
    let qs = Post::objects().filter("created__year", 2026_i64);
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(
        stmt.sql.contains(r#"EXTRACT(YEAR FROM "created")"#),
        "PG year lookup: {}",
        stmt.sql
    );
    assert!(stmt.sql.contains(" = $1"), "year=$1: {}", stmt.sql);
    assert_eq!(stmt.params, vec![SqlValue::I64(2026)]);
}

#[test]
fn year_lookup_emits_extract_year_on_mysql() {
    let qs = Post::objects().filter("created__year", 2026_i64);
    let stmt = MySql.compile_select(&qs.compile().unwrap()).unwrap();
    // MySQL: dedicated `YEAR(x)` shortcut (writer optimization over EXTRACT).
    assert!(
        stmt.sql.contains("YEAR(`created`)"),
        "MySQL year: {}",
        stmt.sql
    );
    assert!(stmt.sql.contains(" = ?"), "mysql placeholder: {}", stmt.sql);
}

#[test]
fn year_lookup_emits_strftime_on_sqlite() {
    let qs = Post::objects().filter("created__year", 2026_i64);
    let stmt = Sqlite.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(
        stmt.sql.contains("strftime('%Y', \"created\")"),
        "sqlite year: {}",
        stmt.sql
    );
}

// ---------- __year__gte (compose with comparison op) ----------

#[test]
fn year_lookup_supports_trailing_comparison() {
    for (suffix, op_sql) in [
        ("gt", " > "),
        ("gte", " >= "),
        ("lt", " < "),
        ("lte", " <= "),
        ("ne", " <> "),
        ("exact", " = "),
    ] {
        let qs = Post::objects().filter(&format!("created__year__{suffix}"), 2026_i64);
        let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
        assert!(
            stmt.sql.contains(r#"EXTRACT(YEAR FROM "created")"#),
            "year__{suffix} should still wrap LHS in EXTRACT: {}",
            stmt.sql
        );
        assert!(
            stmt.sql.contains(op_sql),
            "year__{suffix} should emit{op_sql}: {}",
            stmt.sql
        );
    }
}

// ---------- __month / __day / __hour / __minute / __second / __quarter ----------

#[test]
fn other_extract_lookups_emit_correct_token_pg() {
    for (suffix, token) in [
        ("month", "MONTH"),
        ("day", "DAY"),
        ("hour", "HOUR"),
        ("minute", "MINUTE"),
        ("second", "SECOND"),
        ("quarter", "QUARTER"),
        ("week", "WEEK"),
    ] {
        let qs = Post::objects().filter(&format!("created__{suffix}"), 1_i64);
        let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
        assert!(
            stmt.sql
                .contains(&format!(r#"EXTRACT({token} FROM "created")"#)),
            "PG __{suffix} should EXTRACT({token}): {}",
            stmt.sql
        );
    }
}

// ---------- __week_day (normalized 0=Sun..6=Sat across dialects) ----------

#[test]
fn week_day_lookup_emits_extract_dow_pg() {
    let qs = Post::objects().filter("created__week_day", 1_i64);
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    // PG: EXTRACT(DOW FROM x) — already 0=Sun..6=Sat
    assert!(
        stmt.sql.contains(r#"EXTRACT(DOW FROM "created")"#),
        "PG week_day: {}",
        stmt.sql
    );
}

#[test]
fn week_day_lookup_normalizes_on_mysql() {
    let qs = Post::objects().filter("created__week_day", 1_i64);
    let stmt = MySql.compile_select(&qs.compile().unwrap()).unwrap();
    // MySQL DAYOFWEEK is 1=Sun..7=Sat; writer subtracts 1 to align
    assert!(
        stmt.sql.contains("DAYOFWEEK(`created`)"),
        "MySQL week_day: {}",
        stmt.sql
    );
    assert!(stmt.sql.contains("- 1"), "MySQL normalize -1: {}", stmt.sql);
}

// ---------- __date (TruncDate) ----------

#[test]
fn date_lookup_strips_time_component_pg() {
    let day = NaiveDate::from_ymd_opt(2026, 6, 6).unwrap();
    let qs = Post::objects().filter("created__date", day);
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(
        stmt.sql.contains(r#"DATE("created")"#),
        "PG date: {}",
        stmt.sql
    );
    assert!(stmt.sql.contains(" = $1"));
}

#[test]
fn date_lookup_with_gte_pg() {
    let day = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
    let qs = Post::objects().filter("created__date__gte", day);
    let stmt = Postgres.compile_select(&qs.compile().unwrap()).unwrap();
    assert!(stmt.sql.contains(r#"DATE("created")"#));
    assert!(stmt.sql.contains(" >= $1"));
}

// ---------- Unknown trailing op surfaces as UnknownLookup ----------

#[test]
fn unknown_trailing_op_errors_as_unknown_lookup() {
    let result = Post::objects()
        .filter("created__year__icontains", "26")
        .compile();
    assert!(result.is_err(), "year__icontains should be rejected");
    let err = format!("{:?}", result.unwrap_err());
    assert!(
        err.contains("UnknownLookup") || err.contains("year__icontains"),
        "expected UnknownLookup, got: {err}"
    );
}

// ---------- Field unknown still errors ----------

#[test]
fn date_transform_on_unknown_field_errors() {
    let result = Post::objects()
        .filter("nonexistent__year", 2026_i64)
        .compile();
    assert!(result.is_err());
}
