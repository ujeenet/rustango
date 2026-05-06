//! Per-field write-time validation: `max_length`, `min`, `max`.
//!
//! Used by the query and SQL layers before binding a value to a parameter.
//! Type-shape validation (`FieldType` vs `SqlValue`) lives elsewhere — this
//! module is purely for declared bounds.

use super::{FieldSchema, QueryError, SqlValue};

/// Validate a single bound value against its field's declared bounds.
///
/// Skips:
/// * `SqlValue::Null` — the database enforces NOT NULL via the schema.
/// * `SqlValue::List` — element-by-element checking is the caller's job
///   (used for `IN`, where bounds normally don't apply anyway).
///
/// # Errors
/// Returns [`QueryError::MaxLengthExceeded`] for over-length strings and
/// [`QueryError::OutOfRange`] for integers outside `[min, max]`.
pub fn validate_value(
    model: &'static str,
    field: &FieldSchema,
    value: &SqlValue,
) -> Result<(), QueryError> {
    match value {
        SqlValue::String(s) => check_max_length(model, field, s),
        SqlValue::I16(v) => check_int_range(model, field, i64::from(*v)),
        SqlValue::I32(v) => check_int_range(model, field, i64::from(*v)),
        SqlValue::I64(v) => check_int_range(model, field, *v),
        // No declared bounds for these variants in v0.1; `Null` is enforced
        // by the DB's NOT NULL constraint, and `List` is bound element-wise
        // by the caller (used only for `IN`).
        SqlValue::Null
        | SqlValue::List(_)
        | SqlValue::F32(_)
        | SqlValue::F64(_)
        | SqlValue::Bool(_)
        | SqlValue::DateTime(_)
        | SqlValue::Date(_)
        | SqlValue::Uuid(_)
        | SqlValue::Json(_) => Ok(()),
    }
}

fn check_max_length(
    model: &'static str,
    field: &FieldSchema,
    value: &str,
) -> Result<(), QueryError> {
    let Some(max) = field.max_length else {
        return Ok(());
    };
    let actual = value.chars().count();
    let actual_u32 = u32::try_from(actual).unwrap_or(u32::MAX);
    if actual_u32 > max {
        return Err(QueryError::MaxLengthExceeded {
            model,
            field: field.name.to_owned(),
            max,
            actual: actual_u32,
        });
    }
    Ok(())
}

fn check_int_range(model: &'static str, field: &FieldSchema, value: i64) -> Result<(), QueryError> {
    let below = field.min.is_some_and(|m| value < m);
    let above = field.max.is_some_and(|m| value > m);
    if below || above {
        return Err(QueryError::OutOfRange {
            model,
            field: field.name.to_owned(),
            value,
            min: field.min,
            max: field.max,
        });
    }
    Ok(())
}
