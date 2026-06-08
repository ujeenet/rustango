//! Unit coverage for `Range<T>` PostgreSQL range columns — Django
//! `RangeField` family (#343). No database required: asserts the derived
//! schema's `FieldType::Range` mapping, the per-dialect column-type
//! emission (`int4range` / `int8range` / `numrange` / `daterange` /
//! `tstzrange` on PG; degraded `TEXT` on MySQL / SQLite), and the
//! range-literal serialization.

use rustango::core::{FieldType, Model as _, RangeElem, SqlValue};
use rustango::sql::{Dialect, MySql, Postgres, Range, Sqlite};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "rng_event")]
#[allow(dead_code)]
pub struct Event {
    #[rustango(primary_key)]
    id: i64,
    seats: Range<i32>,
    capacity: Range<i64>,
    price_band: Range<rust_decimal::Decimal>,
    valid_on: Range<chrono::NaiveDate>,
    during: Range<chrono::DateTime<chrono::Utc>>,
}

fn field(name: &str) -> &'static rustango::core::FieldSchema {
    Event::SCHEMA
        .fields
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no field {name}"))
}

#[test]
fn derive_maps_range_fields_to_range_field_type() {
    assert_eq!(field("seats").ty, FieldType::Range(RangeElem::Int));
    assert_eq!(field("capacity").ty, FieldType::Range(RangeElem::BigInt));
    assert_eq!(field("price_band").ty, FieldType::Range(RangeElem::Numeric));
    assert_eq!(field("valid_on").ty, FieldType::Range(RangeElem::Date));
    assert_eq!(field("during").ty, FieldType::Range(RangeElem::DateTime));
}

#[test]
fn postgres_column_types_are_native_ranges() {
    assert_eq!(Postgres.column_type(field("seats").ty, None), "int4range");
    assert_eq!(
        Postgres.column_type(field("capacity").ty, None),
        "int8range"
    );
    assert_eq!(
        Postgres.column_type(field("price_band").ty, None),
        "numrange"
    );
    assert_eq!(
        Postgres.column_type(field("valid_on").ty, None),
        "daterange"
    );
    assert_eq!(Postgres.column_type(field("during").ty, None), "tstzrange");
}

#[test]
fn postgres_cast_types_match() {
    assert_eq!(
        Postgres.cast_type(FieldType::Range(RangeElem::Int)),
        Some("int4range")
    );
    assert_eq!(
        Postgres.cast_type(FieldType::Range(RangeElem::DateTime)),
        Some("tstzrange")
    );
}

#[test]
fn non_pg_dialects_degrade_to_text() {
    for elem in [
        RangeElem::Int,
        RangeElem::BigInt,
        RangeElem::Numeric,
        RangeElem::Date,
        RangeElem::DateTime,
    ] {
        let ty = FieldType::Range(elem);
        assert_eq!(MySql.column_type(ty, None), "TEXT");
        assert_eq!(Sqlite.column_type(ty, None), "TEXT");
        assert_eq!(MySql.cast_type(ty), None);
    }
}

#[test]
fn range_elem_type_tokens() {
    assert_eq!(RangeElem::Int.pg_range_type(), "int4range");
    assert_eq!(RangeElem::BigInt.pg_range_type(), "int8range");
    assert_eq!(RangeElem::Numeric.pg_range_type(), "numrange");
    assert_eq!(RangeElem::Date.pg_range_type(), "daterange");
    assert_eq!(RangeElem::DateTime.pg_range_type(), "tstzrange");
}

#[test]
fn range_into_sqlvalue_is_range_literal() {
    let v: SqlValue = Range::closed_open(1_i32, 10).into();
    assert!(matches!(v, SqlValue::RangeLiteral(ref s) if s == "[1,10)"));
}

#[test]
fn insert_casts_range_literal_to_its_pg_type() {
    // Regression for the CI failure: PG rejects `INSERT … VALUES ($1)`
    // when $1 is bound as text but the column is `int4range` (no
    // assignment cast). The writer must emit `$N::int4range`.
    use rustango::core::InsertQuery;
    let q = InsertQuery {
        model: Event::SCHEMA,
        columns: vec!["seats", "valid_on"],
        values: vec![
            SqlValue::RangeLiteral("[1,10)".into()),
            SqlValue::RangeLiteral("[2025-01-01,2025-02-01)".into()),
        ],
        returning: vec![],
        on_conflict: None,
    };
    let sql = Postgres.compile_insert(&q).unwrap().sql;
    assert!(
        sql.contains("$1::int4range"),
        "missing int4range cast: {sql}"
    );
    assert!(
        sql.contains("$2::daterange"),
        "missing daterange cast: {sql}"
    );
}
