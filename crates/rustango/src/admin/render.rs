//! HTML rendering helpers — handful of `format!` and per-`FieldType`
//! decoders. No template engine on purpose: keeps the dep tree small and
//! the output trivially auditable. We can swap in `maud` later if it
//! becomes worth it.

use std::fmt::Write as _;

use crate::core::{FieldSchema, FieldType};
#[cfg(feature = "postgres")]
use crate::sql::sqlx::{self, postgres::PgRow, Postgres, Row};

/// Escape a string for safe inclusion in HTML body or attribute context.
pub(crate) fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Render a single column value from a row to a (HTML-escaped) string.
///
/// `NULL` becomes the literal `<em>NULL</em>` (raw HTML — escape is
/// already applied to the inner text). Decode errors render as
/// `<error: …>`. Per-type formatting:
///
/// * scalars → `Display` impl
/// * `Uuid`  → hyphenated text form
/// * `DateTime<Utc>` → RFC 3339
/// * `NaiveDate`     → ISO 8601
/// * `serde_json::Value` → compact JSON
#[cfg(feature = "postgres")]
pub(crate) fn render_value(row: &PgRow, field: &FieldSchema) -> String {
    match field.ty {
        FieldType::I16 => format_display::<i16>(row, field),
        FieldType::I32 => format_display::<i32>(row, field),
        FieldType::I64 => format_display::<i64>(row, field),
        FieldType::F32 => format_display::<f32>(row, field),
        FieldType::F64 => format_display::<f64>(row, field),
        FieldType::Bool => format_bool_checkbox(row, field),
        FieldType::String => format_display::<String>(row, field),
        FieldType::DateTime => format_display::<chrono::DateTime<chrono::Utc>>(row, field),
        FieldType::Date => format_display::<chrono::NaiveDate>(row, field),
        FieldType::Uuid => format_display::<uuid::Uuid>(row, field),
        FieldType::Json => format_json(row, field),
    }
}

/// Render a `bool` field as a checkbox glyph instead of the raw
/// "true" / "false" text. Uses Unicode ☑ / ☐ so it works in the
/// admin's default chrome without needing Material Symbols or any
/// other icon font loaded.
///
/// NULL renders as the empty marker so the column stays a clean
/// vertical line of ✓ / ☐ — eyes can scan tens of rows in one
/// sweep, which the literal `true` / `false` text never allowed.
#[cfg(feature = "postgres")]
fn format_bool_checkbox(row: &PgRow, field: &FieldSchema) -> String {
    match row.try_get::<Option<bool>, _>(field.column) {
        Ok(Some(true)) => r#"<span class="rcms-bool yes" aria-label="true">☑</span>"#.to_owned(),
        Ok(Some(false)) | Ok(None) => {
            r#"<span class="rcms-bool no" aria-label="false">☐</span>"#.to_owned()
        }
        Err(e) => format!("<em>&lt;error: {}&gt;</em>", escape(&e.to_string())),
    }
}

#[cfg(feature = "postgres")]
fn format_display<T>(row: &PgRow, field: &FieldSchema) -> String
where
    T: std::fmt::Display + for<'r> sqlx::Decode<'r, Postgres> + sqlx::Type<Postgres>,
{
    match row.try_get::<Option<T>, _>(field.column) {
        Ok(Some(v)) => escape(&v.to_string()),
        Ok(None) => "<em>NULL</em>".to_owned(),
        Err(e) => format!("<em>&lt;error: {}&gt;</em>", escape(&e.to_string())),
    }
}

#[cfg(feature = "postgres")]
fn format_json(row: &PgRow, field: &FieldSchema) -> String {
    // sqlx requires the `json` feature to decode `serde_json::Value`. We
    // don't enable it in v0.1 to keep the dep tree small, so render a
    // placeholder rather than panic.
    let _ = (row, field);
    "<em>JSON (rendering disabled)</em>".to_owned()
}

/// Read a column value as a typed [`serde_json::Value`]. Mirrors what
/// the macro's audit emit produces via `serde_json::to_value(&self.f)`,
/// so admin-side audit JSON shapes match the app-code-side shapes:
///
/// * `I32` / `I64` / `F32` / `F64` → JSON number
/// * `Bool` → JSON true/false
/// * `String` / `Uuid` → JSON string
/// * `Date` → ISO 8601 date string
/// * `DateTime` → RFC 3339 datetime string
/// * `Json` → the raw JSONB value
///
/// `NULL` columns become `Value::Null`. Decode errors fall back to
/// `Value::Null` so an audit emit never fails the data write.
#[cfg(feature = "postgres")]
pub(crate) fn read_value_as_json(row: &PgRow, field: &FieldSchema) -> serde_json::Value {
    use serde_json::Value;
    match field.ty {
        FieldType::I16 => row
            .try_get::<Option<i16>, _>(field.column)
            .ok()
            .flatten()
            .map(Value::from)
            .unwrap_or(Value::Null),
        FieldType::I32 => row
            .try_get::<Option<i32>, _>(field.column)
            .ok()
            .flatten()
            .map(|v| Value::from(v))
            .unwrap_or(Value::Null),
        FieldType::I64 => row
            .try_get::<Option<i64>, _>(field.column)
            .ok()
            .flatten()
            .map(|v| Value::from(v))
            .unwrap_or(Value::Null),
        FieldType::F32 => row
            .try_get::<Option<f32>, _>(field.column)
            .ok()
            .flatten()
            .map(|v| Value::from(v))
            .unwrap_or(Value::Null),
        FieldType::F64 => row
            .try_get::<Option<f64>, _>(field.column)
            .ok()
            .flatten()
            .map(|v| Value::from(v))
            .unwrap_or(Value::Null),
        FieldType::Bool => row
            .try_get::<Option<bool>, _>(field.column)
            .ok()
            .flatten()
            .map(Value::from)
            .unwrap_or(Value::Null),
        FieldType::String => row
            .try_get::<Option<String>, _>(field.column)
            .ok()
            .flatten()
            .map(Value::from)
            .unwrap_or(Value::Null),
        FieldType::Uuid => row
            .try_get::<Option<uuid::Uuid>, _>(field.column)
            .ok()
            .flatten()
            .map(|v| Value::from(v.to_string()))
            .unwrap_or(Value::Null),
        FieldType::Date => row
            .try_get::<Option<chrono::NaiveDate>, _>(field.column)
            .ok()
            .flatten()
            .map(|d| Value::from(d.format("%Y-%m-%d").to_string()))
            .unwrap_or(Value::Null),
        FieldType::DateTime => row
            .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>(field.column)
            .ok()
            .flatten()
            .map(|d| Value::from(d.to_rfc3339()))
            .unwrap_or(Value::Null),
        FieldType::Json => row
            .try_get::<Option<Value>, _>(field.column)
            .ok()
            .flatten()
            .unwrap_or(Value::Null),
    }
}

/// Parse a form-payload string into a typed [`serde_json::Value`]
/// matching `field.ty`. Used by the admin audit emit to coerce form
/// values back to typed JSON before diffing — operators see numbers
/// as numbers and booleans as booleans, not as quoted strings.
///
/// Falls back to `Value::String(raw)` when the raw doesn't parse as
/// the field's type (e.g. a malformed integer); the emit path's job
/// is to record what happened, not to validate.
pub(crate) fn coerce_form_to_json(field: &FieldSchema, raw: &str) -> serde_json::Value {
    use serde_json::Value;
    if raw.is_empty() && field.nullable {
        return Value::Null;
    }
    match field.ty {
        FieldType::I16 => raw
            .parse::<i16>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(raw.to_owned())),
        FieldType::I32 => raw
            .parse::<i32>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(raw.to_owned())),
        FieldType::I64 => raw
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(raw.to_owned())),
        FieldType::F32 => raw
            .parse::<f32>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(raw.to_owned())),
        FieldType::F64 => raw
            .parse::<f64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(raw.to_owned())),
        FieldType::Bool => match raw.to_ascii_lowercase().as_str() {
            "true" | "on" | "1" | "yes" => Value::Bool(true),
            "false" | "off" | "0" | "no" | "" => Value::Bool(false),
            _ => Value::String(raw.to_owned()),
        },
        // String / Uuid / Date / DateTime / Json all stringify cleanly.
        _ => Value::String(raw.to_owned()),
    }
}

/// Backend-agnostic counterpart of [`render_value_for_input`]. Takes
/// a `serde_json::Value` instead of a `PgRow` — call sites that fetch
/// rows via the ORM (`Model::objects().fetch_pool` then
/// `serde_json::to_value(&row)`) can render form inputs without
/// pinning the registry to Postgres. Used by the v0.34 operator
/// console.
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
        FieldType::Bool => v
            .as_bool()
            .map(|b| b.to_string())
            .unwrap_or_else(|| v.as_str().unwrap_or("").to_owned()),
        FieldType::String | FieldType::Uuid => v.as_str().unwrap_or("").to_owned(),
        FieldType::Date => v.as_str().unwrap_or("").to_owned(),
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
            if v == &serde_json::Value::Object(serde_json::Map::new()) {
                String::new()
            } else {
                serde_json::to_string_pretty(v).unwrap_or_default()
            }
        }
    }
}

/// Best-effort string form of a column value for pre-populating form
/// inputs. Returns an empty string for `NULL`, so the input renders blank.
#[cfg(feature = "postgres")]
pub(crate) fn render_value_for_input(row: &PgRow, field: &FieldSchema) -> String {
    fn opt_to_string<T: std::fmt::Display>(v: Option<T>) -> String {
        v.map(|x| x.to_string()).unwrap_or_default()
    }
    match field.ty {
        FieldType::I16 => opt_to_string(row.try_get::<Option<i16>, _>(field.column).ok().flatten()),
        FieldType::I32 => opt_to_string(row.try_get::<Option<i32>, _>(field.column).ok().flatten()),
        FieldType::I64 => opt_to_string(row.try_get::<Option<i64>, _>(field.column).ok().flatten()),
        FieldType::F32 => opt_to_string(row.try_get::<Option<f32>, _>(field.column).ok().flatten()),
        FieldType::F64 => opt_to_string(row.try_get::<Option<f64>, _>(field.column).ok().flatten()),
        FieldType::Bool => row
            .try_get::<Option<bool>, _>(field.column)
            .ok()
            .flatten()
            .map(|b| b.to_string())
            .unwrap_or_default(),
        FieldType::String => row
            .try_get::<Option<String>, _>(field.column)
            .ok()
            .flatten()
            .unwrap_or_default(),
        FieldType::Uuid => opt_to_string(
            row.try_get::<Option<uuid::Uuid>, _>(field.column)
                .ok()
                .flatten(),
        ),
        FieldType::Date => row
            .try_get::<Option<chrono::NaiveDate>, _>(field.column)
            .ok()
            .flatten()
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_default(),
        FieldType::DateTime => row
            .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>(field.column)
            .ok()
            .flatten()
            .map(|d| d.format("%Y-%m-%dT%H:%M:%S").to_string())
            .unwrap_or_default(),
        FieldType::Json => row
            .try_get::<serde_json::Value, _>(field.column)
            .ok()
            .map(|v| {
                if v == serde_json::Value::Object(serde_json::Map::new())
                    || v == serde_json::json!({})
                {
                    String::new()
                } else {
                    serde_json::to_string_pretty(&v).unwrap_or_default()
                }
            })
            .unwrap_or_default(),
    }
}

/// Render a form input for `field`, optionally pre-filled with `value`.
/// PK fields get `readonly` when `pk_locked` is true (edit mode).
pub(crate) fn render_input(field: &FieldSchema, value: &str, pk_locked: bool) -> String {
    let name = escape(field.name);
    let val = escape(value);
    let required =
        if field.nullable || field.ty == FieldType::Bool || field.auto || field.primary_key {
            ""
        } else {
            " required"
        };
    // Caller passes `pk_locked=true` to mean "render this field as
    // read-only" — used both for PKs on edit forms (slice 10.5
    // pre-existing behavior) and for `readonly_fields` flagged via
    // `#[rustango(admin(...))]`.
    let readonly = if pk_locked { " readonly" } else { "" };

    match field.ty {
        FieldType::Bool => {
            let checked = if value == "true" { " checked" } else { "" };
            format!(
                r#"<input type="checkbox" name="{name}" id="{name}" value="true"{checked}{readonly}>"#
            )
        }
        FieldType::I16 | FieldType::I32 | FieldType::I64 => {
            let mut attrs = String::new();
            if let Some(min) = field.min {
                let _ = write!(attrs, r#" min="{min}""#);
            }
            if let Some(max) = field.max {
                let _ = write!(attrs, r#" max="{max}""#);
            }
            format!(
                r#"<input type="number" step="1" name="{name}" id="{name}" value="{val}"{attrs}{required}{readonly}>"#
            )
        }
        FieldType::F32 | FieldType::F64 => {
            format!(
                r#"<input type="number" step="any" name="{name}" id="{name}" value="{val}"{required}{readonly}>"#
            )
        }
        FieldType::String => match field.max_length {
            Some(n) if n <= 80 => format!(
                r#"<input type="text" name="{name}" id="{name}" value="{val}" maxlength="{n}"{required}{readonly}>"#
            ),
            Some(n) => format!(
                r#"<textarea name="{name}" id="{name}" maxlength="{n}"{required}{readonly}>{val}</textarea>"#
            ),
            None => format!(
                r#"<textarea name="{name}" id="{name}"{required}{readonly}>{val}</textarea>"#
            ),
        },
        FieldType::Date => format!(
            r#"<input type="date" name="{name}" id="{name}" value="{val}"{required}{readonly}>"#
        ),
        FieldType::DateTime => format!(
            r#"<input type="datetime-local" name="{name}" id="{name}" value="{val}"{required}{readonly}>"#
        ),
        FieldType::Uuid => format!(
            r#"<input type="text" name="{name}" id="{name}" value="{val}" pattern="[0-9a-fA-F\-]+"{required}{readonly}>"#
        ),
        FieldType::Json => format!(
            r#"<textarea name="{name}" id="{name}"{readonly} style="font-family:monospace">{val}</textarea>"#
        ),
    }
}

// ============================================================== FK helpers

/// Read a column value as a string, for use as a hash-map key or URL
/// fragment. Returns `None` for `NULL` and for value types we don't
/// support as PKs/FKs.
#[cfg(feature = "postgres")]
pub(crate) fn read_value_as_string(row: &PgRow, field: &FieldSchema) -> Option<String> {
    read_value_as_string_at(row, field, field.column)
}

// ============================================================ v0.36 — tri-dialect JSON companions
//
// The `_json` family below mirrors the PG-typed `render_value` /
// `read_value_as_string` / `read_value_as_string_at` /
// `read_joined_value_as_html` / `read_value_as_json` API surface,
// but takes a `&serde_json::Value` (the row object produced by
// `crate::sql::row_to_json` / `row_to_json_my` / `row_to_json_sqlite`)
// instead of a backend-specific `Row` type. The bundled admin's
// fetch path (v0.36 slice 4) routes every row through
// `select_rows_as_json_pool` → these renderers, so the rendering
// layer never sees a `PgRow` and works uniformly on PG / MySQL /
// SQLite.

/// Tri-dialect counterpart of [`render_value`]. Takes a JSON object
/// (`{ field.name: value }` per the [`crate::sql::row_to_json`]
/// shape) and renders the named field as HTML, applying the same
/// type-specific formatting:
///
/// * scalars → JSON numeric → `to_string`
/// * `Bool`  → ☑ / ☐ checkbox glyph (matches `render_value`)
/// * `String` / `Uuid` / `Date` / `DateTime` → escaped text
/// * `Json`  → compact JSON text
///
/// `null` (or missing key) renders as `<em>NULL</em>` — same as the
/// PG path's NULL marker. Decode errors aren't possible at this
/// layer (JSON is already typed); shape mismatches fall back to a
/// best-effort `to_string` of the underlying JSON node.
pub(crate) fn render_value_json(row: &serde_json::Value, field: &FieldSchema) -> String {
    let v = row.get(field.name);
    if matches!(v, None | Some(serde_json::Value::Null)) {
        // Bool keeps its checkbox-glyph shape even for NULL — admin
        // list rows scan cleaner as a vertical line of ☑/☐ than
        // text/NULL mixed.
        return if field.ty == FieldType::Bool {
            r#"<span class="rcms-bool no" aria-label="false">☐</span>"#.to_owned()
        } else {
            "<em>NULL</em>".to_owned()
        };
    }
    let v = v.unwrap();
    match field.ty {
        FieldType::Bool => match v.as_bool() {
            Some(true) => r#"<span class="rcms-bool yes" aria-label="true">☑</span>"#.to_owned(),
            _ => r#"<span class="rcms-bool no" aria-label="false">☐</span>"#.to_owned(),
        },
        FieldType::I16 | FieldType::I32 | FieldType::I64 => v
            .as_i64()
            .map(|n| escape(&n.to_string()))
            .unwrap_or_else(|| escape(&v.to_string())),
        FieldType::F32 | FieldType::F64 => v
            .as_f64()
            .map(|n| escape(&n.to_string()))
            .unwrap_or_else(|| escape(&v.to_string())),
        FieldType::String | FieldType::Uuid | FieldType::Date | FieldType::DateTime => {
            escape(v.as_str().unwrap_or(""))
        }
        FieldType::Json => {
            // Compact serialize to keep list cells one-line; the
            // detail view template can render expanded.
            serde_json::to_string(v)
                .map(|s| escape(&s))
                .unwrap_or_default()
        }
    }
}

/// Tri-dialect counterpart of [`read_value_as_string`]. JSON-shape
/// version: read the value at `field.name` and return its string
/// form, or `None` for `NULL` / missing / unsupported types.
pub(crate) fn read_value_as_string_json(
    row: &serde_json::Value,
    field: &FieldSchema,
) -> Option<String> {
    read_value_as_string_at_json(row, field, field.name)
}

/// Tri-dialect counterpart of [`read_value_as_string_at`]. The
/// `key` parameter is the JSON-map lookup name (typically
/// `field.name` for direct cells, or a facet alias like
/// `"facet_value"` for SELECT-AS aliases). PG-typed counterpart's
/// behavior is preserved: `Bool` / `F32` / `F64` / `Date` /
/// `DateTime` / `Json` are NOT supported as PK/FK key shapes and
/// return `None`.
pub(crate) fn read_value_as_string_at_json(
    row: &serde_json::Value,
    field: &FieldSchema,
    key: &str,
) -> Option<String> {
    let v = row.get(key)?;
    if v.is_null() {
        return None;
    }
    match field.ty {
        FieldType::I16 | FieldType::I32 | FieldType::I64 => v.as_i64().map(|n| n.to_string()),
        FieldType::String | FieldType::Uuid => v.as_str().map(str::to_owned),
        _ => None,
    }
}

/// Tri-dialect counterpart of [`read_joined_value_as_html`]. Reads
/// from `<alias>__<field.column>` in the JSON row object — the
/// admin fetch path writes joined columns under that prefixed key
/// so the JSON shape stays self-describing. Returns
/// already-HTML-escaped text or `None` for `NULL` (LEFT JOIN miss)
/// and unsupported types (`FieldType::Json`).
pub(crate) fn read_joined_value_as_html_json(
    row: &serde_json::Value,
    alias: &str,
    field: &FieldSchema,
) -> Option<String> {
    let key = format!("{}__{}", alias, field.column);
    let v = row.get(&key)?;
    if v.is_null() {
        return None;
    }
    let text: Option<String> = match field.ty {
        FieldType::I16 | FieldType::I32 | FieldType::I64 => v.as_i64().map(|n| n.to_string()),
        FieldType::F32 | FieldType::F64 => v.as_f64().map(|n| n.to_string()),
        FieldType::Bool => v.as_bool().map(|b| b.to_string()),
        FieldType::String | FieldType::Uuid | FieldType::Date | FieldType::DateTime => {
            v.as_str().map(str::to_owned)
        }
        FieldType::Json => None,
    };
    text.map(|s| escape(&s))
}

/// Tri-dialect counterpart of [`read_value_as_json`]. Walks the
/// already-decoded JSON row object and re-extracts the named field
/// as a `serde_json::Value` shaped consistently with what
/// `serde_json::to_value(&model.field)` would produce. Mostly an
/// identity passthrough since the row is already JSON; provides the
/// same coercion fallbacks as the PG path (e.g. strings parsing as
/// numbers when the field type is `I64`).
pub(crate) fn read_value_as_json_from_json(
    row: &serde_json::Value,
    field: &FieldSchema,
) -> serde_json::Value {
    use serde_json::Value;
    let v = row.get(field.name).cloned().unwrap_or(Value::Null);
    if v.is_null() {
        return Value::Null;
    }
    // Coerce the existing JSON value to the field's expected JSON
    // shape. Most types pass through unchanged; the exceptions
    // (`String → I64` when the row stores numbers as strings, e.g.
    // SQLite TEXT-affinity dates) get coerced.
    match field.ty {
        FieldType::I16 | FieldType::I32 | FieldType::I64 => {
            v.as_i64().map(Value::from).unwrap_or(v)
        }
        FieldType::F32 | FieldType::F64 => v.as_f64().map(Value::from).unwrap_or(v),
        FieldType::Bool => v.as_bool().map(Value::from).unwrap_or(v),
        _ => v,
    }
}

/// Variant of [`read_value_as_string`] that reads from an arbitrary
/// column alias (e.g. `"facet_value"` after a `SELECT col AS facet_value`).
/// Used by the facet-filter machinery (slice 10.4) which renames the
/// column to keep its query independent of the source table's schema.
#[cfg(feature = "postgres")]
pub(crate) fn read_value_as_string_at(
    row: &PgRow,
    field: &FieldSchema,
    column_alias: &str,
) -> Option<String> {
    match field.ty {
        FieldType::I16 => row
            .try_get::<Option<i16>, _>(column_alias)
            .ok()
            .flatten()
            .map(|v| v.to_string()),
        FieldType::I32 => row
            .try_get::<Option<i32>, _>(column_alias)
            .ok()
            .flatten()
            .map(|v| v.to_string()),
        FieldType::I64 => row
            .try_get::<Option<i64>, _>(column_alias)
            .ok()
            .flatten()
            .map(|v| v.to_string()),
        FieldType::String => row
            .try_get::<Option<String>, _>(column_alias)
            .ok()
            .flatten(),
        FieldType::Uuid => row
            .try_get::<Option<uuid::Uuid>, _>(column_alias)
            .ok()
            .flatten()
            .map(|v| v.to_string()),
        _ => None,
    }
}

/// Read a joined column from a row produced by a `LEFT JOIN`. The column
/// name in the result set is `<alias>__<field.column>`. Returns the value
/// as already-HTML-escaped text suitable for templating, or `None` for
/// `NULL` (LEFT JOIN miss) and unsupported types.
#[cfg(feature = "postgres")]
pub(crate) fn read_joined_value_as_html(
    row: &PgRow,
    alias: &str,
    field: &FieldSchema,
) -> Option<String> {
    let prefixed = format!("{}__{}", alias, field.column);
    let text: Option<String> = match field.ty {
        FieldType::I16 => row
            .try_get::<Option<i16>, _>(prefixed.as_str())
            .ok()
            .flatten()
            .map(|v| v.to_string()),
        FieldType::I32 => row
            .try_get::<Option<i32>, _>(prefixed.as_str())
            .ok()
            .flatten()
            .map(|v| v.to_string()),
        FieldType::I64 => row
            .try_get::<Option<i64>, _>(prefixed.as_str())
            .ok()
            .flatten()
            .map(|v| v.to_string()),
        FieldType::F32 => row
            .try_get::<Option<f32>, _>(prefixed.as_str())
            .ok()
            .flatten()
            .map(|v| v.to_string()),
        FieldType::F64 => row
            .try_get::<Option<f64>, _>(prefixed.as_str())
            .ok()
            .flatten()
            .map(|v| v.to_string()),
        FieldType::Bool => row
            .try_get::<Option<bool>, _>(prefixed.as_str())
            .ok()
            .flatten()
            .map(|v| v.to_string()),
        FieldType::String => row
            .try_get::<Option<String>, _>(prefixed.as_str())
            .ok()
            .flatten(),
        FieldType::Uuid => row
            .try_get::<Option<uuid::Uuid>, _>(prefixed.as_str())
            .ok()
            .flatten()
            .map(|v| v.to_string()),
        FieldType::Date => row
            .try_get::<Option<chrono::NaiveDate>, _>(prefixed.as_str())
            .ok()
            .flatten()
            .map(|v| v.to_string()),
        FieldType::DateTime => row
            .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>(prefixed.as_str())
            .ok()
            .flatten()
            .map(|v| v.to_string()),
        FieldType::Json => None,
    };
    text.map(|s| escape(&s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{FieldSchema, FieldType};
    use serde_json::json;

    fn field(name: &'static str, column: &'static str, ty: FieldType) -> FieldSchema {
        FieldSchema {
            name,
            column,
            ty,
            nullable: true,
            primary_key: false,
            auto: false,
            unique: false,
            max_length: None,
            min: None,
            max: None,
            default: None,
            relation: None,
            generated_as: None,
        }
    }

    #[test]
    fn render_value_json_bool_uses_checkbox_glyph() {
        let f = field("active", "active", FieldType::Bool);
        let row = json!({ "active": true });
        let html = render_value_json(&row, &f);
        assert!(html.contains("rcms-bool yes"));
        assert!(html.contains("☑"));
        let row = json!({ "active": false });
        let html = render_value_json(&row, &f);
        assert!(html.contains("rcms-bool no"));
        assert!(html.contains("☐"));
    }

    #[test]
    fn render_value_json_null_renders_em_null_except_for_bool() {
        let f_str = field("name", "name", FieldType::String);
        let row = json!({ "name": null });
        assert_eq!(render_value_json(&row, &f_str), "<em>NULL</em>");
        // Bool NULL still renders as the empty-checkbox so list cells
        // stay a clean vertical line of ☑/☐.
        let f_bool = field("active", "active", FieldType::Bool);
        let row = json!({ "active": null });
        let html = render_value_json(&row, &f_bool);
        assert!(html.contains("☐"));
    }

    #[test]
    fn render_value_json_integers_render_unadorned() {
        let f = field("count", "count", FieldType::I64);
        let row = json!({ "count": 42 });
        assert_eq!(render_value_json(&row, &f), "42");
    }

    #[test]
    fn render_value_json_strings_are_html_escaped() {
        let f = field("title", "title", FieldType::String);
        let row = json!({ "title": "<script>alert('xss')</script>" });
        let html = render_value_json(&row, &f);
        assert!(html.contains("&lt;script"));
        assert!(!html.contains("<script"));
    }

    #[test]
    fn read_value_as_string_json_handles_supported_types() {
        let row = json!({ "id": 42, "name": "alice" });
        assert_eq!(
            read_value_as_string_json(&row, &field("id", "id", FieldType::I64)),
            Some("42".into())
        );
        assert_eq!(
            read_value_as_string_json(&row, &field("name", "name", FieldType::String)),
            Some("alice".into())
        );
        // Bool and Date aren't supported as PK/FK key shapes — return None.
        let row = json!({ "active": true });
        assert_eq!(
            read_value_as_string_json(&row, &field("active", "active", FieldType::Bool)),
            None
        );
    }

    #[test]
    fn read_value_as_string_at_json_uses_custom_key() {
        let row = json!({ "facet_value": 7 });
        let f = field("id", "id", FieldType::I64);
        assert_eq!(
            read_value_as_string_at_json(&row, &f, "facet_value"),
            Some("7".into())
        );
    }

    #[test]
    fn read_joined_value_as_html_json_reads_prefixed_key() {
        let row = json!({ "author__name": "Ada Lovelace" });
        let f = field("name", "name", FieldType::String);
        assert_eq!(
            read_joined_value_as_html_json(&row, "author", &f),
            Some("Ada Lovelace".into())
        );
    }

    #[test]
    fn read_joined_value_as_html_json_returns_none_for_left_join_miss() {
        let row = json!({ "author__name": null });
        let f = field("name", "name", FieldType::String);
        assert_eq!(read_joined_value_as_html_json(&row, "author", &f), None);
    }

    #[test]
    fn read_value_as_json_from_json_passes_through_numbers() {
        let row = json!({ "count": 7 });
        let f = field("count", "count", FieldType::I64);
        let v = read_value_as_json_from_json(&row, &f);
        assert_eq!(v, json!(7));
    }
}
