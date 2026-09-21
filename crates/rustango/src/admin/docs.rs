//! `GET <admin_prefix>/__docs`: an in-admin model reference, like
//! Django's `admindocs`.
//!
//! Lists every registered, visible model, grouped by app label, with
//! each field's column, type, key, nullability and relation. All of it
//! comes from the admin registry, so the page does no database work.
//!
//! Only models are documented. Axum routes cannot be listed at
//! runtime the way Django's URLconf can, and the Tera filter and tag
//! set cannot be inspected either.

use axum::extract::State;
use axum::response::Html;
use serde_json::json;

use super::helpers::{chrome_context, inventory_entries_dedup_by_table};
use super::templates::render_with_chrome;
use super::urls::AppState;
use crate::core::Relation;

pub(crate) async fn docs_view(State(state): State<AppState>) -> Html<String> {
    // Group models by app label, as the sidebar does, keeping
    // registration order inside each app.
    let mut by_app: indexmap::IndexMap<String, Vec<serde_json::Value>> = indexmap::IndexMap::new();
    for entry in inventory_entries_dedup_by_table() {
        let schema = entry.schema;
        // Honour the admin's visibility config, so this page never
        // reveals a model the operator hid.
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
        // `__docs` highlights the sidebar "Model reference" link. It
        // matches no real table, so no model row lights up.
        chrome_context(&state, Some("__docs")),
    ))
}
