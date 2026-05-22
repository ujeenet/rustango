//! Django-parity #333 — `PositiveIntegerField` equivalent via
//! `#[rustango(min = 0)]` on an integer field. Verifies that the
//! validation runs model-side at write time and rejects negatives.

#![cfg(feature = "sqlite")]

use rustango::core::{Model as _, QueryError, SqlValue};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "pos_int_post")]
#[allow(dead_code)]
pub struct PosIntPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(min = 0)]
    views: i64,
}

fn field(name: &str) -> &'static rustango::core::FieldSchema {
    PosIntPost::SCHEMA.field(name).expect("field exists")
}

#[test]
fn schema_records_min_zero() {
    assert_eq!(field("views").min, Some(0));
}

#[test]
fn validate_value_rejects_negative() {
    let err =
        rustango::core::validate_value(PosIntPost::SCHEMA.name, field("views"), &SqlValue::I64(-1))
            .unwrap_err();
    match err {
        QueryError::OutOfRange {
            field,
            value,
            min,
            max,
            ..
        } => {
            assert_eq!(field, "views");
            assert_eq!(value, -1);
            assert_eq!(min, Some(0));
            assert_eq!(max, None);
        }
        other => panic!("expected OutOfRange, got: {other:?}"),
    }
}

#[test]
fn validate_value_accepts_zero_and_positive() {
    for v in [0_i64, 1, 100, i64::MAX] {
        rustango::core::validate_value(PosIntPost::SCHEMA.name, field("views"), &SqlValue::I64(v))
            .unwrap_or_else(|e| panic!("value {v} should be accepted, got {e:?}"));
    }
}
