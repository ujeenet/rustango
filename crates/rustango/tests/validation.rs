//! Per-field validation: `max_length`, `min`, `max`.
//!
//! Covers the schema-landing of bounds via `#[derive(Model)]`, the
//! `validate_value` helper, and `InsertQuery::validate` /
//! `UpdateQuery::validate` IR-level checks.

use rustango::core::{
    validate_value, Assignment, Filter, InsertQuery, Model as _, Op, QueryError, SqlValue,
    UpdateQuery, WhereExpr,
};
use rustango::Model;

#[derive(Model)]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 8)]
    name: String,
    #[rustango(max_length = 16)]
    email: Option<String>,
    #[rustango(min = 0, max = 150)]
    age: i32,
    #[rustango(min = -100)]
    balance: i64,
    is_active: bool,
}

// ---------------- schema landing ----------------

#[test]
fn schema_carries_max_length() {
    let f = User::SCHEMA.field("name").unwrap();
    assert_eq!(f.max_length, Some(8));
    assert_eq!(f.min, None);
    assert_eq!(f.max, None);
}

#[test]
fn schema_carries_max_length_through_option() {
    let f = User::SCHEMA.field("email").unwrap();
    assert_eq!(f.max_length, Some(16));
    assert!(f.nullable);
}

#[test]
fn schema_carries_min_and_max() {
    let f = User::SCHEMA.field("age").unwrap();
    assert_eq!(f.min, Some(0));
    assert_eq!(f.max, Some(150));
    assert_eq!(f.max_length, None);
}

#[test]
fn schema_carries_negative_min_only() {
    let f = User::SCHEMA.field("balance").unwrap();
    assert_eq!(f.min, Some(-100));
    assert_eq!(f.max, None);
}

#[test]
fn schema_has_no_bounds_when_unspecified() {
    let f = User::SCHEMA.field("is_active").unwrap();
    assert_eq!(f.max_length, None);
    assert_eq!(f.min, None);
    assert_eq!(f.max, None);
}

#[test]
fn field_by_column_lookup_works() {
    let f = User::SCHEMA.field_by_column("name").unwrap();
    assert_eq!(f.name, "name");
    assert!(User::SCHEMA.field_by_column("nope").is_none());
}

// ---------------- validate_value ----------------

#[test]
fn validate_value_accepts_string_at_max_length() {
    let f = User::SCHEMA.field("name").unwrap();
    let value = SqlValue::String("a".repeat(8));
    assert!(validate_value(User::SCHEMA.name, f, &value).is_ok());
}

#[test]
fn validate_value_rejects_string_over_max_length() {
    let f = User::SCHEMA.field("name").unwrap();
    let value = SqlValue::String("a".repeat(9));
    let err = validate_value(User::SCHEMA.name, f, &value).unwrap_err();
    assert_eq!(
        err,
        QueryError::MaxLengthExceeded {
            model: "User",
            field: "name".into(),
            max: 8,
            actual: 9,
        }
    );
}

#[test]
fn validate_value_counts_characters_not_bytes() {
    // Cyrillic "ё" is 2 bytes in UTF-8 but 1 character; max_length = 8
    // means 8 characters, not 8 bytes.
    let f = User::SCHEMA.field("name").unwrap();
    let value = SqlValue::String("ёёёёёёёё".to_owned());
    assert!(validate_value(User::SCHEMA.name, f, &value).is_ok());
    let value = SqlValue::String("ёёёёёёёёё".to_owned());
    assert!(validate_value(User::SCHEMA.name, f, &value).is_err());
}

#[test]
fn validate_value_skips_null() {
    let f = User::SCHEMA.field("email").unwrap();
    assert!(validate_value(User::SCHEMA.name, f, &SqlValue::Null).is_ok());
}

#[test]
fn validate_value_accepts_int_at_bounds() {
    let f = User::SCHEMA.field("age").unwrap();
    assert!(validate_value(User::SCHEMA.name, f, &SqlValue::I32(0)).is_ok());
    assert!(validate_value(User::SCHEMA.name, f, &SqlValue::I32(150)).is_ok());
}

#[test]
fn validate_value_rejects_int_below_min() {
    let f = User::SCHEMA.field("age").unwrap();
    let err = validate_value(User::SCHEMA.name, f, &SqlValue::I32(-1)).unwrap_err();
    assert_eq!(
        err,
        QueryError::OutOfRange {
            model: "User",
            field: "age".into(),
            value: -1,
            min: Some(0),
            max: Some(150),
        }
    );
}

#[test]
fn validate_value_rejects_int_above_max() {
    let f = User::SCHEMA.field("age").unwrap();
    let err = validate_value(User::SCHEMA.name, f, &SqlValue::I32(151)).unwrap_err();
    assert!(matches!(err, QueryError::OutOfRange { value: 151, .. }));
}

#[test]
fn validate_value_accepts_int_when_only_min_set() {
    let f = User::SCHEMA.field("balance").unwrap();
    // min = -100, max = None
    assert!(validate_value(User::SCHEMA.name, f, &SqlValue::I64(-100)).is_ok());
    assert!(validate_value(User::SCHEMA.name, f, &SqlValue::I64(i64::MAX)).is_ok());
    assert!(validate_value(User::SCHEMA.name, f, &SqlValue::I64(-101)).is_err());
}

#[test]
fn validate_value_no_bounds_passes_everything() {
    let f = User::SCHEMA.field("is_active").unwrap();
    assert!(validate_value(User::SCHEMA.name, f, &SqlValue::Bool(true)).is_ok());
    assert!(validate_value(User::SCHEMA.name, f, &SqlValue::Bool(false)).is_ok());
}

// ---------------- InsertQuery::validate ----------------

#[test]
fn insert_validate_accepts_in_bounds_values() {
    let q = InsertQuery {
        model: User::SCHEMA,
        columns: vec!["id", "name", "email", "age", "balance", "is_active"],
        values: vec![
            SqlValue::I64(1),
            SqlValue::String("alice".into()),
            SqlValue::Null,
            SqlValue::I32(30),
            SqlValue::I64(0),
            SqlValue::Bool(true),
        ],
        returning: Vec::new(),
        on_conflict: None,
    };
    assert!(q.validate().is_ok());
}

#[test]
fn insert_validate_rejects_too_long_string() {
    let q = InsertQuery {
        model: User::SCHEMA,
        columns: vec!["name"],
        values: vec![SqlValue::String("a".repeat(50))],
        returning: Vec::new(),
        on_conflict: None,
    };
    let err = q.validate().unwrap_err();
    assert!(matches!(err, QueryError::MaxLengthExceeded { max: 8, .. }));
}

#[test]
fn insert_validate_rejects_out_of_range_int() {
    let q = InsertQuery {
        model: User::SCHEMA,
        columns: vec!["age"],
        values: vec![SqlValue::I32(200)],
        returning: Vec::new(),
        on_conflict: None,
    };
    let err = q.validate().unwrap_err();
    assert!(matches!(err, QueryError::OutOfRange { value: 200, .. }));
}

#[test]
fn insert_validate_reports_unknown_column() {
    let q = InsertQuery {
        model: User::SCHEMA,
        columns: vec!["nope"],
        values: vec![SqlValue::I32(1)],
        returning: Vec::new(),
        on_conflict: None,
    };
    let err = q.validate().unwrap_err();
    assert_eq!(
        err,
        QueryError::UnknownField {
            model: "User",
            field: "nope".into()
        }
    );
}

// ---------------- UpdateQuery::validate ----------------

#[test]
fn update_validate_checks_set_values_only() {
    // Filters intentionally don't validate — they compare against existing
    // rows. A long filter value is allowed; a long SET value is not.
    let q = UpdateQuery {
        model: User::SCHEMA,
        set: vec![Assignment {
            column: "name",
            value: SqlValue::String("a".repeat(20)).into(),
        }],
        where_clause: WhereExpr::Predicate(Filter {
            column: "name",
            op: Op::Eq,
            value: SqlValue::String("a".repeat(50)), // not validated
        }),
    };
    let err = q.validate().unwrap_err();
    assert!(matches!(err, QueryError::MaxLengthExceeded { .. }));
}

#[test]
fn update_validate_allows_in_bounds_set() {
    let q = UpdateQuery {
        model: User::SCHEMA,
        set: vec![
            Assignment {
                column: "name",
                value: SqlValue::String("ok".into()).into(),
            },
            Assignment {
                column: "age",
                value: SqlValue::I32(25).into(),
            },
        ],
        where_clause: WhereExpr::And(vec![]),
    };
    assert!(q.validate().is_ok());
}

#[test]
fn update_validate_rejects_out_of_range_set() {
    let q = UpdateQuery {
        model: User::SCHEMA,
        set: vec![Assignment {
            column: "age",
            value: SqlValue::I32(-5).into(),
        }],
        where_clause: WhereExpr::And(vec![]),
    };
    let err = q.validate().unwrap_err();
    assert!(matches!(err, QueryError::OutOfRange { value: -5, .. }));
}
