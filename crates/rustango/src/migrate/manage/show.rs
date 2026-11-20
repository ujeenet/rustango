//! `showurls` and `showmodels`.

use super::*;

/// `manage showurls [--format <plain|json>]` — print every named
/// URL pattern registered via `register_url!`.
///
/// Defaults to plain two-column output (name, pattern). `--format
/// json` emits a JSON array of `{"name": "...", "pattern": "..."}`
/// for machine consumption.
///
/// Sorted by name for deterministic output.
pub(super) fn showurls_cmd<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let mut format = "plain";
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                writeln!(w, "showurls [--format <plain|json>]")?;
                writeln!(w)?;
                writeln!(
                    w,
                    "  Print every named URL pattern registered via `register_url!`."
                )?;
                writeln!(w, "  --format plain (default) | json")?;
                return Ok(());
            }
            "--format" => {
                let v = iter.next().ok_or_else(|| missing_value("--format"))?;
                match v.as_str() {
                    "plain" | "json" => {
                        format = match v.as_str() {
                            "plain" => "plain",
                            "json" => "json",
                            _ => unreachable!(),
                        }
                    }
                    other => {
                        return Err(MigrateError::Validation(format!(
                            "--format: unknown value `{other}` (expected `plain` or `json`)"
                        )));
                    }
                }
            }
            other if other.starts_with('-') => {
                return Err(unknown_flag(other));
            }
            other => {
                return Err(unexpected_positional(other));
            }
        }
    }

    let mut routes: Vec<&'static crate::urls::NamedRoute> =
        inventory::iter::<crate::urls::NamedRoute>().collect();
    routes.sort_by_key(|r| r.name);

    match format {
        "json" => {
            let items: Vec<serde_json::Value> = routes
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "name": r.name,
                        "pattern": r.pattern,
                    })
                })
                .collect();
            let rendered = serde_json::to_string_pretty(&items)
                .map_err(|e| MigrateError::Validation(format!("showurls: serialize JSON: {e}")))?;
            writeln!(w, "{rendered}")?;
        }
        _ => {
            // plain
            if routes.is_empty() {
                writeln!(w, "(no named URLs registered)")?;
                return Ok(());
            }
            let max_name = routes.iter().map(|r| r.name.len()).max().unwrap_or(0);
            for r in &routes {
                writeln!(w, "  {:<width$}  {}", r.name, r.pattern, width = max_name)?;
            }
        }
    }
    Ok(())
}

/// `manage showmodels [--format plain|json] [--app <label>]` —
/// print every model registered via `#[derive(Model)]` + `inventory`.
/// Useful for confirming model registration in CI / debugging
/// "where did my model go?" / cross-checking the admin sidebar
/// against the inventory.
///
/// - `--format plain` (default): one row per model. Columns are
///   app, model name, table, field count.
/// - `--format json`: JSON array of `{"app", "name", "table",
///   "fields"}` for piping to `jq`.
/// - `--app <label>`: filter to one app's models. Useful when the
///   binary registers many apps.
///
/// Sorted by `(app, name)` for deterministic output.
pub(super) fn showmodels_cmd<W: Write>(args: &[String], w: &mut W) -> Result<(), MigrateError> {
    let mut format = "plain";
    let mut app_filter: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                writeln!(w, "showmodels [--format plain|json] [--app <label>]")?;
                writeln!(w)?;
                writeln!(
                    w,
                    "  Print every model registered via #[derive(Model)] + inventory."
                )?;
                writeln!(w, "  --format plain (default) | json")?;
                writeln!(w, "  --app <label>          Limit to a single app.")?;
                return Ok(());
            }
            "--format" => {
                let v = iter.next().ok_or_else(|| missing_value("--format"))?;
                match v.as_str() {
                    "plain" => format = "plain",
                    "json" => format = "json",
                    other => {
                        return Err(MigrateError::Validation(format!(
                            "--format: unknown value `{other}` (expected `plain` or `json`)"
                        )));
                    }
                }
            }
            "--app" => {
                let v = iter.next().ok_or_else(|| missing_value("--app"))?;
                app_filter = Some(v.clone());
            }
            other if other.starts_with('-') => {
                return Err(unknown_flag(other));
            }
            other => {
                return Err(unexpected_positional(other));
            }
        }
    }

    // Collect + filter + sort.
    #[derive(Debug)]
    struct Row {
        app: String,
        name: &'static str,
        table: &'static str,
        fields: usize,
    }
    let mut rows: Vec<Row> = inventory::iter::<crate::core::ModelEntry>()
        .map(|entry| Row {
            app: entry.resolved_app_label().unwrap_or("").to_owned(),
            name: entry.schema.name,
            table: entry.schema.table,
            fields: entry.schema.fields.len(),
        })
        .filter(|r| app_filter.as_deref().is_none_or(|f| r.app == f))
        .collect();
    rows.sort_by(|a, b| (a.app.as_str(), a.name).cmp(&(b.app.as_str(), b.name)));

    match format {
        "json" => {
            let items: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "app": r.app,
                        "name": r.name,
                        "table": r.table,
                        "fields": r.fields,
                    })
                })
                .collect();
            let rendered = serde_json::to_string_pretty(&items).map_err(|e| {
                MigrateError::Validation(format!("showmodels: serialize JSON: {e}"))
            })?;
            writeln!(w, "{rendered}")?;
        }
        _ => {
            if rows.is_empty() {
                let suffix = app_filter
                    .as_deref()
                    .map(|f| format!(" for app `{f}`"))
                    .unwrap_or_default();
                writeln!(w, "(no models registered{suffix})")?;
                return Ok(());
            }
            let max_app = rows.iter().map(|r| r.app.len()).max().unwrap_or(0).max(3); // "app"
            let max_name = rows.iter().map(|r| r.name.len()).max().unwrap_or(0).max(5);
            let max_table = rows.iter().map(|r| r.table.len()).max().unwrap_or(0).max(5);
            for r in &rows {
                let app = if r.app.is_empty() {
                    "-"
                } else {
                    r.app.as_str()
                };
                writeln!(
                    w,
                    "  {:<aw$}  {:<nw$}  {:<tw$}  {} fields",
                    app,
                    r.name,
                    r.table,
                    r.fields,
                    aw = max_app,
                    nw = max_name,
                    tw = max_table,
                )?;
            }
        }
    }
    Ok(())
}
