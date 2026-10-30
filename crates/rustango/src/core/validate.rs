//! Per-field write-time checks: `max_length`, `min`, `max`, `choices`
//! and the named validators.
//!
//! The query and SQL layers run these before binding a value. Type-shape
//! checks (`FieldType` vs `SqlValue`) live elsewhere.

use super::{FieldSchema, QueryError, SqlValue};
use crate::validators::{self as v, ValidationError};

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
            // `blank` means "" is a valid value, whatever the rules for a non-empty one.
            if field.blank && s.is_empty() {
                return Ok(());
            }
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

/// The one list of `#[rustango(validators = "...")]` names. It builds the
/// runtime dispatch and the `names` items the derive points at, so a name
/// the derive accepts always runs here.
macro_rules! named_validators {
    ($($name:ident => $check:path),* $(,)?) => {
        /// One item per accepted name; the derive refers to them, so a
        /// typo fails to compile.
        #[doc(hidden)]
        #[allow(non_upper_case_globals)]
        pub mod names {
            $(pub const $name: &str = stringify!($name);)*
        }

        fn named_check(name: &str) -> Option<fn(&str) -> Result<(), ValidationError>> {
            match name {
                $(stringify!($name) => Some($check),)*
                _ => None,
            }
        }
    };
}

named_validators! {
    email => v::validate_email,
    url => v::validate_url,
    slug => v::validate_slug,
    unicode_slug => v::validate_unicode_slug,
    phone_e164 => v::validate_phone_e164,
    hex_color => v::validate_hex_color,
    uuid => v::validate_uuid,
    iso_date => v::validate_iso_date,
    iso_time => v::validate_iso_time,
    iso_datetime => v::validate_iso_datetime,
    ipv4 => v::validate_ipv4_address,
    ipv6 => v::validate_ipv6_address,
    // Either IP family; `genericipaddress` is an accepted alias.
    ip_address => v::validate_ip_address,
    genericipaddress => v::validate_ip_address,
    // Shape only: non-empty, no NUL, no `..` segments. It does not touch the disk.
    filepath => v::validate_filepath,
    filepath_field => v::validate_filepath,
    no_null => v::validate_prohibit_null_characters,
    email_list => v::validate_email_list,
    integer => v::validate_integer,
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
    for name in field.validators {
        let Some(check) = named_check(name) else {
            return Err(QueryError::UnknownValidator {
                model,
                field: field.name.to_owned(),
                validator: name,
            });
        };
        if let Err(e) = check(value) {
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
