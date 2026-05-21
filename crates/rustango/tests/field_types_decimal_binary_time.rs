//! Universal `FieldType::Decimal` / `Binary` / `Time` emission +
//! parsing tests. PR for #30 / #31 prerequisite — adds the universal
//! Django field types that work on every backend.
//!
//! ORM-extractability principle: all new types live in `core/` + per-
//! dialect emitters under `sql/`. No tenancy / admin / forms coupling.

use rust_decimal::Decimal;
use std::str::FromStr;

use rustango::core::{FieldType, SqlValue};
use rustango::sql::{Dialect, Postgres};

#[cfg(feature = "mysql")]
use rustango::sql::MySql;
#[cfg(feature = "sqlite")]
use rustango::sql::Sqlite;

// ---------- FieldType enum surface ----------

#[test]
fn field_type_decimal_binary_time_have_stable_names() {
    assert_eq!(FieldType::Decimal.as_str(), "rust_decimal::Decimal");
    assert_eq!(FieldType::Binary.as_str(), "Vec<u8>");
    assert_eq!(FieldType::Time.as_str(), "NaiveTime");
}

// ---------- SqlValue From impls ----------

#[test]
fn sql_value_from_decimal() {
    let d = Decimal::from_str("123.45").unwrap();
    let v: SqlValue = d.into();
    assert_eq!(v, SqlValue::Decimal(d));
    assert_eq!(v.field_type(), Some(FieldType::Decimal));
}

#[test]
fn sql_value_from_binary() {
    let bytes = vec![0xde, 0xad, 0xbe, 0xef];
    let v: SqlValue = bytes.clone().into();
    assert_eq!(v, SqlValue::Binary(bytes));
    assert_eq!(v.field_type(), Some(FieldType::Binary));
}

#[test]
fn sql_value_from_time() {
    let t = chrono::NaiveTime::from_hms_opt(14, 30, 45).unwrap();
    let v: SqlValue = t.into();
    assert_eq!(v, SqlValue::Time(t));
    assert_eq!(v.field_type(), Some(FieldType::Time));
}

#[test]
fn sql_value_display_strings() {
    let d = Decimal::from_str("-0.001").unwrap();
    assert_eq!(SqlValue::Decimal(d).to_display_string(), "-0.001");
    assert_eq!(
        SqlValue::Binary(vec![1, 2, 3, 4]).to_display_string(),
        "<binary 4 bytes>"
    );
    let t = chrono::NaiveTime::from_hms_opt(9, 5, 0).unwrap();
    assert_eq!(SqlValue::Time(t).to_display_string(), "09:05:00");
}

// ---------- Dialect column_type mapping ----------

#[test]
fn postgres_column_type_decimal_binary_time() {
    let d = Postgres;
    assert_eq!(d.column_type(FieldType::Decimal, None), "NUMERIC");
    assert_eq!(d.column_type(FieldType::Binary, None), "BYTEA");
    assert_eq!(d.column_type(FieldType::Time, None), "TIME");
}

#[cfg(feature = "mysql")]
#[test]
fn mysql_column_type_decimal_binary_time() {
    let d = MySql;
    assert_eq!(d.column_type(FieldType::Decimal, None), "DECIMAL(38, 10)");
    assert_eq!(d.column_type(FieldType::Binary, None), "LONGBLOB");
    assert_eq!(d.column_type(FieldType::Time, None), "TIME(6)");
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_column_type_decimal_binary_time() {
    let d = Sqlite;
    assert_eq!(d.column_type(FieldType::Decimal, None), "NUMERIC");
    assert_eq!(d.column_type(FieldType::Binary, None), "BLOB");
    // SQLite has no native TIME; per the affinity table, TEXT is the
    // closest storage class and `chrono::NaiveTime` round-trips
    // through it.
    assert_eq!(d.column_type(FieldType::Time, None), "TEXT");
}

#[test]
fn postgres_null_cast_decimal_binary_time() {
    let d = Postgres;
    assert_eq!(d.null_cast(FieldType::Decimal), Some("NUMERIC"));
    assert_eq!(d.null_cast(FieldType::Binary), Some("BYTEA"));
    assert_eq!(d.null_cast(FieldType::Time), Some("TIME"));
}

// ---------- Form parser ----------

#[test]
fn form_parser_decimal() {
    use rustango::core::FieldSchema;
    use rustango::forms::parse_form_value;
    let f = FieldSchema {
        name: "amount",
        column: "amount",
        ty: FieldType::Decimal,
        nullable: false,
        primary_key: false,
        auto: false,
        unique: false,
        max_length: None,
        min: None,
        max: None,
        default: None,
        relation: None,
        generated_as: None,
        help_text: None,
        choices: None,
        db_comment: None,
        verbose_name: None,
        editable: true,
        blank: false,
        validators: &[],
    };
    let v = parse_form_value(&f, Some("123.45")).unwrap();
    assert!(matches!(v, SqlValue::Decimal(d) if d == Decimal::from_str("123.45").unwrap()));

    // Malformed → Parse error.
    assert!(parse_form_value(&f, Some("not-a-number")).is_err());
}

#[test]
fn form_parser_binary_hex() {
    use rustango::core::FieldSchema;
    use rustango::forms::parse_form_value;
    let f = FieldSchema {
        name: "blob",
        column: "blob",
        ty: FieldType::Binary,
        nullable: false,
        primary_key: false,
        auto: false,
        unique: false,
        max_length: None,
        min: None,
        max: None,
        default: None,
        relation: None,
        generated_as: None,
        help_text: None,
        choices: None,
        db_comment: None,
        verbose_name: None,
        editable: true,
        blank: false,
        validators: &[],
    };
    let v = parse_form_value(&f, Some("deadbeef")).unwrap();
    assert!(matches!(v, SqlValue::Binary(b) if b == vec![0xde, 0xad, 0xbe, 0xef]));

    // Odd-length → reject.
    assert!(parse_form_value(&f, Some("abc")).is_err());
    // Non-hex → reject.
    assert!(parse_form_value(&f, Some("nothex!!")).is_err());
}

#[test]
fn form_parser_time() {
    use rustango::core::FieldSchema;
    use rustango::forms::parse_form_value;
    let f = FieldSchema {
        name: "opens_at",
        column: "opens_at",
        ty: FieldType::Time,
        nullable: false,
        primary_key: false,
        auto: false,
        unique: false,
        max_length: None,
        min: None,
        max: None,
        default: None,
        relation: None,
        generated_as: None,
        help_text: None,
        choices: None,
        db_comment: None,
        verbose_name: None,
        editable: true,
        blank: false,
        validators: &[],
    };
    // Full HH:MM:SS form.
    let v = parse_form_value(&f, Some("14:30:45")).unwrap();
    assert!(matches!(v, SqlValue::Time(_)));

    // Short HH:MM form (matches <input type="time">'s default value
    // when `step` is unset).
    let v = parse_form_value(&f, Some("09:05")).unwrap();
    let SqlValue::Time(t) = v else {
        panic!("expected Time");
    };
    assert_eq!(t, chrono::NaiveTime::from_hms_opt(9, 5, 0).unwrap());

    // Malformed → reject.
    assert!(parse_form_value(&f, Some("25:00:00")).is_err());
}
