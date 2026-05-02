//! Admin view handlers — Django's `views.py` shape.
//!
//! One async fn per route, each returning either rendered HTML or a
//! redirect. Errors flow through [`AdminError`] which converts to a JSON
//! body with the right HTTP status. Backed by [`super::urls::AppState`].

use std::collections::HashMap;

use axum::extract::{Form, Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use crate::core::{
    inventory, Assignment, CountQuery, DeleteQuery, FieldSchema, Filter, InsertQuery, ModelEntry,
    Op, SearchClause, SelectQuery, SqlValue, UpdateQuery, WhereExpr,
};

use super::errors::AdminError;
use super::forms;
use super::helpers::{
    build_fk_joins, chrome_context, fk_map_from_joined_rows, lookup_model, pager_suffix,
    render_cell, render_form,
};
use super::render;
use super::templates::render_with_chrome;
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

    let mut ctx = serde_json::json!({
        "groups": groups_ctx,
        "models": flat_models_ctx,
    });
    Html(render_with_chrome(
        "index.html",
        &mut ctx,
        chrome_context(&state, None),
    ))
}

// ============================================================== LIST

/// Default page size when the model's `admin.list_per_page == 0`.
const DEFAULT_PAGE_SIZE: i64 = 50;

/// Reserved query parameters; everything else is treated as a per-field filter.
const RESERVED_PARAMS: &[&str] = &["page", "q", "facet_show_all"];

/// Default cap on how many values a single facet shows. v0.13.1 —
/// keeps the right rail compact on high-cardinality columns. The
/// remainder collapses into a "+N more" link that opts the column
/// into showing every value via `?facet_show_all=<field>`.
const FACET_TRUNCATE: usize = 15;

#[allow(clippy::too_many_lines)] // mostly linear HTML emission; splitting hurts readability
pub(crate) async fn table_view(
    Path(table): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Result<Html<String>, AdminError> {
    let model = lookup_model(&state, &table).ok_or(AdminError::TableNotFound { table })?;
    let pk_field = model.primary_key();
    let admin_cfg = model
        .admin
        .copied()
        .unwrap_or(crate::core::AdminConfig::DEFAULT);
    // Resolve per-model page size (fall back to framework default when unset).
    let page_size: i64 = if admin_cfg.list_per_page == 0 {
        DEFAULT_PAGE_SIZE
    } else {
        admin_cfg.list_per_page as i64
    };
    let page = params
        .get("page")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(1)
        .max(1);
    let offset = (page - 1) * page_size;
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

    // Build the search clause. If `admin.search_fields` is set, that's
    // the list (Django shape). Otherwise fall back to fields whose
    // `searchable` flag is true on `FieldSchema` (today's auto behavior).
    let search_columns: Vec<&'static str> = if admin_cfg.search_fields.is_empty() {
        model.searchable_fields().map(|f| f.column).collect()
    } else {
        admin_cfg
            .search_fields
            .iter()
            .filter_map(|name| model.field(name).map(|f| f.column))
            .collect()
    };
    let search = q.as_ref().and_then(|qstr| {
        if search_columns.is_empty() {
            None
        } else {
            Some(SearchClause {
                columns: search_columns.clone(),
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
    // Default ordering: PK ASC unless `admin.ordering` overrides.
    let order_by: Vec<crate::core::OrderClause> = if admin_cfg.ordering.is_empty() {
        Vec::new()
    } else {
        admin_cfg
            .ordering
            .iter()
            .filter_map(|(name, desc)| {
                model.field(name).map(|f| crate::core::OrderClause {
                    column: f.column,
                    desc: *desc,
                })
            })
            .collect()
    };
    let rows = crate::sql::select_rows(
        &state.pool,
        &SelectQuery {
            model,
            where_clause,
            search: search.clone(),
            joins,
            order_by,
            limit: Some(page_size),
            offset: Some(offset),
        },
    )
    .await?;

    let fk_map = fk_map_from_joined_rows(&state, model, &rows);

    let last_page = if total == 0 {
        1
    } else {
        ((total - 1) / page_size) + 1
    };
    let read_only = state.is_read_only(model.table);

    // Resolve the columns shown on the list. If `admin.list_display`
    // is set, use those (in order, validated at compile time against
    // declared field names — `model.field(name)` is `Some` here);
    // otherwise show every scalar field.
    let display_fields: Vec<&'static FieldSchema> = if admin_cfg.list_display.is_empty() {
        model.scalar_fields().collect()
    } else {
        admin_cfg
            .list_display
            .iter()
            .filter_map(|name| model.field(name))
            .collect()
    };

    // Per-column header label. PK gets a `<small>(pk)</small>` suffix; the
    // template marks the value `| safe` so this raw HTML survives.
    let columns_ctx: Vec<serde_json::Value> = display_fields
        .iter()
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
            let cells: Vec<String> = display_fields
                .iter()
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

    // Facet filters (slice 10.4). For each field named in
    // `admin.list_filter`, query its distinct values + counts and
    // render a right-rail card. Each value link toggles the
    // `?<col>=<value>` query param: clicking the active value clears
    // the filter; clicking a different value swaps to it.
    let show_all_facet = params
        .get("facet_show_all")
        .map(String::as_str);
    let facets_ctx: Vec<serde_json::Value> = compute_facets(
        &state,
        model,
        &admin_cfg,
        &active_field_filters,
        q.as_deref(),
        show_all_facet,
    )
    .await?;

    // Action menu items (slice 10.6). Empty when the model declares
    // no `admin.actions`, hiding the picker entirely.
    let actions_ctx: Vec<serde_json::Value> = admin_cfg
        .actions
        .iter()
        .map(|name| {
            let label = match *name {
                "delete_selected" => "Delete selected".to_owned(),
                other => other.replace('_', " "),
            };
            serde_json::json!({ "name": name, "label": label })
        })
        .collect();

    let mut ctx = serde_json::json!({
        "model": { "name": model.name, "table": model.table },
        "total": total,
        "plural": if total == 1 { "" } else { "s" },
        "read_only": read_only,
        "has_searchable": !search_columns.is_empty(),
        "q": q.unwrap_or_default(),
        "active_filters": active_filters_ctx,
        "facets": facets_ctx,
        "actions": actions_ctx,
        "columns": columns_ctx,
        "rows": rows_ctx,
        "page": page,
        "last_page": last_page,
        "pager_suffix": pager_suffix_str,
    });
    Ok(Html(render_with_chrome(
        "list.html",
        &mut ctx,
        chrome_context(&state, Some(model.table)),
    )))
}

/// Slice 10.4 — for each `admin.list_filter` field, compute the
/// distinct values + row counts and the URL each value should toggle to.
///
/// SQL is one round-trip per facet field: `SELECT <col>, COUNT(*) FROM
/// <table> GROUP BY <col> ORDER BY <col>`. For dynamic admin pages
/// this is acceptable (handful of facets, modest cardinalities); if a
/// model has 50k distinct values per facet the operator should drop
/// the field from `list_filter`. FK columns get the JOINed display
/// value rendered alongside the raw key for readability.
///
/// Toggle semantics: clicking the active value's link omits that
/// filter from the URL (clears it); clicking a sibling sets it.
async fn compute_facets(
    state: &AppState,
    model: &'static crate::core::ModelSchema,
    admin_cfg: &crate::core::AdminConfig,
    active_field_filters: &[(&'static str, String)],
    q: Option<&str>,
    show_all_facet: Option<&str>,
) -> Result<Vec<serde_json::Value>, AdminError> {
    if admin_cfg.list_filter.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(admin_cfg.list_filter.len());
    for filter_name in admin_cfg.list_filter {
        let Some(field) = model.field(filter_name) else {
            continue;
        };
        let active_value: Option<&str> = active_field_filters
            .iter()
            .find(|(k, _)| k == &field.name)
            .map(|(_, v)| v.as_str());

        // Slice 10.7 — for FK fields, JOIN to the target table on its
        // display column so the facet card shows "Dr. Maeve O'Hara
        // (3)" instead of "1 (3)". Falls back to raw value for
        // non-FK fields, FKs whose target isn't visible in the admin,
        // or FK targets without a `display = "..."` attribute.
        let fk_join: Option<(&'static str, &'static str, &'static str)> =
            field.relation.and_then(|rel| match rel {
                crate::core::Relation::Fk { to, on }
                | crate::core::Relation::O2O { to, on } => {
                    let target = lookup_model(state, to)?;
                    let display_field = target.display_field()?;
                    Some((target.table, on, display_field.column))
                }
            });

        // v0.13.1: order facets by count desc so the most active
        // value floats to the top. Tie-break alphabetically by the
        // displayed value so output stays deterministic across
        // requests.
        let sql = if let Some((target_table, target_pk, display_col)) = fk_join {
            format!(
                r#"SELECT "{src_table}"."{col}" AS facet_value,
                          "{target_table}"."{display_col}" AS facet_display,
                          COUNT(*) AS facet_count
                   FROM "{src_table}"
                   LEFT JOIN "{target_table}"
                     ON "{target_table}"."{target_pk}" = "{src_table}"."{col}"
                   GROUP BY "{src_table}"."{col}", "{target_table}"."{display_col}"
                   ORDER BY facet_count DESC, "{target_table}"."{display_col}""#,
                col = field.column.replace('"', "\"\""),
                src_table = model.table.replace('"', "\"\""),
                target_table = target_table.replace('"', "\"\""),
                target_pk = target_pk.replace('"', "\"\""),
                display_col = display_col.replace('"', "\"\""),
            )
        } else {
            format!(
                r#"SELECT "{col}" AS facet_value, COUNT(*) AS facet_count
                   FROM "{table}"
                   GROUP BY "{col}"
                   ORDER BY facet_count DESC, "{col}""#,
                col = field.column.replace('"', "\"\""),
                table = model.table.replace('"', "\"\""),
            )
        };
        let rows = sqlx::query(&sql).fetch_all(&state.pool).await?;
        let mut values = Vec::with_capacity(rows.len());
        for row in &rows {
            // Stringify the value at the `facet_value` column alias.
            // Same shape `parse_form_value` accepts back when the URL
            // round-trips through the filter machinery.
            let raw = render::read_value_as_string_at(row, field, "facet_value")
                .unwrap_or_default();
            // Display: for FK fields with a JOIN, prefer the target's
            // display value; otherwise fall back to the raw key.
            let display = if raw.is_empty() {
                "—".to_owned()
            } else if fk_join.is_some() {
                let display_text: Option<String> = sqlx::Row::try_get::<Option<String>, _>(
                    row,
                    "facet_display",
                )
                .ok()
                .flatten();
                match display_text {
                    Some(t) if !t.is_empty() => render::escape(&t),
                    _ => render::escape(&raw),
                }
            } else {
                render::escape(&raw)
            };
            let count: i64 = sqlx::Row::try_get(row, "facet_count").unwrap_or(0);
            let is_active = active_value.map(|v| v == raw).unwrap_or(false);
            // Build the toggle URL: drop this filter when active, else
            // set it. Other active filters + ?q= are preserved.
            let mut params: Vec<(String, String)> = Vec::new();
            if let Some(qv) = q {
                params.push(("q".into(), qv.into()));
            }
            for (k, v) in active_field_filters {
                if *k == field.name {
                    continue; // dropped (or replaced below)
                }
                params.push(((*k).into(), v.clone()));
            }
            if !is_active {
                params.push((field.name.into(), raw.clone()));
            }
            let toggle_url = build_query_url(model.table, &params);

            values.push(serde_json::json!({
                "raw": raw,
                "display": display,
                "count": count,
                "active": is_active,
                "toggle_url": toggle_url,
            }));
        }
        // v0.13.1: truncate to FACET_TRUNCATE values unless the
        // operator opted into "show all" for this column. Active
        // filters always render so an active value never disappears
        // behind the cutoff (counted toward the truncate budget).
        let show_all = show_all_facet == Some(field.name);
        let total_values = values.len();
        let mut more_count: usize = 0;
        if !show_all && total_values > FACET_TRUNCATE {
            // Keep every active value + as many of the rest as fit.
            let mut active_first: Vec<serde_json::Value> = Vec::new();
            let mut rest: Vec<serde_json::Value> = Vec::new();
            for v in values.into_iter() {
                if v.get("active").and_then(|b| b.as_bool()).unwrap_or(false) {
                    active_first.push(v);
                } else {
                    rest.push(v);
                }
            }
            let cap = FACET_TRUNCATE.saturating_sub(active_first.len());
            let kept_rest_len = rest.len().min(cap);
            more_count = total_values - active_first.len() - kept_rest_len;
            active_first.extend(rest.into_iter().take(cap));
            values = active_first;
        }
        let show_all_url = if more_count > 0 {
            // Build a URL that preserves current filters AND adds
            // `facet_show_all=<field>`. Click swaps the truncated
            // list to the full distinct-value list.
            let mut params: Vec<(String, String)> = Vec::new();
            if let Some(qv) = q {
                params.push(("q".into(), qv.into()));
            }
            for (k, v) in active_field_filters {
                params.push(((*k).into(), v.clone()));
            }
            params.push(("facet_show_all".into(), field.name.into()));
            Some(build_query_url(model.table, &params))
        } else {
            None
        };
        // For FK facets, build a "clear" URL (removes this filter) used
        // as the "All" option in the <select> dropdown renderer.
        let clear_url = if fk_join.is_some() {
            let mut params: Vec<(String, String)> = Vec::new();
            if let Some(qv) = q {
                params.push(("q".into(), qv.into()));
            }
            for (k, v) in active_field_filters {
                if *k == field.name {
                    continue;
                }
                params.push(((*k).into(), v.clone()));
            }
            Some(build_query_url(model.table, &params))
        } else {
            None
        };
        out.push(serde_json::json!({
            "field": field.name,
            "is_fk": fk_join.is_some(),
            "values": values,
            "more_count": more_count,
            "show_all_url": show_all_url,
            "clear_url": clear_url,
        }));
    }
    Ok(out)
}

fn build_query_url(table: &str, params: &[(String, String)]) -> String {
    if params.is_empty() {
        format!("/__admin/{table}")
    } else {
        let qs: Vec<String> = params
            .iter()
            .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
            .collect();
        format!("/__admin/{table}?{}", qs.join("&"))
    }
}

fn url_encode(s: &str) -> String {
    // Bare-minimum percent-encoding for query-string values: spaces
    // and the seven query-control characters. Avoids pulling in
    // `urlencoding` for one call site.
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '&' => out.push_str("%26"),
            '=' => out.push_str("%3D"),
            '?' => out.push_str("%3F"),
            '#' => out.push_str("%23"),
            '+' => out.push_str("%2B"),
            '%' => out.push_str("%25"),
            _ => out.push(c),
        }
    }
    out
}

// ============================================================== AUDIT LOG
//
// Moved to `super::audit` in v0.13.0. The route handlers
// (`audit_log_view`, `audit_cleanup_submit`) and the per-write
// emit helpers (`emit_admin_audit`, `emit_admin_audit_diff`) now
// live there. `urls.rs` routes to `super::audit::*` directly;
// `views.rs` calls `super::audit::emit_admin_audit*` from its
// create/update/delete/action submit handlers.
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

    // v0.12.2: Audit trail panel for this row. Best-effort — if the
    // audit table doesn't exist yet (project hasn't called
    // `audit::ensure_table` per tenant), the lookup returns Err and
    // we render an empty section instead of failing the whole page.
    let audit_entries_ctx: Vec<serde_json::Value> =
        match crate::audit::fetch_for_entity(&state.pool, model.table, &pk_raw).await {
            Ok(entries) => entries
                .into_iter()
                .map(|e| {
                    let (action_name, cleaned) = super::audit::split_action_marker(&e.changes);
                    serde_json::json!({
                        "id": e.id,
                        "operation": e.operation,
                        "action_name": action_name,
                        "source": e.source,
                        "occurred_at": e.occurred_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
                        "changes": serde_json::to_string_pretty(&cleaned)
                            .unwrap_or_default(),
                    })
                })
                .collect(),
            Err(_) => Vec::new(),
        };

    let mut ctx = serde_json::json!({
        "model": { "name": model.name, "table": model.table },
        "pk": pk_raw,
        "cells": cells_ctx,
        "read_only": state.is_read_only(model.table),
        "audit_entries": audit_entries_ctx,
    });
    let html = render_with_chrome(
        "detail.html",
        &mut ctx,
        chrome_context(&state, Some(model.table)),
    );
    Ok(Html(html))
}

// ============================================================== CREATE

pub(crate) async fn create_form(
    Path(table): Path<String>,
    State(state): State<AppState>,
) -> Result<Html<String>, AdminError> {
    let model = lookup_model(&state, &table).ok_or(AdminError::TableNotFound { table })?;
    if !state.can_add(model.table) {
        return Err(AdminError::ReadOnly {
            table: model.table.to_owned(),
        });
    }
    Ok(Html(render_form(
        &state, model, None, /* pk_locked */ false, None,
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
    if !state.can_add(model.table) {
        return Err(AdminError::ReadOnly {
            table: model.table.to_owned(),
        });
    }

    let pk_field = model.primary_key().ok_or_else(|| {
        AdminError::Internal(format!("model `{}` has no primary key", model.name))
    })?;
    // Auto-PK fields are server-assigned; readonly_fields are display-only
    // and must not be part of the INSERT. Build the combined skip list.
    let admin_cfg = model
        .admin
        .copied()
        .unwrap_or(crate::core::AdminConfig::DEFAULT);
    let mut skip: Vec<&str> = admin_cfg.readonly_fields.to_vec();
    if pk_field.auto {
        skip.push(pk_field.name);
    }
    let collected = match forms::collect_values(model, &form, &skip) {
        Ok(v) => v,
        Err(e) => {
            // Re-render the form with the error message instead of a 4xx.
            let html = render_form(&state, model, Some(&form), false, Some(&e.to_string()));
            return Ok(Html(html).into_response());
        }
    };
    let (columns, values): (Vec<&'static str>, Vec<SqlValue>) = collected.into_iter().unzip();

    let query = InsertQuery {
        model,
        columns,
        values,
        returning: vec![pk_field.column],
        on_conflict: None,
    };
    let row = match crate::sql::insert_returning(&state.pool, &query).await {
        Ok(row) => row,
        Err(e) => {
            let html = render_form(&state, model, Some(&form), false, Some(&e.to_string()));
            return Ok(Html(html).into_response());
        }
    };

    // Pull the (possibly-generated) PK back out of the RETURNING row so
    // the redirect lands on the right detail page even for Auto-PK models.
    let pk_value = render::read_value_as_string(&row, pk_field).unwrap_or_default();
    super::audit::emit_admin_audit(
        &state,
        model,
        &pk_value,
        crate::audit::AuditOp::Create,
        &form,
    )
    .await;
    Ok(Redirect::to(&format!("/__admin/{}/{}", model.table, pk_value)).into_response())
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
    Ok(Html(render_form(&state, model, Some(&prefill), true, None)))
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

    // Don't include PK in SET — keep identity stable. Same for any
    // user-marked `readonly_fields` (slice 10.5): the form rendered
    // them as `readonly` inputs, but a malicious POST could still
    // include them; skip server-side too.
    let admin_cfg = model
        .admin
        .copied()
        .unwrap_or(crate::core::AdminConfig::DEFAULT);
    let mut skip: Vec<&'static str> = vec![pk_field.name];
    skip.extend(admin_cfg.readonly_fields.iter().copied());
    let collected = match forms::collect_values(model, &form, &skip) {
        Ok(v) => v,
        Err(e) => {
            let html = render_form(&state, model, Some(&form), true, Some(&e.to_string()));
            return Ok(Html(html).into_response());
        }
    };
    let assignments: Vec<Assignment> = collected
        .into_iter()
        .map(|(column, value)| Assignment { column, value })
        .collect();

    // v0.12.3: SELECT the row's pre-update state so the audit emit
    // can produce a `{ "field": { "before": v, "after": v } }`
    // diff. Best-effort — if the SELECT fails (race, concurrent
    // delete), we fall back to the snapshot path so the data write
    // still emits something useful.
    let before_row = crate::sql::select_one_row(
        &state.pool,
        &SelectQuery {
            model,
            where_clause: WhereExpr::Predicate(Filter {
                column: pk_field.column,
                op: Op::Eq,
                value: pk_value.clone(),
            }),
            search: None,
            joins: vec![],
            order_by: vec![],
            limit: None,
            offset: None,
        },
    )
    .await
    .ok()
    .flatten();

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
        let html = render_form(&state, model, Some(&form), true, Some(&e.to_string()));
        return Ok(Html(html).into_response());
    }
    // Diff path: before from the SELECT, after from the form. Picks
    // up the per-request `with_source(User { id })` install from
    // `tenancy::admin`, so operators get a "who changed what" trail
    // automatically.
    super::audit::emit_admin_audit_diff(&state, model, &pk_raw, before_row.as_ref(), &form).await;
    Ok(Redirect::to(&format!("/__admin/{}/{}", model.table, pk_raw)).into_response())
}


// ============================================================== DELETE

pub(crate) async fn delete_submit(
    Path((table, pk_raw)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<Response, AdminError> {
    let model = lookup_model(&state, &table).ok_or(AdminError::TableNotFound {
        table: table.clone(),
    })?;
    if !state.can_delete(model.table) {
        return Err(AdminError::ReadOnly {
            table: model.table.to_owned(),
        });
    }
    let pk_field = model.primary_key().ok_or_else(|| {
        AdminError::Internal(format!("model `{}` has no primary key", model.name))
    })?;
    let pk_value = forms::parse_pk_string(pk_field, &pk_raw).map_err(AdminError::Form)?;

    // v0.12.3: SELECT the row before delete so the audit entry
    // captures what was actually removed (snapshot of pre-delete
    // state). Best-effort — missing row falls back to an empty
    // changes payload, which still records the operation + source.
    let before_row = crate::sql::select_one_row(
        &state.pool,
        &SelectQuery {
            model,
            where_clause: WhereExpr::Predicate(Filter {
                column: pk_field.column,
                op: Op::Eq,
                value: pk_value.clone(),
            }),
            search: None,
            joins: vec![],
            order_by: vec![],
            limit: None,
            offset: None,
        },
    )
    .await
    .ok()
    .flatten();

    let audit_op = if model.soft_delete_column.is_some() {
        crate::audit::AuditOp::SoftDelete
    } else {
        crate::audit::AuditOp::Delete
    };

    if let Some(col) = model.soft_delete_column {
        crate::sql::update(
            &state.pool,
            &UpdateQuery {
                model,
                set: vec![Assignment {
                    column: col,
                    value: SqlValue::from(chrono::Utc::now()),
                }],
                where_clause: WhereExpr::Predicate(Filter {
                    column: pk_field.column,
                    op: Op::Eq,
                    value: pk_value,
                }),
            },
        )
        .await?;
    } else {
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
    }

    let pairs: Vec<(&str, serde_json::Value)> = before_row
        .as_ref()
        .map(|row| {
            model
                .scalar_fields()
                .map(|f| (f.name, render::read_value_as_json(row, f)))
                .collect()
        })
        .unwrap_or_default();
    let entry = crate::audit::PendingEntry {
        entity_table: model.table,
        entity_pk: pk_raw.clone(),
        operation: audit_op,
        source: crate::audit::current_source(),
        changes: crate::audit::snapshot_changes(&pairs),
    };
    if let Err(e) = crate::audit::emit_one(&state.pool, &entry).await {
        tracing::warn!(
            target: "rustango::admin::audit",
            error = %e,
            entity_table = %model.table,
            entity_pk = %pk_raw,
            "admin audit emit failed for delete",
        );
    }
    Ok(Redirect::to(&format!("/__admin/{}", model.table)).into_response())
}

// ============================================================== ACTIONS (slice 10.6)

/// `POST /<table>/__action` — bulk action handler. Form payload:
///
/// ```text
/// action=<name>&_selected=<pk1>&_selected=<pk2>&...
/// ```
///
/// `<name>` must be in the model's `admin.actions` allowlist. The
/// built-in `delete_selected` runs `DELETE WHERE pk IN (...)` in a
/// single round-trip. Unknown action names → 400. No selected rows or
/// no action chosen → silent redirect back to the list.
pub(crate) async fn action_submit(
    Path(table): Path<String>,
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Response, AdminError> {
    let model = lookup_model(&state, &table).ok_or(AdminError::TableNotFound {
        table: table.clone(),
    })?;

    // Parse the form preserving repeats. axum's `Form<HashMap>` would
    // collapse duplicate `_selected` keys into one; we read the raw
    // body and use `serde_urlencoded` over a Vec<(String, String)>.
    let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(&body)
        .map_err(|e| AdminError::Internal(format!("parse action form: {e}")))?;

    let mut action_name: Option<String> = None;
    let mut selected_raw: Vec<String> = Vec::new();
    for (k, v) in pairs {
        if k == "action" {
            action_name = Some(v);
        } else if k == "_selected" {
            selected_raw.push(v);
        }
    }
    let Some(action) = action_name.filter(|s| !s.is_empty()) else {
        // No action picked — just bounce back to the list.
        return Ok(Redirect::to(&format!("/__admin/{}", model.table)).into_response());
    };
    if selected_raw.is_empty() {
        return Ok(Redirect::to(&format!("/__admin/{}", model.table)).into_response());
    }

    let admin_cfg = model
        .admin
        .copied()
        .unwrap_or(crate::core::AdminConfig::DEFAULT);
    if !admin_cfg.actions.iter().any(|a| *a == action) {
        return Err(AdminError::Internal(format!(
            "action `{action}` not registered for `{}`",
            model.name
        )));
    }

    let pk_field = model.primary_key().ok_or_else(|| {
        AdminError::Internal(format!("model `{}` has no primary key", model.name))
    })?;

    let pk_values: Vec<SqlValue> = selected_raw
        .iter()
        .filter_map(|raw| forms::parse_pk_string(pk_field, raw).ok())
        .collect();
    if pk_values.is_empty() {
        return Ok(Redirect::to(&format!("/__admin/{}", model.table)).into_response());
    }

    // v0.12.4: SELECT every selected row's pre-action state so the
    // audit emit can record what the action ran against. For
    // delete_selected this snapshots the gone rows; for user-defined
    // actions it records the row state at the time of action.
    let before_rows = crate::sql::select_rows(
        &state.pool,
        &SelectQuery {
            model,
            where_clause: WhereExpr::Predicate(Filter {
                column: pk_field.column,
                op: Op::In,
                value: SqlValue::List(pk_values.clone()),
            }),
            search: None,
            joins: vec![],
            order_by: vec![],
            limit: None,
            offset: None,
        },
    )
    .await
    .unwrap_or_default();

    let audit_op = if action == "delete_selected" {
        if model.soft_delete_column.is_some() {
            crate::audit::AuditOp::SoftDelete
        } else {
            crate::audit::AuditOp::Delete
        }
    } else if action == "restore_selected" {
        crate::audit::AuditOp::Update
    } else {
        crate::audit::AuditOp::Update
    };

    if action == "delete_selected" {
        if !state.can_delete(model.table) {
            return Err(AdminError::ReadOnly {
                table: model.table.to_owned(),
            });
        }
        if let Some(col) = model.soft_delete_column {
            // Soft model — stamp the deleted_at column instead of hard DELETE.
            crate::sql::update(
                &state.pool,
                &UpdateQuery {
                    model,
                    set: vec![Assignment {
                        column: col,
                        value: SqlValue::from(chrono::Utc::now()),
                    }],
                    where_clause: WhereExpr::Predicate(Filter {
                        column: pk_field.column,
                        op: Op::In,
                        value: SqlValue::List(pk_values),
                    }),
                },
            )
            .await?;
        } else {
            crate::sql::delete(
                &state.pool,
                &DeleteQuery {
                    model,
                    where_clause: WhereExpr::Predicate(Filter {
                        column: pk_field.column,
                        op: Op::In,
                        value: SqlValue::List(pk_values),
                    }),
                },
            )
            .await?;
        }
    } else if action == "restore_selected" {
        if state.is_read_only(model.table) {
            return Err(AdminError::ReadOnly {
                table: model.table.to_owned(),
            });
        }
        // Built-in restore — clears the soft-delete column (NULL = live).
        // Only meaningful for models with soft_delete_column; for others
        // the action is a no-op so users don't need to guard it.
        if let Some(col) = model.soft_delete_column {
            crate::sql::update(
                &state.pool,
                &UpdateQuery {
                    model,
                    set: vec![Assignment {
                        column: col,
                        value: SqlValue::Null,
                    }],
                    where_clause: WhereExpr::Predicate(Filter {
                        column: pk_field.column,
                        op: Op::In,
                        value: SqlValue::List(pk_values),
                    }),
                },
            )
            .await?;
        }
    } else if let Some(handler) = state.action_handler(model.table, &action) {
        if state.is_read_only(model.table) {
            return Err(AdminError::ReadOnly {
                table: model.table.to_owned(),
            });
        }
        handler(&state.pool, &pk_values).await?;
    } else {
        return Err(AdminError::Internal(format!(
            "action `{action}` is in `admin.actions` but no handler is registered \
             on the admin builder; register it via \
             `admin::Builder::register_action(\"{}\", \"{action}\", ...)` (built-ins: \
             delete_selected, restore_selected)",
            model.table
        )));
    }

    // Build one audit entry per row and emit them in a single
    // batched INSERT. For delete_selected the changes JSON is the
    // snapshot of what was deleted; for user-defined actions it
    // captures pre-action state with an `__action` marker so the
    // audit panel shows who ran what against which rows.
    let source = crate::audit::current_source();
    let entries: Vec<crate::audit::PendingEntry> = before_rows
        .iter()
        .map(|row| {
            let pk_str = render::read_value_as_string(row, pk_field).unwrap_or_default();
            let mut pairs: Vec<(&str, serde_json::Value)> = model
                .scalar_fields()
                .map(|f| (f.name, render::read_value_as_json(row, f)))
                .collect();
            if action != "delete_selected" {
                // Tag the action name into the changes payload so
                // the per-row audit row distinguishes "alice ran
                // publish_selected" from a plain edit.
                pairs.push(("__action", serde_json::Value::String(action.clone())));
            }
            crate::audit::PendingEntry {
                entity_table: model.table,
                entity_pk: pk_str,
                operation: audit_op,
                source: source.clone(),
                changes: crate::audit::snapshot_changes(&pairs),
            }
        })
        .collect();
    if !entries.is_empty() {
        if let Err(e) = crate::audit::emit_many(&state.pool, &entries).await {
            tracing::warn!(
                target: "rustango::admin::audit",
                error = %e,
                entity_table = %model.table,
                action = %action,
                count = entries.len(),
                "admin bulk-action audit emit failed",
            );
        }
    }

    Ok(Redirect::to(&format!("/__admin/{}", model.table)).into_response())
}
