//! HTML rendering helpers — handful of `format!` and per-`FieldType`
//! decoders. No template engine on purpose: keeps the dep tree small and
//! the output trivially auditable. We can swap in `maud` later if it
//! becomes worth it.

use std::fmt::Write as _;

use crate::core::{FieldSchema, FieldType};
#[cfg(feature = "postgres")]
use crate::sql::sqlx::{postgres::PgRow, Row};

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
        FieldType::Time => v.as_str().unwrap_or("").to_owned(),
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
        // Decimal round-trips as a string via `row_to_json` to
        // preserve precision; Binary likewise (hex-encoded).
        FieldType::Decimal | FieldType::Binary => v.as_str().unwrap_or("").to_owned(),
        // #341 — PG array: render the JSON form compactly.
        FieldType::Array(_) => v.to_string(),
        // #343 — PG range: literal string (or compact JSON fallback).
        FieldType::Range(_) => v.as_str().map_or_else(|| v.to_string(), str::to_owned),
        // #342 — PG hstore: compact JSON object form.
        FieldType::HStore => v.to_string(),
        // #824 — pgvector: compact JSON array form.
        FieldType::Vector(_) => v.to_string(),
        // #443 — PostGIS Point: compact JSON object form.
        FieldType::Geometry(_) => v.to_string(),
        // #444 — PostGIS raster: hex-WKB string form.
        FieldType::Raster => v.to_string(),
    }
}

/// Render a `<select>` widget over registered ContentType rows, for
/// the `ct_column` of a `#[rustango(generic_fk(...))]` declaration.
/// Issue #244 — replaces the raw `<input type="number">` an operator
/// would otherwise have to fill with a `rustango_content_types.id`
/// integer they'd have to memorize.
///
/// `value` is the currently-selected CT id as a string (numeric).
/// Empty string renders no row pre-selected. `readonly` true emits a
/// `disabled` attribute (matches the read-only feel `pk_locked` gives
/// the regular [`render_input`] path).
pub(crate) fn render_gfk_select(
    field: &FieldSchema,
    value: &str,
    readonly: bool,
    cts: &[crate::contenttypes::ContentType],
) -> String {
    let name = escape(field.name);
    let disabled = if readonly { " disabled" } else { "" };
    let required = if field.nullable || field.primary_key {
        ""
    } else {
        " required"
    };
    let mut out = format!(r#"<select name="{name}" id="{name}"{required}{disabled}>"#,);
    out.push_str(r#"<option value="">— choose target —</option>"#);
    for ct in cts {
        let Some(id) = ct.id.get().copied() else {
            continue;
        };
        let selected = if value == id.to_string() {
            " selected"
        } else {
            ""
        };
        let label = escape(&format!("{}.{}", ct.app_label, ct.model_name));
        let _ = write!(out, r#"<option value="{id}"{selected}>{label}</option>"#);
    }
    out.push_str("</select>");
    out
}

/// Render a form input for `field`, optionally pre-filled with `value`.
/// PK fields get `readonly` when `pk_locked` is true (edit mode).
///
/// Delegates to [`render_input_with_widget`] with no widget override.
/// Call sites that have an [`AdminConfig`](crate::core::AdminConfig)
/// in scope should use [`render_input_with_widget`] directly and pass
/// the override (#359) so per-model `formfield_overrides` apply.
pub(crate) fn render_input(field: &FieldSchema, value: &str, pk_locked: bool) -> String {
    render_input_with_widget(field, value, pk_locked, None)
}

/// Same as [`render_input`] but consults an explicit widget override.
///
/// Django-shape `formfield_overrides` (#359). When `widget` is
/// `Some(name)` and matches a built-in widget identifier, the
/// emitted markup matches the override instead of the FieldType
/// default. Unknown names log a single tracing warning and fall
/// through to the default — typos shouldn't render an empty cell.
///
/// Built-in widget names + the FieldType(s) they apply to:
///
/// - `"password"` (String) — `<input type="password">`
/// - `"hidden"` (any) — `<input type="hidden">`
/// - `"textarea"` (String) — force `<textarea>`
/// - `"color"` (String) — `<input type="color">`
/// - `"range"` (I16/I32/I64) — `<input type="range">`
/// - `"email"` (String) — `<input type="email">`
/// - `"url"` (String) — `<input type="url">`
/// - `"tel"` (String) — `<input type="tel">`
/// - `"search"` (String) — `<input type="search">`
///
/// `"hidden"` is the only widget that applies regardless of
/// FieldType — useful for sealed inputs the operator shouldn't see
/// or edit. The others fall back to the FieldType default when
/// applied to an incompatible type (e.g. `"color"` on an integer)
/// so the override never produces a broken form.
pub(crate) fn render_input_with_widget(
    field: &FieldSchema,
    value: &str,
    pk_locked: bool,
    widget: Option<&str>,
) -> String {
    if let Some(name) = widget {
        if let Some(html) = render_named_widget(field, value, pk_locked, name) {
            return html;
        }
        tracing::warn!(
            target: "rustango::admin",
            field = %field.name,
            widget = %name,
            "unknown or incompatible formfield_overrides widget — falling back to FieldType default"
        );
    }
    render_input_default(field, value, pk_locked)
}

/// Default FieldType-derived widget — extracted so
/// [`render_input_with_widget`] can fall through to it after a
/// widget-override miss without duplicating the dispatch table.
fn render_input_default(field: &FieldSchema, value: &str, pk_locked: bool) -> String {
    let name = escape(field.name);
    let val = escape(value);
    // #445 — Django-shape `blank = true` drops the `required` HTML
    // attribute even on NOT-NULL columns (form may submit empty
    // even when the DB is NOT NULL — empty string is a valid
    // non-null value for CharField).
    let required = if field.nullable
        || field.ty == FieldType::Bool
        || field.auto
        || field.primary_key
        || field.blank
    {
        ""
    } else {
        " required"
    };
    // Caller passes `pk_locked=true` to mean "render this field as
    // read-only" — used both for PKs on edit forms (slice 10.5
    // pre-existing behavior) and for `readonly_fields` flagged via
    // `#[rustango(admin(...))]`.
    let readonly = if pk_locked { " readonly" } else { "" };

    // `#[rustango(choices = "...")]` renders as a `<select>` regardless of
    // FieldType. The option values are emitted as-is and the admin form
    // parser already accepts the string back through the field's column.
    if let Some(choices) = field.choices {
        let disabled = if pk_locked { " disabled" } else { "" };
        let mut out = format!(r#"<select name="{name}" id="{name}"{required}{disabled}>"#);
        if field.nullable {
            out.push_str(r#"<option value=""></option>"#);
        }
        for (v, label) in choices {
            let v_esc = escape(v);
            let label_esc = escape(label);
            let selected = if value == *v { " selected" } else { "" };
            let _ = write!(
                out,
                r#"<option value="{v_esc}"{selected}>{label_esc}</option>"#
            );
        }
        out.push_str("</select>");
        return out;
    }

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
        FieldType::Time => format!(
            r#"<input type="time" name="{name}" id="{name}" value="{val}" step="1"{required}{readonly}>"#
        ),
        FieldType::Uuid => format!(
            r#"<input type="text" name="{name}" id="{name}" value="{val}" pattern="[0-9a-fA-F\-]+"{required}{readonly}>"#
        ),
        FieldType::Json => format!(
            r#"<textarea name="{name}" id="{name}"{readonly} style="font-family:monospace">{val}</textarea>"#
        ),
        // `step="any"` on `type="number"` matches Django's
        // DecimalField widget. Browsers handle precision via the
        // `step` attribute when present; we omit it for now.
        FieldType::Decimal => format!(
            r#"<input type="number" step="any" inputmode="decimal" name="{name}" id="{name}" value="{val}"{required}{readonly}>"#
        ),
        // Binary inputs are typically uploads, but for admin
        // consistency we expose a hex text field; the form parser
        // accepts lowercase hex (even length).
        FieldType::Binary => format!(
            r#"<input type="text" name="{name}" id="{name}" value="{val}" pattern="[0-9a-f]*" inputmode="latin"{readonly} style="font-family:monospace">"#
        ),
        // #341 — PG array: comma-separated text input (the form parser
        // splits on `,`). Placeholder hints the expected shape.
        FieldType::Array(_) => format!(
            r#"<input type="text" name="{name}" id="{name}" value="{val}" placeholder="comma, separated, values"{required}{readonly}>"#
        ),
        // #343 — PG range: a range-literal text input (`[lower,upper)`).
        FieldType::Range(_) => format!(
            r#"<input type="text" name="{name}" id="{name}" value="{val}" placeholder="[lower,upper)"{required}{readonly}>"#
        ),
        // #342 — PG hstore: a JSON-object text input (`{{"k":"v"}}`).
        FieldType::HStore => format!(
            r#"<input type="text" name="{name}" id="{name}" value="{val}" placeholder="{{&quot;key&quot;: &quot;value&quot;}}"{required}{readonly}>"#
        ),
        // #824 — pgvector: a JSON-array text input (`[0.1, 0.2, ...]`).
        FieldType::Vector(_) => format!(
            r#"<input type="text" name="{name}" id="{name}" value="{val}" placeholder="[0.1, 0.2, ...]"{required}{readonly}>"#
        ),
        // #443 — PostGIS Point: a JSON-object text input (`{{"x":..,"y":..}}`).
        FieldType::Geometry(_) => format!(
            r#"<input type="text" name="{name}" id="{name}" value="{val}" placeholder="{{&quot;x&quot;: 0, &quot;y&quot;: 0, &quot;srid&quot;: 4326}}"{required}{readonly}>"#
        ),
        // #444 — PostGIS raster: a hex-WKB text input (rarely hand-edited).
        FieldType::Raster => format!(
            r#"<input type="text" name="{name}" id="{name}" value="{val}" placeholder="hex WKB raster"{required}{readonly}>"#
        ),
    }
}

/// Render the named built-in widget override (#359). Returns `None`
/// when the widget name is unknown OR is incompatible with the
/// field's [`FieldType`] (e.g. `"color"` applied to an integer),
/// signaling the caller to fall through to
/// [`render_input_default`].
///
/// `"hidden"` is the only widget that applies to every FieldType.
fn render_named_widget(
    field: &FieldSchema,
    value: &str,
    pk_locked: bool,
    widget: &str,
) -> Option<String> {
    let name = escape(field.name);
    let val = escape(value);
    let required = if field.nullable
        || field.ty == FieldType::Bool
        || field.auto
        || field.primary_key
        || field.blank
    {
        ""
    } else {
        " required"
    };
    let readonly = if pk_locked { " readonly" } else { "" };

    match widget {
        // `"hidden"` is the universal override — any FieldType.
        "hidden" => Some(format!(
            r#"<input type="hidden" name="{name}" id="{name}" value="{val}">"#
        )),
        // String-typed widgets.
        "password" if matches!(field.ty, FieldType::String) => Some(format!(
            r#"<input type="password" name="{name}" id="{name}" value="{val}"{required}{readonly}>"#
        )),
        "textarea" if matches!(field.ty, FieldType::String) => {
            let maxlen = field
                .max_length
                .map(|n| format!(r#" maxlength="{n}""#))
                .unwrap_or_default();
            Some(format!(
                r#"<textarea name="{name}" id="{name}"{maxlen}{required}{readonly}>{val}</textarea>"#
            ))
        }
        "color" if matches!(field.ty, FieldType::String) => Some(format!(
            r#"<input type="color" name="{name}" id="{name}" value="{val}"{required}{readonly}>"#
        )),
        "email" if matches!(field.ty, FieldType::String) => Some(format!(
            r#"<input type="email" name="{name}" id="{name}" value="{val}"{required}{readonly}>"#
        )),
        "url" if matches!(field.ty, FieldType::String) => Some(format!(
            r#"<input type="url" name="{name}" id="{name}" value="{val}"{required}{readonly}>"#
        )),
        "tel" if matches!(field.ty, FieldType::String) => Some(format!(
            r#"<input type="tel" name="{name}" id="{name}" value="{val}"{required}{readonly}>"#
        )),
        "search" if matches!(field.ty, FieldType::String) => Some(format!(
            r#"<input type="search" name="{name}" id="{name}" value="{val}"{required}{readonly}>"#
        )),
        // Integer-typed widgets.
        "range" if matches!(field.ty, FieldType::I16 | FieldType::I32 | FieldType::I64) => {
            let mut attrs = String::new();
            if let Some(min) = field.min {
                attrs.push_str(&format!(r#" min="{min}""#));
            }
            if let Some(max) = field.max {
                attrs.push_str(&format!(r#" max="{max}""#));
            }
            Some(format!(
                r#"<input type="range" step="1" name="{name}" id="{name}" value="{val}"{attrs}{readonly}>"#
            ))
        }
        // Unknown name or type mismatch → caller falls through to
        // the FieldType default. The warning is logged at the
        // dispatch site (`render_input_with_widget`).
        _ => None,
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
// `select_rows_as_json` → these renderers, so the rendering
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
        FieldType::String
        | FieldType::Uuid
        | FieldType::Date
        | FieldType::Time
        | FieldType::DateTime
        | FieldType::Decimal => escape(v.as_str().unwrap_or("")),
        FieldType::Binary => {
            // Hex-encoded by `row_to_json*`; render the head + a
            // truncation marker so list cells stay readable.
            let s = v.as_str().unwrap_or("");
            if s.len() > 16 {
                escape(&format!("{}… ({} hex chars)", &s[..16], s.len()))
            } else {
                escape(s)
            }
        }
        FieldType::Json => {
            // Compact serialize to keep list cells one-line; the
            // detail view template can render expanded.
            serde_json::to_string(v)
                .map(|s| escape(&s))
                .unwrap_or_default()
        }
        // #341 — PG array: compact JSON form in the list cell.
        FieldType::Array(_) => serde_json::to_string(v)
            .map(|s| escape(&s))
            .unwrap_or_default(),
        // #343 — PG range: literal string in the list cell.
        FieldType::Range(_) => escape(v.as_str().unwrap_or("")),
        // #342 — PG hstore: compact JSON object in the list cell.
        FieldType::HStore => serde_json::to_string(v)
            .map(|s| escape(&s))
            .unwrap_or_default(),
        // #824 — pgvector: compact JSON array in the list cell.
        FieldType::Vector(_) => serde_json::to_string(v)
            .map(|s| escape(&s))
            .unwrap_or_default(),
        // #443 — PostGIS Point: compact JSON object in the list cell.
        FieldType::Geometry(_) => serde_json::to_string(v)
            .map(|s| escape(&s))
            .unwrap_or_default(),
        // #444 — PostGIS raster: hex-WKB string in the list cell.
        FieldType::Raster => serde_json::to_string(v)
            .map(|s| escape(&s))
            .unwrap_or_default(),
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
        FieldType::String
        | FieldType::Uuid
        | FieldType::Date
        | FieldType::Time
        | FieldType::DateTime
        | FieldType::Decimal => v.as_str().map(str::to_owned),
        FieldType::Binary => v.as_str().map(|s| {
            if s.len() > 16 {
                format!("{}… ({} hex chars)", &s[..16], s.len())
            } else {
                s.to_owned()
            }
        }),
        FieldType::Json => None,
        // #341 — arrays aren't a meaningful select_related join target.
        FieldType::Array(_) => None,
        // #343 — ranges aren't a meaningful select_related join target.
        FieldType::Range(_) => None,
        // #342 — hstore isn't a meaningful select_related join target.
        FieldType::HStore => None,
        // #824 — vector isn't a meaningful select_related join target.
        FieldType::Vector(_) => None,
        // #443 — geometry isn't a meaningful select_related join target.
        FieldType::Geometry(_) => None,
        // #444 — raster isn't a meaningful select_related join target.
        FieldType::Raster => None,
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
            help_text: None,
            choices: None,
            db_comment: None,
            verbose_name: None,
            editable: true,
            blank: false,
            case_insensitive: false,
            fk_on_delete: None,
            validators: &[],
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

    /// `#[rustango(choices = "...")]` swaps the rendered `<input>` for a
    /// `<select>` populated with the declared options, with the current
    /// value pre-selected. NULL columns get a blank leading option so
    /// the user can clear the field; NOT NULL columns omit it.
    #[test]
    fn render_input_emits_select_when_choices_present() {
        let mut f = field("status", "status", FieldType::String);
        f.choices = Some(&[("draft", "Draft"), ("published", "Published")]);
        f.nullable = false;

        let html = render_input(&f, "published", false);
        assert!(
            html.starts_with("<select "),
            "expected <select>, got: {html}"
        );
        assert!(html.contains(r#"<option value="draft">Draft</option>"#));
        assert!(html.contains(r#"<option value="published" selected>Published</option>"#));
        assert!(
            !html.contains(r#"<option value="">"#),
            "NOT NULL field should not have empty option, got: {html}"
        );
        assert!(html.contains(" required"));
    }

    #[test]
    fn render_input_choices_include_blank_option_when_nullable() {
        let mut f = field("status", "status", FieldType::String);
        f.choices = Some(&[("a", "Alpha"), ("b", "Beta")]);
        f.nullable = true;

        let html = render_input(&f, "", false);
        assert!(html.contains(r#"<option value=""></option>"#));
        // No selection ⇒ no "selected" attribute on any option
        assert!(!html.contains("selected"));
        // Nullable ⇒ no `required` attribute
        assert!(!html.contains(" required"));
    }

    #[test]
    fn render_input_choices_escape_html() {
        let mut f = field("status", "status", FieldType::String);
        f.choices = Some(&[(r#"<a>"#, r#"<b>"#)]);
        f.nullable = false;

        let html = render_input(&f, "<a>", false);
        assert!(html.contains("&lt;a&gt;"));
        assert!(html.contains("&lt;b&gt;"));
        assert!(!html.contains("<a>"));
        assert!(!html.contains("<b>"));
    }

    /// `#[rustango(blank)]` drops the `required` HTML attribute even
    /// on NOT NULL columns — Django-shape "form may submit empty even
    /// when DB is NOT NULL" semantics (#445).
    #[test]
    fn render_input_blank_drops_required_on_not_null_column() {
        let mut f = field("subtitle", "subtitle", FieldType::String);
        f.nullable = false; // DB-side NOT NULL
        f.blank = true; // form-side allow empty
        f.max_length = Some(50);

        let html = render_input(&f, "", false);
        assert!(
            !html.contains(" required"),
            "blank=true should drop `required`, got: {html}"
        );
    }

    /// NOT NULL + blank=false (default) still emits `required`.
    #[test]
    fn render_input_keeps_required_on_not_null_when_blank_false() {
        let mut f = field("title", "title", FieldType::String);
        f.nullable = false;
        f.blank = false;
        f.max_length = Some(50);

        let html = render_input(&f, "", false);
        assert!(
            html.contains(" required"),
            "NOT NULL non-blank field should be required, got: {html}"
        );
    }
}
