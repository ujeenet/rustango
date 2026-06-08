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
        SqlValue::String(s) => {
            check_max_length(model, field, s)?;
            check_choices(model, field, s)?;
            check_named_validators(model, field, s)
        }
        SqlValue::I16(v) => check_int_range(model, field, i64::from(*v)),
        SqlValue::I32(v) => check_int_range(model, field, i64::from(*v)),
        SqlValue::I64(v) => check_int_range(model, field, *v),
        // No declared bounds for these variants in v0.1; `Null` is enforced
        // by the DB's NOT NULL constraint, and `List` is bound element-wise
        // by the caller (used only for `IN`).
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
        | SqlValue::HStore(_) => Ok(()),
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

/// Dispatch each `#[rustango(validators = "...")]` name in
/// `field.validators` to the built-in `validators::*` family. #447.
///
/// Unknown names surface as [`QueryError::UnknownValidator`]; a
/// validator that rejects the value surfaces as
/// [`QueryError::ValidatorFailed`].
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
            // #337 — Django `GenericIPAddressField` accepts either
            // family. Alias `genericipaddress` for callers translating
            // verbatim from a Django Field.
            "ip_address" | "genericipaddress" => v::validate_ip_address(value),
            // #338 — Django `FilePathField` — structural-only check
            // (non-empty, no NUL, no `..` segments). Existence-on-disk
            // is project-specific; callers add their own validator
            // when needed.
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
