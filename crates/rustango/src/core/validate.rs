//! Per-field write-time checks: `max_length`, `min`, `max`, `choices`
//! and the named validators.
//!
//! The query and SQL layers run these before binding a value. Type-shape
//! checks (`FieldType` vs `SqlValue`) live elsewhere.

use super::{FieldSchema, QueryError, SqlValue};

/// Check one bound value against the rules declared on its field.
///
/// Skips `SqlValue::Null`, since the database enforces NOT NULL, and
/// `SqlValue::List`, whose elements are the caller's job.
///
/// # Errors
/// [`QueryError::MaxLengthExceeded`] for over-long strings,
/// [`QueryError::OutOfRange`] for integers outside `[min, max]`,
/// [`QueryError::InvalidChoice`] for a value outside `choices`, and
/// [`QueryError::UnknownValidator`] / [`QueryError::ValidatorFailed`]
/// from the named validators.
pub fn validate_value(
    model: &'static str,
    field: &FieldSchema,
    value: &SqlValue,
) -> Result<(), QueryError> {
    match value {
        SqlValue::String(s) => {
            check_max_length(model, field, s)?;
            check_choices(model, field, s)?;
            check_named_validators(model, field, s)
        }
        SqlValue::I16(v) => check_int_range(model, field, i64::from(*v)),
        SqlValue::I32(v) => check_int_range(model, field, i64::from(*v)),
        SqlValue::I64(v) => check_int_range(model, field, *v),
        // No declared bounds apply to these variants.
        SqlValue::Null
        | SqlValue::List(_)
        | SqlValue::Array(_)
        | SqlValue::RangeLiteral(_)
        | SqlValue::F32(_)
        | SqlValue::F64(_)
        | SqlValue::Bool(_)
        | SqlValue::DateTime(_)
        | SqlValue::Date(_)
        | SqlValue::Time(_)
        | SqlValue::Uuid(_)
        | SqlValue::Json(_)
        | SqlValue::Decimal(_)
        | SqlValue::Binary(_)
        | SqlValue::HStore(_)
        | SqlValue::Vector(_)
        | SqlValue::Geometry { .. } => Ok(()),
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

fn check_choices(model: &'static str, field: &FieldSchema, value: &str) -> Result<(), QueryError> {
    let Some(choices) = field.choices else {
        return Ok(());
    };
    if choices.iter().any(|(v, _)| *v == value) {
        return Ok(());
    }
    Err(QueryError::InvalidChoice {
        model,
        field: field.name.to_owned(),
        value: value.to_owned(),
        allowed: choices.iter().map(|(v, _)| *v).collect(),
    })
}

/// Run each `#[rustango(validators = "...")]` name in
/// `field.validators` through the built-in `validators::*` family.
/// An unknown name gives [`QueryError::UnknownValidator`]; a rejected
/// value gives [`QueryError::ValidatorFailed`].
fn check_named_validators(
    model: &'static str,
    field: &FieldSchema,
    value: &str,
) -> Result<(), QueryError> {
    use crate::validators as v;

    for name in field.validators {
        let result = match *name {
            "email" => v::validate_email(value),
            "url" => v::validate_url(value),
            "slug" => v::validate_slug(value),
            "unicode_slug" => v::validate_unicode_slug(value),
            "phone_e164" => v::validate_phone_e164(value),
            "hex_color" => v::validate_hex_color(value),
            "uuid" => v::validate_uuid(value),
            "iso_date" => v::validate_iso_date(value),
            "iso_time" => v::validate_iso_time(value),
            "iso_datetime" => v::validate_iso_datetime(value),
            "ipv4" => v::validate_ipv4_address(value),
            "ipv6" => v::validate_ipv6_address(value),
            // Either IP family. `genericipaddress` is the Django spelling.
            "ip_address" | "genericipaddress" => v::validate_ip_address(value),
            // Shape only: non-empty, no NUL, no `..` segments. It does
            // not touch the disk.
            "filepath" | "filepath_field" => v::validate_filepath(value),
            "no_null" => v::validate_prohibit_null_characters(value),
            "email_list" => v::validate_email_list(value),
            "integer" => v::validate_integer(value),
            other => {
                return Err(QueryError::UnknownValidator {
                    model,
                    field: field.name.to_owned(),
                    validator: other,
                });
            }
        };
        if let Err(e) = result {
            return Err(QueryError::ValidatorFailed {
                model,
                field: field.name.to_owned(),
                validator: name,
                reason: e.to_string(),
            });
        }
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
