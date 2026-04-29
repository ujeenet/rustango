//! Form value parsing — shared between the auto-admin and user route
//! handlers (slice 8.4A).
//!
//! Browsers POST `application/x-www-form-urlencoded` with all string
//! values; this module turns the string-keyed payload into typed
//! [`SqlValue`](crate::core::SqlValue)s using the model's
//! [`FieldSchema`](crate::core::FieldSchema). Bool checkboxes are
//! special: an unchecked checkbox produces no key at all, so we look
//! at *presence* rather than value.
//!
//! v0.8 ships the parsers only — admin's existing CRUD handlers
//! re-export from here. Slice 8.4B adds `#[derive(Form)]` (a derive
//! macro that emits a `from_request` impl on a user struct), and
//! slice 8.4C lights up CSRF middleware + multipart file uploads on
//! top of the same parsing layer.
//!
//! Gated by the `forms` feature (in `default`). The `admin` feature
//! implies `forms` so admin's CRUD path doesn't need to re-export
//! parsers locally.
//!
//! ```ignore
//! use rustango::forms::{collect_values, FormError};
//! use std::collections::HashMap;
//!
//! let form: HashMap<String, String> = /* axum's Form extractor result */;
//! let values = collect_values(MyModel::SCHEMA, &form, &[])?;
//! // values is a Vec<(&'static str, SqlValue)> ready to feed an
//! // InsertQuery / UpdateQuery `set` list.
//! # Ok::<_, FormError>(())
//! ```

use std::collections::HashMap;

use crate::core::{FieldSchema, FieldType, SqlValue};

#[cfg(feature = "csrf")]
pub mod csrf;

/// Trait every `#[derive(Form)]` struct implements (slice 8.4B).
///
/// `parse` consumes a string-keyed HashMap (the shape axum's
/// `Form<HashMap<String, String>>` extractor produces) and returns
/// either the typed struct or a [`FormError`] describing which field
/// failed and why.
///
/// The macro generates the parse body — users normally never name
/// this trait, they call `MyForm::parse(&form_map)?` directly.
pub trait FormStruct: Sized {
    /// Parse a payload into the struct. Returns the first failure
    /// encountered (one error at a time, like Django's
    /// `form.is_valid()`). v0.8 ships single-error-mode; v0.9 may
    /// add a multi-error variant if it earns its keep.
    ///
    /// # Errors
    /// Returns [`FormError`] for missing required fields, type-parse
    /// failures, or validator violations.
    fn parse(form: &std::collections::HashMap<String, String>) -> Result<Self, FormError>;
}

/// Errors raised while turning a form payload into IR values.
///
/// Public surface (slice 8.4A); admin's previous internal `FormError`
/// is now a re-export of this type.
#[derive(Debug, thiserror::Error)]
pub enum FormError {
    /// A required (non-nullable, non-bool) field had no value in the
    /// payload — the HTML form omitted it entirely.
    #[error("required field `{field}` was missing from the form")]
    Missing { field: String },

    /// The payload had a value but it didn't parse into the field's
    /// declared type (e.g. `"abc"` for an `i32` field).
    #[error("field `{field}` has invalid {ty} value `{value}`: {detail}")]
    Parse {
        field: String,
        ty: &'static str,
        value: String,
        detail: String,
    },

    /// `parse_pk_string` was called for a field type that can't sit
    /// in a URL path segment (e.g. `Bool`, `Json`). PKs in rustango
    /// are typed `i32` / `i64` / `String` / `Uuid` in practice; the
    /// other variants surface this error.
    #[error("PK field `{field}` of type {ty} is not supported in URL paths")]
    UnsupportedPk { field: String, ty: &'static str },
}

/// Parse a single PK fragment from a URL path segment into an
/// `SqlValue`. Used by admin's `/<table>/<pk>` route shape; user
/// code can call it directly when implementing custom URL patterns.
///
/// # Errors
/// * [`FormError::Parse`] — the string didn't parse into the field's
///   numeric / UUID type.
/// * [`FormError::UnsupportedPk`] — the field type can't sit in a
///   URL path (Bool, Float, Date, DateTime, Json).
pub fn parse_pk_string(field: &FieldSchema, raw: &str) -> Result<SqlValue, FormError> {
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
///
/// Empty string + nullable field → `SqlValue::Null`. Empty string +
/// non-null field → `FormError::Missing`. Bool fields treat the
/// presence of any non-empty value as `true` (HTML checkbox shape),
/// with `"false"`/`"0"`/`"off"`/`"no"` as the falsy escape hatches.
///
/// # Errors
/// As `parse_pk_string`, plus [`FormError::Missing`] for absent
/// required fields.
pub fn parse_form_value(
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

/// Walk every scalar field of `model` and turn the form payload into
/// a `(column, value)` list ready to feed an `InsertQuery` /
/// `UpdateQuery`.
///
/// `skip` is a list of field names to omit (typically the PK on
/// UPDATE so identity stays stable).
///
/// # Errors
/// As [`parse_form_value`].
pub fn collect_values(
    model: &'static crate::core::ModelSchema,
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
