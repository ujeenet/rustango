//! PG `RangeField` operators (`@>`, `<@`, `&&`, `<<`, `>>`, `-|-`) —
//! issue #31. Range values bind as `SqlValue::RangeLiteral(String)`;
//! PG implicit-casts to the column's range type at execute time.
//!
//! Same ORM-extractability principle as the array slice (#30): every
//! variant lives in `core/` (Op, SqlValue::RangeLiteral, Column trait
//! methods) + `sql/` (Dialect `write_range_op` + per-backend rejects).

use rustango::core::{Column as _, SqlValue};
#[cfg(feature = "mysql")]
use rustango::sql::MySql;
#[cfg(feature = "sqlite")]
use rustango::sql::Sqlite;
use rustango::sql::{Dialect, Postgres};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "rng_event")]
#[allow(dead_code)]
pub struct Event {
    #[rustango(primary_key)]
    id: i64,
    /// In a real app this column would be declared via raw migration
    /// SQL (`during tstzrange NOT NULL`) and used as a String here
    /// for the field type; the FieldType::Range(elem) declaration
    /// shape is a follow-up.
    #[rustango(max_length = 64)]
    during: String,
}

// ---------- PG: native emission ----------

#[test]
fn pg_emits_range_contains_operator() {
    let q = Event::objects()
        .where_(Event::during.range_contains("[2025-01-01, 2025-02-01)"))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""during" @> $1"#),
        "PG range_contains: {}",
        stmt.sql
    );
    assert!(
        matches!(&stmt.params[0], SqlValue::RangeLiteral(s) if s == "[2025-01-01, 2025-02-01)"),
        "range literal bound as RangeLiteral: {:?}",
        stmt.params[0]
    );
}

#[test]
fn pg_emits_range_contained_by_operator() {
    let q = Event::objects()
        .where_(Event::during.range_contained_by("[2025-01-01, 2026-01-01)"))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""during" <@ $1"#),
        "PG range_contained_by: {}",
        stmt.sql
    );
}

#[test]
fn pg_emits_range_overlap_operator() {
    let q = Event::objects()
        .where_(Event::during.range_overlap("[2025-06-01, 2025-07-01)"))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""during" && $1"#),
        "PG range_overlap: {}",
        stmt.sql
    );
}

#[test]
fn pg_emits_range_strictly_left_operator() {
    let q = Event::objects()
        .where_(Event::during.range_strictly_left("[2025-06-01,)"))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""during" << $1"#),
        "PG range_strictly_left: {}",
        stmt.sql
    );
}

#[test]
fn pg_emits_range_strictly_right_operator() {
    let q = Event::objects()
        .where_(Event::during.range_strictly_right("(, 2025-01-01)"))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""during" >> $1"#),
        "PG range_strictly_right: {}",
        stmt.sql
    );
}

#[test]
fn pg_emits_range_adjacent_operator() {
    let q = Event::objects()
        .where_(Event::during.range_adjacent("[2025-02-01, 2025-03-01)"))
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""during" -|- $1"#),
        "PG range_adjacent: {}",
        stmt.sql
    );
}

// ---------- MySQL / SQLite reject ----------

#[cfg(feature = "mysql")]
#[test]
fn mysql_rejects_range_op_with_clean_error() {
    use rustango::sql::SqlError;
    let q = Event::objects()
        .where_(Event::during.range_overlap("[a,b)"))
        .compile()
        .unwrap();
    let err = MySql.compile_select(&q).unwrap_err();
    match err {
        SqlError::OpNotSupportedInDialect { op, dialect } => {
            assert!(op.contains("range operators"), "op label: {op}");
            assert_eq!(dialect, "mysql");
        }
        other => panic!("expected OpNotSupportedInDialect, got: {other:?}"),
    }
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_rejects_range_op_with_clean_error() {
    use rustango::sql::SqlError;
    let q = Event::objects()
        .where_(Event::during.range_contains("[1, 10)"))
        .compile()
        .unwrap();
    let err = Sqlite.compile_select(&q).unwrap_err();
    assert!(matches!(
        err,
        SqlError::OpNotSupportedInDialect { op, .. } if op.contains("range operators")
    ));
}

// ---------- Django parser routes ----------

#[test]
fn parser_routes_range_overlap_from_string() {
    // The string-based filter accepts a plain `&str` literal that
    // the parser wraps in `SqlValue::RangeLiteral` automatically.
    let q = Event::objects()
        .filter("during__range_overlap", "[2025-01-01, 2025-02-01)")
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(r#""during" && $1"#),
        "parser route to RangeOverlap: {}",
        stmt.sql
    );
    assert!(
        matches!(&stmt.params[0], SqlValue::RangeLiteral(_)),
        "auto-wrapped in RangeLiteral: {:?}",
        stmt.params[0]
    );
}

#[test]
fn parser_routes_every_range_suffix() {
    for (suffix, op_token) in [
        ("range_contains", "@>"),
        ("range_contained_by", "<@"),
        ("range_overlap", "&&"),
        ("range_strictly_left", "<<"),
        ("range_strictly_right", ">>"),
        ("range_adjacent", "-|-"),
    ] {
        let q = Event::objects()
            .filter(format!("during__{suffix}").as_str(), "[1, 10)")
            .compile()
            .unwrap();
        let stmt = Postgres.compile_select(&q).unwrap();
        let needle = format!(r#""during" {op_token} $1"#);
        assert!(
            stmt.sql.contains(&needle),
            "suffix `{suffix}` should emit `{needle}`: {}",
            stmt.sql
        );
    }
}

#[test]
fn parser_rejects_non_string_value_for_range_lookup() {
    use rustango::core::QueryError;
    let r = Event::objects()
        .filter("during__range_overlap", SqlValue::I64(42))
        .compile();
    assert!(
        matches!(r, Err(QueryError::InvalidLookupValue { ref suffix, .. }) if suffix == "range_overlap"),
        "non-string value rejected: {r:?}",
    );
}

#[test]
fn unknown_lookup_error_advertises_range_suffixes() {
    let r = Event::objects().filter("during__nope", "v").compile();
    let err = r.unwrap_err();
    let msg = format!("{err}");
    for suffix in [
        "range_contains",
        "range_contained_by",
        "range_overlap",
        "range_strictly_left",
        "range_strictly_right",
        "range_adjacent",
    ] {
        assert!(
            msg.contains(suffix),
            "supported-lookups list should advertise `{suffix}`: {msg}"
        );
    }
}

// ---------- SqlValue surface ----------

#[test]
fn sql_value_range_literal_display() {
    let v = SqlValue::RangeLiteral("[1, 10)".to_owned());
    assert_eq!(v.to_display_string(), "range[1, 10)");
    // RangeLiteral has no field_type (it's a typeless text value
    // that PG implicit-casts to the column's range type).
    assert!(v.field_type().is_none());
}
