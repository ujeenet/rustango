//! A stored value as its form input expects it, shared by the admin and template views.

use crate::core::{FieldSchema, FieldType};

/// A bool cell: JSON `true`/`false`, or the `1`/`0` SQLite and MySQL
/// store when a row reaches JSON without the typed decode (#1730).
pub(crate) fn json_bool(v: &serde_json::Value) -> Option<bool> {
    v.as_bool().or_else(|| v.as_i64().map(|n| n != 0))
}

/// `field`'s value in `row` as its HTML input expects it: no offset on a
/// datetime, at most 3 fraction digits on a time, pretty JSON.
pub(crate) fn render_value_for_input_json(row: &serde_json::Value, field: &FieldSchema) -> String {
    let v = row.get(field.column).or_else(|| row.get(field.name));
    let Some(v) = v else { return String::new() };
    if v.is_null() {
        return String::new();
    }
    match field.ty {
        FieldType::I16 | FieldType::I32 | FieldType::I64 => v
            .as_i64()
            .map(|n| n.to_string())
            .unwrap_or_else(|| v.as_str().unwrap_or("").to_owned()),
        FieldType::F32 | FieldType::F64 => v
            .as_f64()
            .map(|n| n.to_string())
            .unwrap_or_else(|| v.as_str().unwrap_or("").to_owned()),
        FieldType::Bool => json_bool(v)
            .map(|b| b.to_string())
            .unwrap_or_else(|| v.as_str().unwrap_or("").to_owned()),
        FieldType::String | FieldType::Uuid => v.as_str().unwrap_or("").to_owned(),
        FieldType::Date => v.as_str().unwrap_or("").to_owned(),
        FieldType::Time => {
            // A time input clears a value with more than 3 fraction digits.
            let s = v.as_str().unwrap_or("");
            match chrono::NaiveTime::parse_from_str(s, "%H:%M:%S%.f") {
                Ok(t) if chrono::Timelike::nanosecond(&t) != 0 => {
                    t.format("%H:%M:%S%.3f").to_string()
                }
                _ => s.to_owned(),
            }
        }
        FieldType::DateTime => {
            // Truncate to `YYYY-MM-DDTHH:MM:SS` (no fractional / TZ)
            // for the datetime-local input. Accept both with-T and
            // with-space separators since serde_json may produce
            // either depending on the underlying chrono Format.
            let s = v.as_str().unwrap_or("");
            if s.len() >= 19 {
                s[..19].to_owned()
            } else {
                s.to_owned()
            }
        }
        FieldType::Json => {
            // A nullable field saves "" as NULL, so its `{}` must show.
            if !field.nullable && v == &serde_json::Value::Object(serde_json::Map::new()) {
                String::new()
            } else {
                serde_json::to_string_pretty(v).unwrap_or_default()
            }
        }
        // Decimal round-trips as a string via `row_to_json` to
        // preserve precision; Binary likewise (hex-encoded).
        FieldType::Decimal | FieldType::Binary => v.as_str().unwrap_or("").to_owned(),
        // PG array: the comma list the form parser splits.
        FieldType::Array(_) => v.as_array().map_or_else(
            || v.to_string(),
            |items| {
                items
                    .iter()
                    .map(|i| i.as_str().map_or_else(|| i.to_string(), str::to_owned))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        ),
        // #343 — PG range: literal string (or compact JSON fallback).
        FieldType::Range(_) => v.as_str().map_or_else(|| v.to_string(), str::to_owned),
        // #342 — PG hstore: compact JSON object form.
        FieldType::HStore => v.to_string(),
        // #824 — pgvector: compact JSON array form.
        FieldType::Vector(_) => v.to_string(),
        // #443 — PostGIS Point: compact JSON object form.
        FieldType::Geometry(_) => v.to_string(),
    }
}
