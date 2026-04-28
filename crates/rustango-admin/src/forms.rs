//! Form value <-> `SqlValue` conversion for create / edit handlers.
//!
//! Browsers POST `application/x-www-form-urlencoded` with all string
//! values; we parse them into typed `SqlValue`s using `FieldSchema`.
//! Bool checkboxes are special: an unchecked checkbox produces no key
//! at all, so we look at *presence* rather than value.

use std::collections::HashMap;

use rustango_core::{FieldSchema, FieldType, SqlValue};

/// Errors raised while turning a form payload into IR values.
#[derive(Debug, thiserror::Error)]
pub(crate) enum FormError {
    #[error("required field `{field}` was missing from the form")]
    Missing { field: String },
    #[error("field `{field}` has invalid {ty} value `{value}`: {detail}")]
    Parse {
        field: String,
        ty: &'static str,
        value: String,
        detail: String,
    },
    #[error("PK field `{field}` of type {ty} is not supported in the admin")]
    UnsupportedPk { field: String, ty: &'static str },
}

/// Parse a single PK fragment from a path segment into an `SqlValue`.
pub(crate) fn parse_pk_string(field: &FieldSchema, raw: &str) -> Result<SqlValue, FormError> {
    let make_parse_err = |ty: &'static str, e: &dyn std::fmt::Display| FormError::Parse {
        field: field.name.to_owned(),
        ty,
        value: raw.to_owned(),
        detail: e.to_string(),
    };
    match field.ty {
        FieldType::I32 => raw
            .parse::<i32>()
            .map(SqlValue::I32)
            .map_err(|e| make_parse_err("i32", &e)),
        FieldType::I64 => raw
            .parse::<i64>()
            .map(SqlValue::I64)
            .map_err(|e| make_parse_err("i64", &e)),
        FieldType::String => Ok(SqlValue::String(raw.to_owned())),
        FieldType::Uuid => uuid::Uuid::parse_str(raw)
            .map(SqlValue::Uuid)
            .map_err(|e| make_parse_err("Uuid", &e)),
        FieldType::Bool
        | FieldType::F32
        | FieldType::F64
        | FieldType::DateTime
        | FieldType::Date
        | FieldType::Json => Err(FormError::UnsupportedPk {
            field: field.name.to_owned(),
            ty: field.ty.as_str(),
        }),
    }
}

/// Parse one form value (`Option<&str>` because absent checkboxes give `None`).
pub(crate) fn parse_form_value(
    field: &FieldSchema,
    raw: Option<&str>,
) -> Result<SqlValue, FormError> {
    let Some(raw) = raw else {
        return Ok(match field.ty {
            FieldType::Bool => SqlValue::Bool(false),
            _ if field.nullable => SqlValue::Null,
            _ => {
                return Err(FormError::Missing {
                    field: field.name.to_owned(),
                });
            }
        });
    };
    if field.nullable && raw.is_empty() {
        return Ok(SqlValue::Null);
    }
    let make_parse_err = |ty: &'static str, e: &dyn std::fmt::Display| FormError::Parse {
        field: field.name.to_owned(),
        ty,
        value: raw.to_owned(),
        detail: e.to_string(),
    };
    match field.ty {
        FieldType::Bool => {
            // HTML form submits "on" (or our explicit value) for a checked box.
            // Anything else we treat as falsy except literal "false"/"0"/"off".
            let v = !matches!(
                raw.to_ascii_lowercase().as_str(),
                "" | "false" | "0" | "off" | "no"
            );
            Ok(SqlValue::Bool(v))
        }
        FieldType::I32 => raw
            .parse::<i32>()
            .map(SqlValue::I32)
            .map_err(|e| make_parse_err("i32", &e)),
        FieldType::I64 => raw
            .parse::<i64>()
            .map(SqlValue::I64)
            .map_err(|e| make_parse_err("i64", &e)),
        FieldType::F32 => raw
            .parse::<f32>()
            .map(SqlValue::F32)
            .map_err(|e| make_parse_err("f32", &e)),
        FieldType::F64 => raw
            .parse::<f64>()
            .map(SqlValue::F64)
            .map_err(|e| make_parse_err("f64", &e)),
        FieldType::String => Ok(SqlValue::String(raw.to_owned())),
        FieldType::Uuid => uuid::Uuid::parse_str(raw)
            .map(SqlValue::Uuid)
            .map_err(|e| make_parse_err("Uuid", &e)),
        FieldType::Date => chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
            .map(SqlValue::Date)
            .map_err(|e| make_parse_err("Date", &e)),
        FieldType::DateTime => {
            // HTML datetime-local: "YYYY-MM-DDTHH:MM" or "...:SS". RFC 3339
            // is also accepted for paste-friendliness.
            if let Ok(d) = chrono::DateTime::parse_from_rfc3339(raw) {
                return Ok(SqlValue::DateTime(d.with_timezone(&chrono::Utc)));
            }
            let ndt = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S")
                .or_else(|_| chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M"))
                .map_err(|e| make_parse_err("DateTime", &e))?;
            Ok(SqlValue::DateTime(ndt.and_utc()))
        }
        FieldType::Json => Err(FormError::UnsupportedPk {
            field: field.name.to_owned(),
            ty: "Json",
        }),
    }
}

/// Walk every scalar field of `model` and turn the form payload into a
/// `(column, value)` list ready to feed an `InsertQuery`/`UpdateQuery`.
pub(crate) fn collect_values(
    model: &'static rustango_core::ModelSchema,
    form: &HashMap<String, String>,
    skip: &[&str],
) -> Result<Vec<(&'static str, SqlValue)>, FormError> {
    let mut out = Vec::new();
    for field in model.scalar_fields() {
        if skip.contains(&field.name) {
            continue;
        }
        let raw = form.get(field.name).map(String::as_str);
        let value = parse_form_value(field, raw)?;
        out.push((field.column, value));
    }
    Ok(out)
}
