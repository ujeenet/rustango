//! Admin view handlers — Django's `views.py` shape.
//!
//! One async fn per route, each returning either rendered HTML or a
//! redirect. Errors flow through [`AdminError`] which converts to a JSON
//! body with the right HTTP status. Backed by [`super::urls::AppState`].

use std::collections::HashMap;

use axum::extract::{Form, Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use crate::core::{
    inventory, CountQuery, DeleteQuery, Filter, InsertQuery, ModelEntry, Op,
    SearchClause, SelectQuery, SqlValue, UpdateQuery, WhereExpr,
};

use super::errors::AdminError;
use super::forms;
use super::helpers::{
    build_fk_joins, fk_map_from_joined_rows, lookup_model, pager_suffix, render_cell, render_form,
};
use super::render;
use super::templates::render_template;
use super::urls::AppState;

// ============================================================== INDEX

pub(crate) async fn index(State(state): State<AppState>) -> Html<String> {
    // Group registered models by Django-shape app label (slice 9.0g).
    // Each entry's `resolved_app_label()` returns the explicit
    // `#[rustango(app = "...")]` override OR infers from the model's
    // module path. Models with no app label land in a "Project" group.
    let mut entries: Vec<&'static ModelEntry> = inventory::iter::<ModelEntry>
        .into_iter()
        .filter(|e| state.is_visible(e.schema.table))
        .collect();
    entries.sort_by_key(|e| e.schema.name);

    let mut by_app: indexmap::IndexMap<String, Vec<&'static ModelEntry>> = indexmap::IndexMap::new();
    for e in entries {
        let label = e
            .resolved_app_label()
            .map_or_else(|| "Project".to_owned(), str::to_owned);
        by_app.entry(label).or_default().push(e);
    }
    // Apps in alpha order, with "Project" pinned to the bottom so the
    // canonical apps come first in the sidebar.
    let mut groups: Vec<(String, Vec<&'static ModelEntry>)> = by_app.into_iter().collect();
    groups.sort_by(|a, b| match (a.0.as_str(), b.0.as_str()) {
        ("Project", _) => std::cmp::Ordering::Greater,
        (_, "Project") => std::cmp::Ordering::Less,
        _ => a.0.cmp(&b.0),
    });

    let groups_ctx: Vec<serde_json::Value> = groups
        .into_iter()
        .map(|(label, items)| {
            let models_ctx: Vec<serde_json::Value> = items
                .into_iter()
                .map(|e| {
                    serde_json::json!({
                        "name": e.schema.name,
                        "table": e.schema.table,
                        "field_count": e.schema.scalar_fields().count(),
                    })
                })
                .collect();
            serde_json::json!({ "app": label, "models": models_ctx })
        })
        .collect();

    // Flat `models` list kept for back-compat with any user-overridden
    // template that still iterates `models` directly. New template
    // renders from `groups`.
    let flat_models_ctx: Vec<serde_json::Value> = groups_ctx
        .iter()
        .flat_map(|g| {
            g.get("models")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
        })
        .collect();

    Html(render_template(
        "index.html",
        &serde_json::json!({
            "groups": groups_ctx,
            "models": flat_models_ctx,
        }),
    ))
}

// ============================================================== LIST

const PAGE_SIZE: i64 = 50;

/// Reserved query parameters; everything else is treated as a per-field filter.
const RESERVED_PARAMS: &[&str] = &["page", "q"];

#[allow(clippy::too_many_lines)] // mostly linear HTML emission; splitting hurts readability
pub(crate) async fn table_view(
    Path(table): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Result<Html<String>, AdminError> {
    let model = lookup_model(&state, &table).ok_or(AdminError::TableNotFound { table })?;
    let pk_field = model.primary_key();
    let page = params
        .get("page")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(1)
        .max(1);
    let offset = (page - 1) * PAGE_SIZE;
    let q = params
        .get("q")
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    // Build per-field filters from extra query params. Unknown fields and
    // unparseable values are silently dropped — bad URLs shouldn't 500.
    let mut filters: Vec<Filter> = Vec::new();
    let mut active_field_filters: Vec<(&'static str, String)> = Vec::new();
    for (key, value) in &params {
        if RESERVED_PARAMS.contains(&key.as_str()) {
            continue;
        }
        if value.is_empty() {
            continue;
        }
        let Some(field) = model.field(key) else {
            continue;
        };
        let Ok(v) = forms::parse_form_value(field, Some(value)) else {
            continue;
        };
        filters.push(Filter {
            column: field.column,
            op: Op::Eq,
            value: v,
        });
        active_field_filters.push((field.name, value.clone()));
    }

    // Build the search clause from String fields with max_length set.
    let search = q.as_ref().and_then(|qstr| {
        let cols: Vec<&'static str> = model.searchable_fields().map(|f| f.column).collect();
        if cols.is_empty() {
            None
        } else {
            Some(SearchClause {
                columns: cols,
                query: qstr.clone(),
            })
        }
    });

    let where_clause = WhereExpr::and_predicates(filters.clone());
    let total = crate::sql::count_rows(
        &state.pool,
        &CountQuery {
            model,
            where_clause: where_clause.clone(),
        },
    )
    .await?;
    // NOTE: count_rows ignores the search clause; counts are approximate
    // when ?q is set. Acceptable for a v0.2 admin pager.
    let joins = build_fk_joins(&state, model);
    let rows = crate::sql::select_rows(
        &state.pool,
        &SelectQuery {
            model,
            where_clause,
            search: search.clone(),
            joins,
            order_by: vec![],
            limit: Some(PAGE_SIZE),
            offset: Some(offset),
        },
    )
    .await?;

    let fk_map = fk_map_from_joined_rows(&state, model, &rows);

    let last_page = if total == 0 {
        1
    } else {
        ((total - 1) / PAGE_SIZE) + 1
    };
    let read_only = state.is_read_only(model.table);

    // Per-column header label. PK gets a `<small>(pk)</small>` suffix; the
    // template marks the value `| safe` so this raw HTML survives.
    let columns_ctx: Vec<serde_json::Value> = model
        .scalar_fields()
        .map(|f| {
            let label = if f.primary_key {
                format!("{} <small>(pk)</small>", render::escape(f.name))
            } else {
                render::escape(f.name)
            };
            serde_json::json!({ "label": label })
        })
        .collect();

    // Per-row payload. Each cell is pre-rendered HTML (FK link or escaped
    // scalar) and `pk` carries the row's URL fragment for the action link.
    let rows_ctx: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let cells: Vec<String> = model
                .scalar_fields()
                .map(|f| render_cell(row, f, &fk_map))
                .collect();
            let pk = pk_field.map(|pk| render::escape(&render::render_value_for_input(row, pk)));
            serde_json::json!({ "cells": cells, "pk": pk })
        })
        .collect();

    let active_filters_ctx: Vec<serde_json::Value> = active_field_filters
        .iter()
        .map(|(k, v)| serde_json::json!({ "key": k, "value": v }))
        .collect();
    let pager_suffix_str = pager_suffix(q.as_deref(), &active_field_filters);

    Ok(Html(render_template(
        "list.html",
        &serde_json::json!({
            "model": { "name": model.name, "table": model.table },
            "total": total,
            "plural": if total == 1 { "" } else { "s" },
            "read_only": read_only,
            "has_searchable": model.searchable_fields().next().is_some(),
            "q": q.unwrap_or_default(),
            "active_filters": active_filters_ctx,
            "columns": columns_ctx,
            "rows": rows_ctx,
            "page": page,
            "last_page": last_page,
            "pager_suffix": pager_suffix_str,
        }),
    )))
}

// ============================================================== DETAIL

pub(crate) async fn detail_view(
    Path((table, pk_raw)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<Html<String>, AdminError> {
    let model = lookup_model(&state, &table).ok_or(AdminError::TableNotFound {
        table: table.clone(),
    })?;
    let pk_field = model.primary_key().ok_or_else(|| {
        AdminError::Internal(format!("model `{}` has no primary key", model.name))
    })?;
    let pk_value = forms::parse_pk_string(pk_field, &pk_raw).map_err(AdminError::Form)?;

    let row = crate::sql::select_one_row(
        &state.pool,
        &SelectQuery {
            model,
            where_clause: WhereExpr::Predicate(Filter {
                column: pk_field.column,
                op: Op::Eq,
                value: pk_value,
            }),
            search: None,
            joins: build_fk_joins(&state, model),
            order_by: vec![],
            limit: None,
            offset: None,
        },
    )
    .await?
    .ok_or(AdminError::RowNotFound {
        table: table.clone(),
        pk: pk_raw.clone(),
    })?;

    // Read joined FK display values from the same row — no extra queries.
    let fk_map = fk_map_from_joined_rows(&state, model, std::slice::from_ref(&row));

    let cells_ctx: Vec<serde_json::Value> = model
        .scalar_fields()
        .map(|f| {
            serde_json::json!({
                "label": f.name,
                "value": render_cell(&row, f, &fk_map),
            })
        })
        .collect();
    let html = render_template(
        "detail.html",
        &serde_json::json!({
            "model": { "name": model.name, "table": model.table },
            "pk": pk_raw,
            "cells": cells_ctx,
            "read_only": state.is_read_only(model.table),
        }),
    );
    Ok(Html(html))
}

// ============================================================== CREATE

pub(crate) async fn create_form(
    Path(table): Path<String>,
    State(state): State<AppState>,
) -> Result<Html<String>, AdminError> {
    let model = lookup_model(&state, &table).ok_or(AdminError::TableNotFound { table })?;
    if state.is_read_only(model.table) {
        return Err(AdminError::ReadOnly {
            table: model.table.to_owned(),
        });
    }
    Ok(Html(render_form(
        model, None, /* pk_locked */ false, None,
    )))
}

pub(crate) async fn create_submit(
    Path(table): Path<String>,
    State(state): State<AppState>,
    Form(form): Form<HashMap<String, String>>,
) -> Result<Response, AdminError> {
    let model = lookup_model(&state, &table).ok_or(AdminError::TableNotFound {
        table: table.clone(),
    })?;
    if state.is_read_only(model.table) {
        return Err(AdminError::ReadOnly {
            table: model.table.to_owned(),
        });
    }

    let collected = match forms::collect_values(model, &form, &[]) {
        Ok(v) => v,
        Err(e) => {
            // Re-render the form with the error message instead of a 4xx.
            let html = render_form(model, Some(&form), false, Some(&e.to_string()));
            return Ok(Html(html).into_response());
        }
    };
    let (columns, values): (Vec<&'static str>, Vec<SqlValue>) = collected.into_iter().unzip();

    let query = InsertQuery {
        model,
        columns,
        values,
        returning: Vec::new(),
    };
    if let Err(e) = crate::sql::insert(&state.pool, &query).await {
        let html = render_form(model, Some(&form), false, Some(&e.to_string()));
        return Ok(Html(html).into_response());
    }

    // Redirect to the new row's detail page using whatever PK the user supplied.
    let pk_field = model.primary_key().ok_or_else(|| {
        AdminError::Internal(format!("model `{}` has no primary key", model.name))
    })?;
    let pk_value = form.get(pk_field.name).cloned().unwrap_or_default();
    Ok(Redirect::to(&format!("/{}/{}", model.table, pk_value)).into_response())
}

// ============================================================== EDIT

pub(crate) async fn edit_form(
    Path((table, pk_raw)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<Html<String>, AdminError> {
    let model = lookup_model(&state, &table).ok_or(AdminError::TableNotFound {
        table: table.clone(),
    })?;
    let pk_field = model.primary_key().ok_or_else(|| {
        AdminError::Internal(format!("model `{}` has no primary key", model.name))
    })?;
    let pk_value = forms::parse_pk_string(pk_field, &pk_raw).map_err(AdminError::Form)?;

    let row = crate::sql::select_one_row(
        &state.pool,
        &SelectQuery {
            model,
            where_clause: WhereExpr::Predicate(Filter {
                column: pk_field.column,
                op: Op::Eq,
                value: pk_value,
            }),
            search: None,
            joins: vec![],
            order_by: vec![],
            limit: None,
            offset: None,
        },
    )
    .await?
    .ok_or(AdminError::RowNotFound {
        table: table.clone(),
        pk: pk_raw.clone(),
    })?;

    let mut prefill = HashMap::new();
    for f in model.scalar_fields() {
        prefill.insert(f.name.to_owned(), render::render_value_for_input(&row, f));
    }
    Ok(Html(render_form(model, Some(&prefill), true, None)))
}

pub(crate) async fn update_submit(
    Path((table, pk_raw)): Path<(String, String)>,
    State(state): State<AppState>,
    Form(form): Form<HashMap<String, String>>,
) -> Result<Response, AdminError> {
    let model = lookup_model(&state, &table).ok_or(AdminError::TableNotFound {
        table: table.clone(),
    })?;
    if state.is_read_only(model.table) {
        return Err(AdminError::ReadOnly {
            table: model.table.to_owned(),
        });
    }
    let pk_field = model.primary_key().ok_or_else(|| {
        AdminError::Internal(format!("model `{}` has no primary key", model.name))
    })?;
    let pk_value = forms::parse_pk_string(pk_field, &pk_raw).map_err(AdminError::Form)?;

    // Don't include PK in SET — keep identity stable.
    let collected = match forms::collect_values(model, &form, &[pk_field.name]) {
        Ok(v) => v,
        Err(e) => {
            let html = render_form(model, Some(&form), true, Some(&e.to_string()));
            return Ok(Html(html).into_response());
        }
    };
    let assignments: Vec<crate::core::Assignment> = collected
        .into_iter()
        .map(|(column, value)| crate::core::Assignment { column, value })
        .collect();

    let query = UpdateQuery {
        model,
        set: assignments,
        where_clause: WhereExpr::Predicate(Filter {
            column: pk_field.column,
            op: Op::Eq,
            value: pk_value,
        }),
    };
    if let Err(e) = crate::sql::update(&state.pool, &query).await {
        let html = render_form(model, Some(&form), true, Some(&e.to_string()));
        return Ok(Html(html).into_response());
    }
    Ok(Redirect::to(&format!("/{}/{}", model.table, pk_raw)).into_response())
}

// ============================================================== DELETE

pub(crate) async fn delete_submit(
    Path((table, pk_raw)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<Response, AdminError> {
    let model = lookup_model(&state, &table).ok_or(AdminError::TableNotFound {
        table: table.clone(),
    })?;
    if state.is_read_only(model.table) {
        return Err(AdminError::ReadOnly {
            table: model.table.to_owned(),
        });
    }
    let pk_field = model.primary_key().ok_or_else(|| {
        AdminError::Internal(format!("model `{}` has no primary key", model.name))
    })?;
    let pk_value = forms::parse_pk_string(pk_field, &pk_raw).map_err(AdminError::Form)?;

    crate::sql::delete(
        &state.pool,
        &DeleteQuery {
            model,
            where_clause: WhereExpr::Predicate(Filter {
                column: pk_field.column,
                op: Op::Eq,
                value: pk_value,
            }),
        },
    )
    .await?;

    Ok(Redirect::to(&format!("/{}", model.table)).into_response())
}
