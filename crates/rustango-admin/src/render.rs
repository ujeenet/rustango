//! HTML rendering helpers — handful of `format!` and per-`FieldType`
//! decoders. No template engine on purpose: keeps the dep tree small and
//! the output trivially auditable. We can swap in `maud` later if it
//! becomes worth it.

use rustango_core::{FieldSchema, FieldType};
use rustango_sql::sqlx::{self, postgres::PgRow, Postgres, Row};

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
