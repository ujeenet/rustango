//! `GET <admin_prefix>/__docs` — in-admin model reference (Django
//! `django.contrib.admindocs` parity, #1011).
//!
//! Renders a read-only reference of every registered, *visible* model
//! grouped by Django-shape app label, with each field's column / type /
//! key / nullability / relation. The data is what the admin registry
//! already holds (the `ModelEntry` inventory + each `ModelSchema`), so
//! this is pure presentation — no DB I/O.
//!
//! Scope: the **models** reference. Django's admindocs also documents
//! views and template tags/filters; those are out of scope for v1
//! (axum routes aren't enumerable at runtime the way Django's URLconf
//! is, and the Tera filter/tag set isn't introspectable).

use axum::extract::State;
use axum::response::Html;
use serde_json::json;

use super::helpers::{chrome_context, inventory_entries_dedup_by_table};
use super::templates::render_with_chrome;
use super::urls::AppState;
use crate::core::Relation;

pub(crate) async fn docs_view(State(state): State<AppState>) -> Html<String> {
    // Group registered models by app label — same grouping the sidebar
    // uses — preserving registration order within each app.
    let mut by_app: indexmap::IndexMap<String, Vec<serde_json::Value>> = indexmap::IndexMap::new();
    for entry in inventory_entries_dedup_by_table() {
        let schema = entry.schema;
        // Respect the admin's table visibility allow/deny config so the
        // docs page never reveals a model the operator hid.
        if !state.is_visible(schema.table) {
            continue;
        }
        let app = entry.resolved_app_label().unwrap_or("(project)").to_owned();
        let fields: Vec<serde_json::Value> = schema
            .fields
            .iter()
            .map(|f| {
                let relation = match &f.relation {
                    Some(Relation::Fk { to, on }) => format!("FK → {to}.{on}"),
                    Some(Relation::O2O { to, on }) => format!("O2O → {to}.{on}"),
                    None => String::new(),
                };
                json!({
                    "name": f.name,
                    "column": f.column,
                    "type": f.ty.to_string(),
                    "pk": f.primary_key,
                    "nullable": f.nullable,
                    "unique": f.unique,
                    "relation": relation,
                })
            })
            .collect();
        by_app.entry(app).or_default().push(json!({
            "model": schema.name,
            "table": schema.table,
            "fields": fields,
        }));
    }

    let apps: Vec<serde_json::Value> = by_app
        .into_iter()
        .map(|(app, models)| json!({ "app": app, "models": models }))
        .collect();

    let mut ctx = json!({ "apps": apps });
    Html(render_with_chrome(
        "docs.html",
        &mut ctx,
        // `__docs` highlights the sidebar "Model reference" link and
        // matches no real table, so no model row is falsely activated.
        chrome_context(&state, Some("__docs")),
    ))
}
