//! HTML rendering helpers — handful of `format!` and per-`FieldType`
//! decoders. No template engine on purpose: keeps the dep tree small and
//! the output trivially auditable. We can swap in `maud` later if it
//! becomes worth it.

use std::fmt::Write as _;

use rustango_core::{FieldSchema, FieldType};
use rustango_sql::sqlx::{self, Postgres, Row, postgres::PgRow};

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
pub(crate) fn render_value(row: &PgRow, field: &FieldSchema) -> String {
    match field.ty {
        FieldType::I32 => format_display::<i32>(row, field),
        FieldType::I64 => format_display::<i64>(row, field),
        FieldType::F32 => format_display::<f32>(row, field),
        FieldType::F64 => format_display::<f64>(row, field),
        FieldType::Bool => format_display::<bool>(row, field),
        FieldType::String => format_display::<String>(row, field),
        FieldType::DateTime => format_display::<chrono::DateTime<chrono::Utc>>(row, field),
        FieldType::Date => format_display::<chrono::NaiveDate>(row, field),
        FieldType::Uuid => format_display::<uuid::Uuid>(row, field),
        FieldType::Json => format_json(row, field),
    }
}

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

fn format_json(row: &PgRow, field: &FieldSchema) -> String {
    // sqlx requires the `json` feature to decode `serde_json::Value`. We
    // don't enable it in v0.1 to keep the dep tree small, so render a
    // placeholder rather than panic.
    let _ = (row, field);
    "<em>JSON (rendering disabled)</em>".to_owned()
}

/// Best-effort string form of a column value for pre-populating form
/// inputs. Returns an empty string for `NULL`, so the input renders blank.
pub(crate) fn render_value_for_input(row: &PgRow, field: &FieldSchema) -> String {
    fn opt_to_string<T: std::fmt::Display>(v: Option<T>) -> String {
        v.map(|x| x.to_string()).unwrap_or_default()
    }
    match field.ty {
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
        FieldType::Json => String::new(),
    }
}

/// Render a form input for `field`, optionally pre-filled with `value`.
/// PK fields get `readonly` when `pk_locked` is true (edit mode).
pub(crate) fn render_input(field: &FieldSchema, value: &str, pk_locked: bool) -> String {
    let name = escape(field.name);
    let val = escape(value);
    let required = if field.nullable || field.ty == FieldType::Bool {
        ""
    } else {
        " required"
    };
    let readonly = if pk_locked && field.primary_key {
        " readonly"
    } else {
        ""
    };

    match field.ty {
        FieldType::Bool => {
            let checked = if value == "true" { " checked" } else { "" };
            format!(
                r#"<input type="checkbox" name="{name}" id="{name}" value="true"{checked}{readonly}>"#
            )
        }
        FieldType::I32 | FieldType::I64 => {
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
            r#"<textarea name="{name}" id="{name}" disabled>JSON editing disabled in v0.1</textarea>"#
        ),
    }
}
