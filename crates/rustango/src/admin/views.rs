//! Admin view handlers: the code behind every admin screen.
//!
//! One async fn per route, each returning either rendered HTML or a
//! redirect. Errors flow through [`AdminError`] which converts to a JSON
//! body with the right HTTP status. Backed by [`super::urls::AppState`].

use std::collections::HashMap;

use crate::core::{
    Assignment, CountQuery, DeleteQuery, FieldSchema, Filter, InsertQuery, ModelEntry, Op,
    SearchClause, SelectQuery, SqlValue, UpdateQuery, WhereExpr,
};
use axum::extract::{Form, Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};

use super::errors::AdminError;
use super::forms;
use super::helpers::{
    admin_config_or_default, build_fk_joins, chrome_context, fk_map_from_joined_rows_json,
    lookup_model, pager_suffix, primary_key_or_internal, render_cell_json, render_form,
    resolve_model, resolve_model_and_pk,
};
use super::render;
use super::templates::render_with_chrome;
use super::urls::AppState;

/// Render a `data.<key>` cell: read a JSON column at the given key
/// path and emit an HTML-escaped scalar.
///
/// Paths are dotted (`a.b.c`). A numeric segment indexes an array
/// (`items.0`). Returns `<em>NULL</em>` when the path does not resolve.
fn render_json_path_cell(
    row: &serde_json::Value,
    field: &'static crate::core::FieldSchema,
    key: &str,
) -> String {
    let mut node = match row.get(field.column).or_else(|| row.get(field.name)) {
        Some(v) => v,
        None => return "<em>NULL</em>".to_owned(),
    };
    for seg in key.split('.') {
        if seg.is_empty() {
            continue;
        }
        let next = if let Ok(idx) = seg.parse::<usize>() {
            node.as_array().and_then(|a| a.get(idx))
        } else {
            node.as_object().and_then(|o| o.get(seg))
        };
        node = match next {
            Some(n) => n,
            None => return "<em>NULL</em>".to_owned(),
        };
    }
    match node {
        serde_json::Value::Null => "<em>NULL</em>".to_owned(),
        serde_json::Value::String(s) => render::escape(s),
        serde_json::Value::Bool(true) => {
            r#"<span class="rcms-bool yes" aria-label="true">☑</span>"#.to_owned()
        }
        serde_json::Value::Bool(false) => {
            r#"<span class="rcms-bool no" aria-label="false">☐</span>"#.to_owned()
        }
        serde_json::Value::Number(n) => n.to_string(),
        // Nested objects and arrays print as compact JSON. The full
        // structure is visible on the detail page.
        other => render::escape(&other.to_string()),
    }
}

/// Render one generic-FK cell: read `(ct_column, pk_column)` off the
/// JSON row and emit a link to the target.
///
/// Output matches `contenttypes::render_generic_fk_link`, but this is
/// synchronous. The list view preloads every ContentType on the page
/// first, so there is no DB I/O per cell.
fn render_gfk_cell(
    row: &serde_json::Value,
    gr: &crate::core::GenericRelation,
    ct_map: &HashMap<i64, crate::contenttypes::ContentType>,
) -> String {
    let ct_id = row
        .get(gr.ct_column)
        .and_then(serde_json::Value::as_i64)
        .unwrap_or_default();
    let object_pk = row
        .get(gr.pk_column)
        .and_then(serde_json::Value::as_i64)
        .unwrap_or_default();
    if ct_id == 0 && object_pk == 0 {
        return "<em>NULL</em>".to_owned();
    }
    let Some(ct) = ct_map.get(&ct_id) else {
        // CT stale or not seeded: same fallback `render_generic_fk_link` uses.
        return format!("<em>(ct={ct_id}, pk={object_pk})</em>");
    };
    let label = format!("{}.{}", ct.app_label, ct.model_name);
    let table_esc = render::escape(&ct.table);
    let label_esc = render::escape(&label);
    format!(
        r#"<a href="/{table}/{pk}">{label} #{pk}</a>"#,
        table = table_esc,
        pk = object_pk,
        label = label_esc,
    )
}

// ============================================================== INDEX

pub(crate) async fn index(State(state): State<AppState>) -> Html<String> {
    // Group registered models by app label. `resolved_app_label()`
    // returns the `#[rustango(app = "...")]` override, or infers one
    // from the module path. Models with no app label go to "Project".
    let mut entries: Vec<&'static ModelEntry> = super::helpers::inventory_entries_dedup_by_table()
        .into_iter()
        // Registry-scoped models are hidden in tenant mode.
        .filter(|e| state.scope_visible(e.schema.scope))
        .filter(|e| state.is_visible(e.schema.table))
        .collect();
    entries.sort_by_key(|e| e.schema.name);

    let mut by_app: indexmap::IndexMap<String, Vec<&'static ModelEntry>> =
        indexmap::IndexMap::new();
    for e in entries {
        let label = e
            .resolved_app_label()
            .map_or_else(|| "Project".to_owned(), str::to_owned);
        by_app.entry(label).or_default().push(e);
    }
    // Alphabetical, with "Project" last so named apps come first in
    // the sidebar.
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

    // Flat `models` list kept for custom templates that still iterate
    // `models`. The bundled template renders from `groups`.
    let flat_models_ctx: Vec<serde_json::Value> = groups_ctx
        .iter()
        .flat_map(|g| {
            g.get("models")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
        })
        .collect();

    // Recent-actions feed: the newest 10 audit entries, which the
    // framework writes on every admin create, update and delete.
    // Best-effort: if the audit table does not exist yet, show an
    // empty list instead of failing the admin home.
    let recent_actions_ctx: Vec<serde_json::Value> =
        crate::audit::list(&state.pool, &crate::audit::AuditFilter::default(), 10, 0)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|entry| {
                let action_url = format!(
                    "{}/{}/{}",
                    state.config.admin_prefix, entry.entity_table, entry.entity_pk,
                );
                serde_json::json!({
                    "table": entry.entity_table,
                    "pk": entry.entity_pk,
                    "operation": entry.operation,
                    "source": entry.source,
                    "occurred_at": entry.occurred_at.to_rfc3339(),
                    "url": action_url,
                })
            })
            .collect();

    let mut ctx = serde_json::json!({
        "groups": groups_ctx,
        "models": flat_models_ctx,
        "recent_actions": recent_actions_ctx,
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
const RESERVED_PARAMS: &[&str] = &[
    "page",
    "q",
    "facet_show_all",
    "count",
    // Consumed by the date-hierarchy strip.
    "year",
    "month",
    "day",
];

/// Default cap on how many values one facet shows. Keeps the right
/// rail compact on columns with many distinct values. The rest
/// collapse into a "+N more" link (`?facet_show_all=<field>`).
const FACET_TRUNCATE: usize = 15;

#[allow(clippy::too_many_lines)] // mostly linear HTML emission; splitting hurts readability
pub(crate) async fn table_view(
    parts: axum::http::request::Parts,
    Path(table): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Result<Html<String>, AdminError> {
    let model = resolve_model(&state, &table)?;
    let pk_field = model.primary_key();
    let admin_cfg = admin_config_or_default(model);
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

    // Build per-field filters from extra query params. Unknown fields
    // and unparseable values are dropped: a bad URL should not 500.
    let mut filters: Vec<Filter> = Vec::new();
    let mut active_field_filters: Vec<(&'static str, String)> = Vec::new();
    // Names claimed by custom list filters, so `?status=draft` is not
    // also read as a field filter on a column of the same name.
    let custom_filter_names: Vec<&'static str> = crate::admin::list_filters::for_table(model.table)
        .map(|f| f.parameter_name)
        .collect();
    for (key, value) in &params {
        if RESERVED_PARAMS.contains(&key.as_str()) {
            continue;
        }
        if custom_filter_names.contains(&key.as_str()) {
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

    // Custom list filters: a named filter with its own choices. When
    // a filter's parameter is present in the URL, call its predicate
    // function and add the predicates it returns.
    let mut active_custom_filters: Vec<(&'static str, String)> = Vec::new();
    for cf in crate::admin::list_filters::for_table(model.table) {
        if let Some(value) = params.get(cf.parameter_name) {
            if value.is_empty() {
                continue;
            }
            filters.extend((cf.to_filters)(value));
            active_custom_filters.push((cf.parameter_name, value.clone()));
        }
    }

    // Queryset hooks. Each hook sees the request and returns extra
    // predicates for this model's list. They only add
    // WHERE conjuncts, so they compose with search, facets, the date
    // hierarchy and pagination.
    for h in crate::admin::queryset_hooks::for_table(model.table) {
        filters.extend((h.hook)(&parts));
    }

    // Date hierarchy. With `admin(date_hierarchy = "field")` set and
    // `?year[&month[&day]]` in the URL, add the half-open `[lo, hi)`
    // range predicates and keep the selection for the strip.
    let date_sel = crate::admin::date_hierarchy::DateSelection::parse(&params);
    if !admin_cfg.date_hierarchy.is_empty() {
        filters.extend(crate::admin::date_hierarchy::predicates(
            model,
            admin_cfg.date_hierarchy,
            date_sel,
        ));
    }

    // Build the search clause. `admin.search_fields` wins when set.
    // Otherwise use every field whose `searchable` flag is true.
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

    // Skip `SELECT COUNT(*)` on big tables. Turned on per table by
    // `Builder::skip_count_for(...)`, or per request by `?count=skip`
    // or `?count=0`. Without a total the pager shows "Page N" instead
    // of "Page N of M", and prev/next come from fetching one extra
    // row and trimming it.
    let count_skipped = state.count_skipped_for_table(model.table)
        || matches!(
            params.get("count").map(String::as_str),
            Some("skip" | "0" | "false" | "no")
        );
    let total: i64 = if count_skipped {
        0
    } else {
        crate::sql::count_rows_pool(
            &state.pool,
            &CountQuery {
                model,
                where_clause: where_clause.clone(),
                // Same search the SELECT uses, so the pager total
                // matches the visible rows.
                search: search.clone(),
            },
        )
        .await?
    };
    let joins = build_fk_joins(&state, model);
    // Default ordering: PK ASC unless `admin.ordering` overrides.
    let order_by: Vec<crate::core::OrderItem> = if admin_cfg.ordering.is_empty() {
        Vec::new()
    } else {
        admin_cfg
            .ordering
            .iter()
            .filter_map(|(name, desc)| {
                model
                    .field(name)
                    .map(|f| crate::core::OrderItem::column(f.column, *desc))
            })
            .collect()
    };
    // With the count skipped, fetch one extra row to detect "has
    // more" without counting the table. The extra row is trimmed
    // before rendering.
    let fetch_limit = if count_skipped {
        page_size + 1
    } else {
        page_size
    };
    let scalar_fields: Vec<&'static FieldSchema> = model.scalar_fields().collect();
    let mut rows = crate::sql::select_rows_as_json(
        &state.pool,
        &SelectQuery {
            where_clause,
            search: search.clone(),
            joins,
            order_by,
            limit: Some(fetch_limit),
            offset: Some(offset),
            ..SelectQuery::new(model)
        },
        &scalar_fields,
    )
    .await?;
    let has_next_skipped = if count_skipped && rows.len() as i64 > page_size {
        rows.truncate(page_size as usize);
        true
    } else {
        false
    };

    let fk_map = fk_map_from_joined_rows_json(&state, model, &rows);

    let last_page = if count_skipped {
        // No total means no last page. The pager renders "Page N"
        // and uses `has_next_skipped` for prev/next.
        page
    } else if total == 0 {
        1
    } else {
        ((total - 1) / page_size) + 1
    };
    let read_only = state.is_read_only(model.table);

    // Resolve the columns shown on the list. Each `admin.list_display`
    // entry must name one of: a scalar field, a computed field
    // registered with `register_admin_computed!`, a
    // `#[rustango(generic_fk(name = "…"))]` relation, or a dotted path
    // into a JSON column. A name that matches none of them is dropped
    // and logged: without the warning the only symptom is a missing
    // column, which reads as a framework limit rather than a typo.
    // Empty `list_display` falls back to every scalar field.
    enum DisplayItem {
        Field(&'static FieldSchema),
        Computed(&'static crate::admin::computed_fields::ComputedField),
        GenericFk(&'static crate::core::GenericRelation),
        /// `list_display = "data.title"`: the head segment names a
        /// `FieldType::Json` column, the rest is the path inside it.
        JsonPath(&'static FieldSchema, &'static str),
    }
    let display_items: Vec<DisplayItem> = if admin_cfg.list_display.is_empty() {
        model.scalar_fields().map(DisplayItem::Field).collect()
    } else {
        admin_cfg
            .list_display
            .iter()
            .filter_map(|name| {
                model
                    .field(name)
                    .map(DisplayItem::Field)
                    .or_else(|| {
                        crate::admin::computed_fields::find(model.table, name)
                            .map(DisplayItem::Computed)
                    })
                    .or_else(|| {
                        model
                            .generic_relations
                            .iter()
                            .find(|gr| gr.name == *name)
                            .map(DisplayItem::GenericFk)
                    })
                    .or_else(|| {
                        // `data.title`: the head segment must be a Json
                        // column on the model.
                        let (head, tail) = name.split_once('.')?;
                        let field = model.field(head)?;
                        if field.ty == crate::core::FieldType::Json {
                            Some(DisplayItem::JsonPath(field, tail))
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        tracing::warn!(
                            table = model.table,
                            name = %name,
                            "list_display names `{name}`, which is not a field, a \
                             registered computed field, a generic_fk, or a dotted \
                             path into a JSON column on `{}` — the column is omitted. \
                             Check for a typo, a renamed field, or a \
                             register_admin_computed! that did not run.",
                            model.table,
                        );
                        None
                    })
            })
            .collect()
    };

    // Preload every ContentType used by a `DisplayItem::GenericFk`
    // cell, so the row loop reads a map instead of doing one async
    // lookup per cell. The CT registry is process-cached, so this is
    // at most one round-trip per distinct ct_id.
    let gfk_ct_map: std::collections::HashMap<i64, crate::contenttypes::ContentType> = {
        use std::collections::HashSet;
        let mut needed: HashSet<i64> = HashSet::new();
        for gr in model.generic_relations {
            if display_items
                .iter()
                .any(|i| matches!(i, DisplayItem::GenericFk(g) if g.name == gr.name))
            {
                for row in &rows {
                    if let Some(id) = row.get(gr.ct_column).and_then(serde_json::Value::as_i64) {
                        needed.insert(id);
                    }
                }
            }
        }
        let mut map = std::collections::HashMap::with_capacity(needed.len());
        for id in needed {
            if let Ok(Some(ct)) = crate::contenttypes::ContentType::by_id(&state.pool, id).await {
                map.insert(id, ct);
            }
        }
        map
    };

    // Per-column header label. The PK gets a `<small>(pk)</small>`
    // suffix. Computed fields show their declared label, or the bare
    // identifier when they have none.
    let columns_ctx: Vec<serde_json::Value> = display_items
        .iter()
        .map(|item| {
            let label = match item {
                DisplayItem::Field(f) => {
                    // `#[rustango(verbose_name = "...")]` wins over the
                    // Rust identifier here.
                    let caption = f.display_label();
                    if f.primary_key {
                        format!("{} <small>(pk)</small>", render::escape(caption))
                    } else {
                        render::escape(caption)
                    }
                }
                DisplayItem::Computed(m) => {
                    render::escape(if m.label.is_empty() { m.name } else { m.label })
                }
                DisplayItem::GenericFk(gr) => render::escape(gr.name),
                DisplayItem::JsonPath(f, key) => {
                    render::escape(&format!("{}.{}", f.display_label(), key))
                }
            };
            serde_json::json!({ "label": label })
        })
        .collect();

    // `list_display_links`: every named column has its cell wrapped
    // in an `<a>` to the detail view. When the list is empty, no cell
    // is wrapped and the template's trailing "View" column is the
    // only link.
    let link_columns: std::collections::HashSet<&str> =
        admin_cfg.list_display_links.iter().copied().collect();
    // Resolve "wrap this cell?" once per column, in `display_items`
    // order, so the row loop does not repeat the lookup.
    let cell_is_link: Vec<bool> = display_items
        .iter()
        .map(|item| {
            let name = match item {
                DisplayItem::Field(f) => f.name,
                DisplayItem::Computed(m) => m.name,
                DisplayItem::GenericFk(gr) => gr.name,
                // JsonPath columns are never links: the value is a
                // nested datum, not the row's identity.
                DisplayItem::JsonPath(_, _) => return false,
            };
            link_columns.contains(name)
        })
        .collect();

    // Per-row payload. A computed-field cell is HTML the user's
    // closure already escaped. Scalar cells go through
    // `render_cell_json`, which emits an FK link or an escaped scalar.
    let rows_ctx: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let pk_raw = pk_field
                .map(|pk| render::render_value_for_input_json(row, pk))
                .unwrap_or_default();
            let pk = if pk_raw.is_empty() {
                None
            } else {
                Some(render::escape(&pk_raw))
            };
            // A detail URL needs a pk. Rows without one keep plain
            // cell content.
            let detail_href = pk.as_deref().map(|pk_str| {
                format!(
                    "{prefix}/{table}/{pk_str}",
                    prefix = state.config.admin_prefix,
                    table = model.table,
                )
            });
            let cells: Vec<String> = display_items
                .iter()
                .enumerate()
                .map(|(idx, item)| {
                    let inner = match item {
                        DisplayItem::Field(f) => render_cell_json(row, f, &fk_map),
                        DisplayItem::Computed(m) => (m.render)(row),
                        DisplayItem::GenericFk(gr) => render_gfk_cell(row, gr, &gfk_ct_map),
                        DisplayItem::JsonPath(f, key) => render_json_path_cell(row, f, key),
                    };
                    // A `link =` callable on a computed field wins
                    // over `list_display_links`: it knows where this
                    // one cell should jump, such as an FK target.
                    if let DisplayItem::Computed(cf) = item {
                        if let Some(link_fn) = cf.link {
                            if let Some(url) = link_fn(row) {
                                return format!(
                                    "<a href=\"{href}\">{inner}</a>",
                                    href = render::escape(&url),
                                );
                            }
                        }
                    }
                    match (
                        cell_is_link.get(idx).copied().unwrap_or(false),
                        &detail_href,
                    ) {
                        (true, Some(href)) => format!(
                            "<a href=\"{href}\">{inner}</a>",
                            href = render::escape(href),
                        ),
                        _ => inner,
                    }
                })
                .collect();
            serde_json::json!({ "cells": cells, "pk": pk })
        })
        .collect();

    let active_filters_ctx: Vec<serde_json::Value> = active_field_filters
        .iter()
        .map(|(k, v)| serde_json::json!({ "key": k, "value": v }))
        .collect();
    let pager_suffix_str = pager_suffix(q.as_deref(), &active_field_filters);

    // Facet filters. For each `admin.list_filter` field, query its
    // distinct values and counts for a right-rail card. Each link
    // toggles `?<col>=<value>`: clicking the active value clears the
    // filter, clicking another value swaps to it.
    let show_all_facet = params.get("facet_show_all").map(String::as_str);
    let facets_ctx: Vec<serde_json::Value> = compute_facets(
        &state,
        model,
        &admin_cfg,
        &active_field_filters,
        q.as_deref(),
        show_all_facet,
    )
    .await?;

    // Date-hierarchy strip: the breadcrumb plus the child buckets at
    // the current drill level (year, month or day). One GROUP BY
    // query, narrowed by the parent level's WHERE.
    let date_hierarchy_ctx: Option<serde_json::Value> = if admin_cfg.date_hierarchy.is_empty() {
        None
    } else {
        compute_date_hierarchy(
            &state,
            model,
            &admin_cfg,
            date_sel,
            q.as_deref(),
            &active_field_filters,
        )
        .await?
    };

    // Right-rail card per custom list filter: the filter title and
    // its clickable options, with the active value marked.
    let admin_prefix = state.config.admin_prefix.as_str();
    let preserved_params_for_custom: Vec<(String, String)> = {
        let mut out = Vec::new();
        if let Some(qv) = q.as_deref() {
            out.push(("q".into(), qv.into()));
        }
        for (k, v) in &active_field_filters {
            out.push(((*k).into(), v.clone()));
        }
        out
    };
    let custom_filters_ctx: Vec<serde_json::Value> =
        crate::admin::list_filters::for_table(model.table)
            .map(|cf| {
                let active_value: Option<&str> = active_custom_filters
                    .iter()
                    .find(|(k, _)| *k == cf.parameter_name)
                    .map(|(_, v)| v.as_str());
                let mut params_clear = preserved_params_for_custom.clone();
                // Keep the other custom filters' selections.
                for (k, v) in &active_custom_filters {
                    if *k != cf.parameter_name {
                        params_clear.push(((*k).into(), v.clone()));
                    }
                }
                let clear_url = build_query_url(admin_prefix, model.table, &params_clear);
                let values: Vec<serde_json::Value> = cf
                    .lookups
                    .iter()
                    .map(|(value, label)| {
                        let mut p = params_clear.clone();
                        p.push((cf.parameter_name.into(), (*value).into()));
                        serde_json::json!({
                            "value": value,
                            "label": label,
                            "active": active_value == Some(*value),
                            "url": build_query_url(admin_prefix, model.table, &p),
                        })
                    })
                    .collect();
                serde_json::json!({
                    "parameter_name": cf.parameter_name,
                    "title": cf.title,
                    "values": values,
                    "clear_url": clear_url,
                })
            })
            .collect();

    // Action menu items. Empty when the model declares no
    // `admin.actions`, which hides the picker.
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
        "model": {
            "name": model.name,
            "table": model.table,
            // Captions from `#[rustango(verbose_name = "…",
            // verbose_name_plural = "…")]`. Templates use these for
            // headings and breadcrumbs, and `name` for routing.
            "label": model.display_label(),
            "label_plural": model.display_label_plural(),
        },
        "total": total,
        "plural": if total == 1 { "" } else { "s" },
        "read_only": read_only,
        "has_searchable": !search_columns.is_empty(),
        // An empty `search_help_text` hides the caption.
        "search_help_text": admin_cfg.search_help_text,
        // Action-bar position flags. Default: top on, bottom off.
        "actions_on_top": admin_cfg.actions_on_top,
        "actions_on_bottom": admin_cfg.actions_on_bottom,
        "q": q.unwrap_or_default(),
        "active_filters": active_filters_ctx,
        "facets": facets_ctx,
        "custom_filters": custom_filters_ctx,
        "date_hierarchy": date_hierarchy_ctx,
        "actions": actions_ctx,
        "columns": columns_ctx,
        "rows": rows_ctx,
        "page": page,
        "last_page": last_page,
        "pager_suffix": pager_suffix_str,
        // Count-skip pager fields. Templates branch on
        // `count_skipped` to render "Page N" with `has_next` driving
        // prev/next. A custom template that ignores them sees
        // total=0 and last_page=page, so its `if last_page > 1`
        // guard simply renders no pager.
        "count_skipped": count_skipped,
        "has_next": has_next_skipped,
    });
    Ok(Html(render_with_chrome(
        "list.html",
        &mut ctx,
        chrome_context(&state, Some(model.table)),
    )))
}

/// For each `admin.list_filter` field, compute the distinct values,
/// their row counts, and the URL each value toggles to.
///
/// One `GROUP BY` round-trip per facet field. That is fine for a
/// handful of facets with modest cardinality. Drop the field from
/// `list_filter` if a column has tens of thousands of distinct
/// values. FK columns also show the joined display value.
///
/// Clicking the active value clears that filter. Clicking another
/// value sets it.
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

        // For FK fields, join the target table on its display column
        // so the card shows "Dr. Maeve O'Hara (3)", not "1 (3)".
        // Falls back to the raw value for non-FK fields, targets
        // hidden from the admin, and targets with no `display = "…"`.
        let fk_join: Option<(&'static str, &'static str, &'static str)> =
            field.relation.and_then(|rel| match rel {
                crate::core::Relation::Fk { to, on } | crate::core::Relation::O2O { to, on } => {
                    let target = lookup_model(state, to)?;
                    let display_field = target.display_field()?;
                    Some((target.table, on, display_field.column))
                }
            });

        // Order by count descending so the most used value is first,
        // then by the displayed value so output is stable across
        // requests. Identifiers go through the dialect's
        // `quote_ident`, so the same SQL works on all backends.
        let dialect = state.pool.dialect();
        let sql = if let Some((target_table, target_pk, display_col)) = fk_join {
            let src_t = dialect.quote_ident(model.table);
            let src_c = dialect.quote_ident(field.column);
            let tgt_t = dialect.quote_ident(target_table);
            let tgt_pk = dialect.quote_ident(target_pk);
            let tgt_disp = dialect.quote_ident(display_col);
            format!(
                "SELECT {src_t}.{src_c} AS facet_value, \
                        {tgt_t}.{tgt_disp} AS facet_display, \
                        COUNT(*) AS facet_count \
                 FROM {src_t} \
                 LEFT JOIN {tgt_t} ON {tgt_t}.{tgt_pk} = {src_t}.{src_c} \
                 GROUP BY {src_t}.{src_c}, {tgt_t}.{tgt_disp} \
                 ORDER BY facet_count DESC, {tgt_t}.{tgt_disp}"
            )
        } else {
            let t = dialect.quote_ident(model.table);
            let c = dialect.quote_ident(field.column);
            format!(
                "SELECT {c} AS facet_value, COUNT(*) AS facet_count \
                 FROM {t} \
                 GROUP BY {c} \
                 ORDER BY facet_count DESC, {c}"
            )
        };
        let facet_rows = fetch_facet_rows(&state.pool, &sql, fk_join.is_some())
            .await
            .map_err(|e| AdminError::Internal(e.to_string()))?;
        let mut values = Vec::with_capacity(facet_rows.len());
        for (raw_value, display_text, count) in &facet_rows {
            // `raw_value` is already a string: the per-backend fetch
            // stringifies it. This is the shape `parse_form_value`
            // accepts back when the URL round-trips.
            let raw = raw_value.clone();
            // Joined FK facets show the target's display value, other
            // facets show the raw key.
            let display = if raw.is_empty() {
                "—".to_owned()
            } else if let Some(d) = display_text.as_deref().filter(|s| !s.is_empty()) {
                render::escape(d)
            } else {
                render::escape(&raw)
            };
            let count: i64 = *count;
            let is_active = active_value.map(|v| v == raw).unwrap_or(false);
            // Toggle URL: drop this filter when it is active, else
            // set it. Other active filters and `?q=` are kept.
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
            let toggle_url =
                build_query_url(state.config.admin_prefix.as_str(), model.table, &params);

            values.push(serde_json::json!({
                "raw": raw,
                "display": display,
                "count": count,
                "active": is_active,
                "toggle_url": toggle_url,
            }));
        }
        // Truncate to FACET_TRUNCATE values unless "show all" is on
        // for this column. Active values always render, so one never
        // hides behind the cutoff. They count toward the budget.
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
            // Keep the current filters and add
            // `facet_show_all=<field>`, which swaps the truncated
            // list for the full one.
            let mut params: Vec<(String, String)> = Vec::new();
            if let Some(qv) = q {
                params.push(("q".into(), qv.into()));
            }
            for (k, v) in active_field_filters {
                params.push(((*k).into(), v.clone()));
            }
            params.push(("facet_show_all".into(), field.name.into()));
            Some(build_query_url(
                state.config.admin_prefix.as_str(),
                model.table,
                &params,
            ))
        } else {
            None
        };
        // FK facets render as a `<select>`; this is the "All" option,
        // which removes the filter.
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
            Some(build_query_url(
                state.config.admin_prefix.as_str(),
                model.table,
                &params,
            ))
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

/// Run a facet `GROUP BY` on the current backend.
///
/// Returns `(raw_value, display_text, count)` triples. The column
/// value is stringified here so the caller stays backend-agnostic.
/// Pass `expect_display = true` for the FK-joined facet, which also
/// reads the joined display column.
async fn fetch_facet_rows(
    pool: &crate::sql::Pool,
    sql: &str,
    expect_display: bool,
) -> Result<Vec<(String, Option<String>, i64)>, sqlx::Error> {
    // The fetch stays per-arm because sqlx's `Executor` is bound to
    // a concrete `Database`. Only the row decode is generic, so it
    // lives in `decode_facet_row`.
    match pool {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => {
            let rows = sqlx::query(sql).fetch_all(pg).await?;
            Ok(rows
                .iter()
                .map(|r| decode_facet_row(r, expect_display))
                .collect())
        }
        #[cfg(feature = "mysql")]
        crate::sql::Pool::Mysql(my) => {
            let rows = sqlx::query(sql).fetch_all(my).await?;
            Ok(rows
                .iter()
                .map(|r| decode_facet_row(r, expect_display))
                .collect())
        }
        #[cfg(feature = "sqlite")]
        crate::sql::Pool::Sqlite(sq) => {
            let rows = sqlx::query(sql).fetch_all(sq).await?;
            Ok(rows
                .iter()
                .map(|r| decode_facet_row(r, expect_display))
                .collect())
        }
    }
}

/// Per-row decoder for [`fetch_facet_rows`], generic over the row
/// type so every backend shares one loop. The bounds are those of
/// [`stringify_facet_value`] plus the display and count decodes.
fn decode_facet_row<'r, R>(row: &'r R, expect_display: bool) -> (String, Option<String>, i64)
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    Option<String>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Option<i64>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Option<i32>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Option<bool>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    let raw = stringify_facet_value(row);
    let display = if expect_display {
        row.try_get::<Option<String>, _>("facet_display")
            .ok()
            .flatten()
    } else {
        None
    };
    let count: i64 = row.try_get("facet_count").unwrap_or(0);
    (raw, display, count)
}

/// Decode the `facet_value` column: text first, then the numeric and
/// boolean scalars facets commonly hit. Returns an empty string when
/// no shape matches.
fn stringify_facet_value<'r, R>(row: &'r R) -> String
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    Option<String>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Option<i64>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Option<i32>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Option<bool>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    if let Ok(Some(s)) = row.try_get::<Option<String>, _>("facet_value") {
        return s;
    }
    if let Ok(Some(n)) = row.try_get::<Option<i64>, _>("facet_value") {
        return n.to_string();
    }
    if let Ok(Some(n)) = row.try_get::<Option<i32>, _>("facet_value") {
        return n.to_string();
    }
    if let Ok(Some(b)) = row.try_get::<Option<bool>, _>("facet_value") {
        return b.to_string();
    }
    String::new()
}

// ============================================================== DATE HIERARCHY
//
// Breadcrumb and drill children for the list view's clickable
// year/month/day strip. One GROUP BY against the model's table,
// narrowed by the parent level's WHERE so a deeper level only sees
// its own slice. The bucket expression per dialect:
//
// - PG / MySQL: `EXTRACT({YEAR|MONTH|DAY} FROM <col>)`
// - SQLite:     `CAST(strftime('%Y'|'%m'|'%d', <col>) AS INTEGER)`
async fn compute_date_hierarchy(
    state: &AppState,
    model: &'static crate::core::ModelSchema,
    admin_cfg: &crate::core::AdminConfig,
    sel: crate::admin::date_hierarchy::DateSelection,
    q: Option<&str>,
    active_field_filters: &[(&'static str, String)],
) -> Result<Option<serde_json::Value>, AdminError> {
    use crate::admin::date_hierarchy::{range, DrillLevel};

    let field_name = admin_cfg.date_hierarchy;
    let Some(field) = model.field(field_name) else {
        return Ok(None);
    };
    if !matches!(
        field.ty,
        crate::core::FieldType::Date | crate::core::FieldType::DateTime
    ) {
        return Ok(None);
    }

    // ---------- Breadcrumb -------------------------------------------------
    let admin_prefix = state.config.admin_prefix.as_str();
    let preserved: Vec<(String, String)> = {
        let mut out = Vec::new();
        if let Some(qv) = q {
            out.push(("q".into(), qv.into()));
        }
        for (k, v) in active_field_filters {
            out.push(((*k).into(), v.clone()));
        }
        out
    };

    let url_for = |year: Option<i32>, month: Option<u32>, day: Option<u32>| -> String {
        let mut params = preserved.clone();
        if let Some(y) = year {
            params.push(("year".into(), y.to_string()));
            if let Some(m) = month {
                params.push(("month".into(), m.to_string()));
                if let Some(d) = day {
                    params.push(("day".into(), d.to_string()));
                }
            }
        }
        build_query_url(admin_prefix, model.table, &params)
    };

    const MONTH_NAMES: &[&str] = &[
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];

    let mut crumbs: Vec<serde_json::Value> = vec![serde_json::json!({
        "label": "All",
        "url": url_for(None, None, None),
        "active": sel.year.is_none(),
    })];
    if let Some(y) = sel.year {
        crumbs.push(serde_json::json!({
            "label": y.to_string(),
            "url": url_for(Some(y), None, None),
            "active": sel.month.is_none(),
        }));
    }
    if let (Some(y), Some(m)) = (sel.year, sel.month) {
        crumbs.push(serde_json::json!({
            "label": MONTH_NAMES.get((m as usize).saturating_sub(1)).copied().unwrap_or(""),
            "url": url_for(Some(y), Some(m), None),
            "active": sel.day.is_none(),
        }));
    }
    if let (Some(y), Some(m), Some(d)) = (sel.year, sel.month, sel.day) {
        crumbs.push(serde_json::json!({
            "label": d.to_string(),
            "url": url_for(Some(y), Some(m), Some(d)),
            "active": true,
        }));
    }

    // ---------- Children ---------------------------------------------------
    let Some(level) = DrillLevel::for_selection(sel) else {
        return Ok(Some(serde_json::json!({
            "field": field_name,
            "crumbs": crumbs,
            "buckets": Vec::<serde_json::Value>::new(),
        })));
    };

    let dialect = state.pool.dialect();
    let table_q = dialect.quote_ident(model.table);
    let col_q = format!("{}.{}", table_q, dialect.quote_ident(field.column));
    let bucket_expr = level.bucket_expr(dialect, &col_q);
    let where_sql = if range(sel).is_some() {
        // Bind through placeholders so sqlx handles quoting and
        // escaping instead of string formatting.
        let p1 = dialect.placeholder(1);
        let p2 = dialect.placeholder(2);
        format!("WHERE {col_q} >= {p1} AND {col_q} < {p2}")
    } else {
        String::new()
    };
    let order_dir = match level {
        DrillLevel::Year => "DESC", // newest year first
        _ => "ASC",
    };
    let sql = format!(
        "SELECT {bucket_expr} AS bucket, COUNT(*) AS bucket_count \
         FROM {table_q} {where_sql} \
         GROUP BY bucket \
         ORDER BY bucket {order_dir}"
    );

    let buckets_raw =
        fetch_date_hierarchy_buckets(&state.pool, &sql, range(sel), &field.ty).await?;

    let buckets_ctx: Vec<serde_json::Value> = buckets_raw
        .into_iter()
        .filter_map(|(bucket, count)| {
            let label;
            let next_year;
            let next_month;
            let next_day;
            match level {
                DrillLevel::Year => {
                    label = bucket.to_string();
                    next_year = Some(bucket);
                    next_month = None;
                    next_day = None;
                }
                DrillLevel::Month => {
                    if !(1..=12).contains(&bucket) {
                        return None;
                    }
                    label = MONTH_NAMES
                        .get((bucket as usize).saturating_sub(1))
                        .copied()
                        .unwrap_or("")
                        .to_owned();
                    next_year = sel.year;
                    next_month = Some(bucket as u32);
                    next_day = None;
                }
                DrillLevel::Day => {
                    if !(1..=31).contains(&bucket) {
                        return None;
                    }
                    label = bucket.to_string();
                    next_year = sel.year;
                    next_month = sel.month;
                    next_day = Some(bucket as u32);
                }
            }
            Some(serde_json::json!({
                "label": label,
                "value": bucket,
                "count": count,
                "url": url_for(next_year, next_month, next_day),
            }))
        })
        .collect();

    Ok(Some(serde_json::json!({
        "field": field_name,
        "crumbs": crumbs,
        "buckets": buckets_ctx,
    })))
}

/// Fetch the date-hierarchy buckets. The parent level's `[lo, hi)`
/// range is bound as real parameters, never formatted into the SQL.
async fn fetch_date_hierarchy_buckets(
    pool: &crate::sql::Pool,
    sql: &str,
    range: Option<(chrono::NaiveDate, chrono::NaiveDate)>,
    field_ty: &crate::core::FieldType,
) -> Result<Vec<(i32, i64)>, AdminError> {
    use chrono::{TimeZone, Utc};
    // The bind is the same on every backend; only the `bucket`
    // decode differs (PG returns NUMERIC, the others INT), so each
    // arm calls its own `decode_bucket_*_row`.
    let lo_hi = range.map(|(lo, hi)| match field_ty {
        crate::core::FieldType::DateTime => {
            let lo_dt = Utc.from_utc_datetime(&lo.and_hms_opt(0, 0, 0).unwrap());
            let hi_dt = Utc.from_utc_datetime(&hi.and_hms_opt(0, 0, 0).unwrap());
            (BucketBind::DateTime(lo_dt, hi_dt), true)
        }
        crate::core::FieldType::Date => (BucketBind::Date(lo, hi), true),
        _ => (BucketBind::None, false),
    });
    match pool {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => {
            let mut q = sqlx::query(sql);
            if let Some((bind, _)) = &lo_hi {
                q = bind.apply_pg(q);
            }
            let rows = q
                .fetch_all(pg)
                .await
                .map_err(|e| AdminError::Internal(e.to_string()))?;
            Ok(rows.iter().map(decode_bucket_pg_row).collect())
        }
        #[cfg(feature = "mysql")]
        crate::sql::Pool::Mysql(my) => {
            let mut q = sqlx::query(sql);
            if let Some((bind, _)) = &lo_hi {
                q = bind.apply_my(q);
            }
            let rows = q
                .fetch_all(my)
                .await
                .map_err(|e| AdminError::Internal(e.to_string()))?;
            Ok(rows.iter().map(decode_bucket_my_row).collect())
        }
        #[cfg(feature = "sqlite")]
        crate::sql::Pool::Sqlite(sq) => {
            let mut q = sqlx::query(sql);
            if let Some((bind, _)) = &lo_hi {
                q = bind.apply_sq(q);
            }
            let rows = q
                .fetch_all(sq)
                .await
                .map_err(|e| AdminError::Internal(e.to_string()))?;
            Ok(rows.iter().map(decode_bucket_sq_row).collect())
        }
    }
}

/// Range bind for the date-hierarchy SELECT. The lo/hi pair binds as
/// a [`chrono::NaiveDate`] for a Date column, or a
/// [`chrono::DateTime<Utc>`] for a DateTime column.
#[derive(Debug, Clone, Copy)]
enum BucketBind {
    None,
    Date(chrono::NaiveDate, chrono::NaiveDate),
    DateTime(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>),
}

impl BucketBind {
    #[cfg(feature = "postgres")]
    fn apply_pg<'a>(
        &self,
        q: sqlx::query::Query<'a, sqlx::Postgres, sqlx::postgres::PgArguments>,
    ) -> sqlx::query::Query<'a, sqlx::Postgres, sqlx::postgres::PgArguments> {
        match self {
            BucketBind::None => q,
            BucketBind::Date(lo, hi) => q.bind(*lo).bind(*hi),
            BucketBind::DateTime(lo, hi) => q.bind(*lo).bind(*hi),
        }
    }
    #[cfg(feature = "mysql")]
    fn apply_my<'a>(
        &self,
        q: sqlx::query::Query<'a, sqlx::MySql, sqlx::mysql::MySqlArguments>,
    ) -> sqlx::query::Query<'a, sqlx::MySql, sqlx::mysql::MySqlArguments> {
        match self {
            BucketBind::None => q,
            BucketBind::Date(lo, hi) => q.bind(*lo).bind(*hi),
            BucketBind::DateTime(lo, hi) => q.bind(*lo).bind(*hi),
        }
    }
    #[cfg(feature = "sqlite")]
    fn apply_sq<'a>(
        &self,
        q: sqlx::query::Query<'a, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'a>>,
    ) -> sqlx::query::Query<'a, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'a>> {
        match self {
            BucketBind::None => q,
            // A `Date` has no time part, so it binds as itself.
            BucketBind::Date(lo, hi) => q.bind(*lo).bind(*hi),
            // A `DateTime` must use the canonical encoder. The bounds
            // are compared against a TEXT column, and every other
            // writer emits the fixed-width shape. sqlx's own encoding
            // is variable-width, so the filter would miss rows whose
            // fractional seconds differ in length.
            BucketBind::DateTime(lo, hi) => q
                .bind(crate::sql::encode_datetime(*lo))
                .bind(crate::sql::encode_datetime(*hi)),
        }
    }
}

#[cfg(feature = "postgres")]
fn decode_bucket_pg_row(row: &sqlx::postgres::PgRow) -> (i32, i64) {
    use sqlx::Row as _;
    // PG's EXTRACT returns NUMERIC. Decoding as f64 and casting
    // avoids pulling in a Decimal feature.
    let bucket: f64 = row.try_get("bucket").unwrap_or(0.0);
    let count: i64 = row.try_get("bucket_count").unwrap_or(0);
    (bucket as i32, count)
}

#[cfg(feature = "mysql")]
fn decode_bucket_my_row(row: &sqlx::mysql::MySqlRow) -> (i32, i64) {
    use sqlx::Row as _;
    // MySQL EXTRACT returns INT; try i64 first then i32.
    let bucket: i64 = row
        .try_get::<i64, _>("bucket")
        .or_else(|_| row.try_get::<i32, _>("bucket").map(i64::from))
        .unwrap_or(0);
    let count: i64 = row.try_get("bucket_count").unwrap_or(0);
    (bucket as i32, count)
}

#[cfg(feature = "sqlite")]
fn decode_bucket_sq_row(row: &sqlx::sqlite::SqliteRow) -> (i32, i64) {
    use sqlx::Row as _;
    let bucket: i64 = row.try_get("bucket").unwrap_or(0);
    let count: i64 = row.try_get("bucket_count").unwrap_or(0);
    (bucket as i32, count)
}

// Where to send the user after a successful save. The add and change
// forms ship three submit buttons, and the one that was pressed
// arrives as a form key:
//
//   _save        → the list view
//   _continue    → back to the detail page
//   _addanother  → an empty add form
pub(crate) fn post_save_redirect(
    admin_prefix: &str,
    table: &str,
    pk_value: &str,
    form: &HashMap<String, String>,
) -> String {
    if form.contains_key("_continue") {
        format!("{admin_prefix}/{table}/{pk_value}")
    } else if form.contains_key("_addanother") {
        format!("{admin_prefix}/{table}/add")
    } else {
        format!("{admin_prefix}/{table}")
    }
}

// Takes `admin_prefix` because the admin can be mounted anywhere;
// hardcoding `/__admin` here 404s every facet and pager link.
fn build_query_url(admin_prefix: &str, table: &str, params: &[(String, String)]) -> String {
    if params.is_empty() {
        format!("{admin_prefix}/{table}")
    } else {
        let qs: Vec<String> = params
            .iter()
            .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
            .collect();
        format!("{admin_prefix}/{table}?{}", qs.join("&"))
    }
}

// The shared encoder: it covers the full RFC 3986 unreserved set.
// A narrower local one leaves `/`, `@` and non-ASCII bytes raw,
// which breaks facet values that contain them.
use crate::url_codec::url_encode;

// ============================================================== AUTOCOMPLETE
//
// Backs `autocomplete_fields`. A widget on the parent form calls
// `GET <admin>/<target>/__autocomplete?q=…` and puts the matches in
// a `<datalist>`.
//
// The route lives on the *target* model, not on the field's owner, so
// one route serves every form that points at it. A target with no
// searchable columns returns an empty list rather than every row.

pub(crate) async fn autocomplete_view(
    Path(table): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Result<axum::Json<serde_json::Value>, AdminError> {
    let model = resolve_model(&state, &table)?;
    let admin_cfg = admin_config_or_default(model);
    let q = params
        .get("q")
        .map(String::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();

    // Cap the result set so an unfiltered fetch cannot send millions
    // of rows over the wire.
    let limit: i64 = params
        .get("limit")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(20)
        .clamp(1, 100);

    let pk_field = match model.primary_key() {
        Some(f) => f,
        None => {
            return Ok(axum::Json(serde_json::json!({ "results": [] })));
        }
    };
    let display_field = model.display_field().unwrap_or(pk_field);

    // `admin.search_fields` when set, else the auto-searchable set.
    let search_columns: Vec<&'static str> = if admin_cfg.search_fields.is_empty() {
        model.searchable_fields().map(|f| f.column).collect()
    } else {
        admin_cfg
            .search_fields
            .iter()
            .filter_map(|name| model.field(name).map(|f| f.column))
            .collect()
    };
    let search = if q.is_empty() || search_columns.is_empty() {
        None
    } else {
        Some(SearchClause {
            columns: search_columns,
            query: q.clone(),
        })
    };

    let scalar_fields: Vec<&'static FieldSchema> = model.scalar_fields().collect();
    let rows = crate::sql::select_rows_as_json(
        &state.pool,
        &SelectQuery {
            search,
            order_by: vec![crate::core::OrderItem::column(display_field.column, false)],
            limit: Some(limit),
            offset: Some(0),
            ..SelectQuery::new(model)
        },
        &scalar_fields,
    )
    .await?;

    let results: Vec<serde_json::Value> = rows
        .into_iter()
        .filter_map(|row| {
            let id = row.get(pk_field.column)?.clone();
            let text = row
                .get(display_field.column)
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_else(|| id.to_string());
            Some(serde_json::json!({ "id": id, "text": text }))
        })
        .collect();

    Ok(axum::Json(serde_json::json!({ "results": results })))
}

// ============================================================== AUDIT LOG
//
// The audit route handlers and the emit helpers live in
// `super::audit`. The submit handlers below call
// `super::audit::emit_admin_audit*`.
// ============================================================== DETAIL

pub(crate) async fn detail_view(
    parts: axum::http::request::Parts,
    Path((table, pk_raw)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<Html<String>, AdminError> {
    let (model, pk_field, pk_value) = resolve_model_and_pk(&state, &table, &pk_raw)?;

    let detail_fields: Vec<&'static FieldSchema> = model.scalar_fields().collect();
    // Single PK lookup, plus LEFT JOINs that carry the FK display
    // names.
    let row = crate::sql::select_one_row_as_json(
        &state.pool,
        &SelectQuery {
            joins: build_fk_joins(&state, model),
            limit: None,
            ..SelectQuery::by_pk(model, pk_field.column, pk_value.clone())
        },
        &detail_fields,
    )
    .await?
    .ok_or(AdminError::RowNotFound {
        table: table.clone(),
        pk: pk_raw.clone(),
    })?;

    // Object-level `has_view_permission(request, obj)`. If any
    // registered `view` hook returns false, answer 403 before the
    // detail HTML is rendered.
    if !crate::admin::object_permissions::is_allowed(model.table, "view", &parts, Some(&row)) {
        return Err(AdminError::Forbidden {
            table: model.table.to_owned(),
            action: "view",
        });
    }

    // Read joined FK display values from the same row — no extra queries.
    let fk_map = fk_map_from_joined_rows_json(&state, model, std::slice::from_ref(&row));

    let mut cells_ctx: Vec<serde_json::Value> = model
        .scalar_fields()
        .map(|f| {
            serde_json::json!({
                "label": f.display_label(),
                "value": render_cell_json(&row, f, &fk_map),
            })
        })
        .collect();

    // One row per registered computed field, so the detail view
    // shows the same derived columns as the list view.
    for cf in crate::admin::computed_fields::for_table(model.table) {
        cells_ctx.push(serde_json::json!({
            "label": if cf.label.is_empty() { cf.name } else { cf.label },
            "value": (cf.render)(&row),
        }));
    }

    // One row per `#[rustango(generic_fk(...))]` declaration: read
    // the `(content_type_id, object_pk)` pair and render a link to
    // the target. A stale reference (CT not seeded, target deleted)
    // falls back to `(ct=N, pk=M)` instead of failing the page.
    for gfk in model.generic_relations {
        let ct_id = row
            .get(gfk.ct_column)
            .and_then(serde_json::Value::as_i64)
            .unwrap_or_default();
        let object_pk = row
            .get(gfk.pk_column)
            .and_then(serde_json::Value::as_i64)
            .unwrap_or_default();
        let g = crate::contenttypes::GenericForeignKey::new(ct_id, object_pk);
        let html = crate::contenttypes::render_generic_fk_link(&state.pool, g)
            .await
            .unwrap_or_else(|_| format!("<em>(ct={ct_id}, pk={object_pk})</em>"));
        cells_ctx.push(serde_json::json!({
            "label": gfk.name,
            "value": html,
        }));
    }

    // Inline panels: for every `register_admin_inline!` on this
    // parent table, fetch the child rows and render a panel.
    // Generic (ContentType-keyed) inlines come after the FK ones, in
    // registration order. Best-effort: a fetch error, such as a
    // child table missing on this tenant, drops the panels to empty
    // instead of failing the page.
    let mut inline_panels = super::inlines::render_for_parent(&state.pool, model, pk_value.clone())
        .await
        .unwrap_or_default();
    let generic_panels = super::inlines::render_generic_for_parent(&state.pool, model, pk_value)
        .await
        .unwrap_or_default();
    inline_panels.extend(generic_panels);
    let inline_panels_ctx: Vec<serde_json::Value> = inline_panels
        .into_iter()
        .map(|p| serde_json::to_value(p).unwrap_or(serde_json::Value::Null))
        .collect();

    // Audit-trail panel for this row. Best-effort: if the audit
    // table is missing, because the project never called
    // `audit::ensure_table` for this tenant, render an empty
    // section instead of failing the page.
    let audit_entries_ctx: Vec<serde_json::Value> =
        match crate::audit::fetch_for_entity_pool(&state.pool, model.table, &pk_raw).await {
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

    // Side panel with the user's roles and effective permissions,
    // for `rustango_users` only. Best-effort, like the audit panel:
    // unseeded permission tables render an empty section. Needs the
    // `tenancy` feature because it reads tenant tables.
    #[cfg(feature = "tenancy")]
    let user_roles_ctx: Option<serde_json::Value> = if model.table == "rustango_users" {
        user_roles_panel_ctx(&state, &pk_raw).await
    } else {
        None
    };
    #[cfg(not(feature = "tenancy"))]
    let user_roles_ctx: Option<serde_json::Value> = None;

    let mut ctx = serde_json::json!({
        "model": {
            "name": model.name,
            "table": model.table,
            // Captions from `#[rustango(verbose_name = "…",
            // verbose_name_plural = "…")]`. Templates use these for
            // headings and breadcrumbs, and `name` for routing.
            "label": model.display_label(),
            "label_plural": model.display_label_plural(),
        },
        "pk": pk_raw,
        "cells": cells_ctx,
        "read_only": state.is_read_only(model.table),
        "audit_entries": audit_entries_ctx,
        "user_roles_panel": user_roles_ctx,
        "inline_panels": inline_panels_ctx,
    });
    let html = render_with_chrome(
        "detail.html",
        &mut ctx,
        chrome_context(&state, Some(model.table)),
    );
    Ok(Html(html))
}

/// Build the roles and effective-permissions panel for a
/// `rustango_users` detail page. Returns `None` when a lookup fails,
/// for example when the tables are not ensured yet. The template
/// then hides the section.
#[cfg(feature = "tenancy")]
async fn user_roles_panel_ctx(state: &AppState, pk_raw: &str) -> Option<serde_json::Value> {
    let user_id: i64 = pk_raw.parse().ok()?;
    let roles = crate::tenancy::permissions::user_roles_qs_pool(user_id, &state.pool)
        .await
        .ok()?;
    let perms = crate::tenancy::permissions::user_permissions_pool(user_id, &state.pool)
        .await
        .ok()?;
    let roles_ctx: Vec<serde_json::Value> = roles
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id.get().copied().unwrap_or(0),
                "name": r.name,
                "description": r.description,
            })
        })
        .collect();
    Some(serde_json::json!({
        "roles": roles_ctx,
        "permissions": perms,
    }))
}

// ============================================================== CREATE

pub(crate) async fn create_form(
    parts: axum::http::request::Parts,
    Path(table): Path<String>,
    State(state): State<AppState>,
) -> Result<Html<String>, AdminError> {
    let model = resolve_model(&state, &table)?;
    if !state.can_add(model.table) {
        return Err(AdminError::ReadOnly {
            table: model.table.to_owned(),
        });
    }
    // `has_add_permission(request)` hook. No row exists yet, so the
    // hook gets `None`.
    if !crate::admin::object_permissions::is_allowed(model.table, "add", &parts, None) {
        return Err(AdminError::Forbidden {
            table: model.table.to_owned(),
            action: "add",
        });
    }
    // Preload the ContentType list so a `generic_fk` ct_column
    // renders as a `<select>` rather than a raw integer input. An
    // empty list on failure falls back to the plain input.
    let gfk_cts = preload_gfk_cts(&state, model).await;
    Ok(Html(super::helpers::render_form_with_inlines_and_picker(
        &state,
        model,
        None,
        /* pk_locked */ false,
        None,
        Vec::new(),
        &gfk_cts,
    )))
}

/// Load every ContentType when the model declares a `generic_fk`.
/// Returns an empty list otherwise: the picker only renders when the
/// model has a generic_fk and the CT list is non-empty.
async fn preload_gfk_cts(
    state: &AppState,
    model: &'static crate::core::ModelSchema,
) -> Vec<crate::contenttypes::ContentType> {
    if model.generic_relations.is_empty() {
        return Vec::new();
    }
    crate::contenttypes::ContentType::all_ordered(&state.pool)
        .await
        .unwrap_or_default()
}

pub(crate) async fn create_submit(
    parts: axum::http::request::Parts,
    Path(table): Path<String>,
    State(state): State<AppState>,
    Form(form): Form<HashMap<String, String>>,
) -> Result<Response, AdminError> {
    let model = resolve_model(&state, &table)?;
    if !state.can_add(model.table) {
        return Err(AdminError::ReadOnly {
            table: model.table.to_owned(),
        });
    }
    // `has_add_permission(request)` hook.
    if !crate::admin::object_permissions::is_allowed(model.table, "add", &parts, None) {
        return Err(AdminError::Forbidden {
            table: model.table.to_owned(),
            action: "add",
        });
    }

    let pk_field = primary_key_or_internal(model)?;
    // An auto PK is server-assigned and `readonly_fields` are
    // display-only, so neither belongs in the INSERT.
    let admin_cfg = admin_config_or_default(model);
    let mut skip: Vec<&str> = admin_cfg.readonly_fields.to_vec();
    if pk_field.auto {
        skip.push(pk_field.name);
    }
    let collected = match forms::collect_insert_values(model, &form, &skip) {
        Ok(v) => v,
        Err(e) => {
            // Re-render the form with the error instead of a 4xx.
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
    // Insert, then read back the PK. PG and SQLite use RETURNING.
    // MySQL has no RETURNING, so the helper returns
    // `LAST_INSERT_ID()`.
    let pk_value = match crate::sql::insert_returning_pool(&state.pool, &query).await {
        #[cfg(feature = "postgres")]
        Ok(crate::sql::InsertReturningPool::PgRow(row)) => {
            render::read_value_as_string(&row, pk_field).unwrap_or_default()
        }
        #[cfg(feature = "mysql")]
        Ok(crate::sql::InsertReturningPool::MySqlAutoId(id)) => id.to_string(),
        #[cfg(feature = "sqlite")]
        Ok(crate::sql::InsertReturningPool::SqliteRow(row)) => {
            // SQLite returns a typed row. Convert it to JSON once so
            // the read path matches the rest of the admin.
            let row_fields: Vec<&'static FieldSchema> = model.scalar_fields().collect();
            let json = crate::sql::row_to_json_sqlite(&row, &row_fields);
            render::read_value_as_string_json(&json, pk_field).unwrap_or_default()
        }
        Err(e) => {
            let html = render_form(&state, model, Some(&form), false, Some(&e.to_string()));
            return Ok(Html(html).into_response());
        }
    };
    super::audit::emit_admin_audit(
        &state,
        model,
        &pk_value,
        crate::audit::AuditOp::Create,
        &form,
    )
    .await;
    // `save_model` hook. It fires only on admin writes, not on every
    // ORM insert, so it is a seam for admin-only side effects.
    crate::signals::admin::send_admin_post_save(crate::signals::admin::AdminSaveContext {
        table: model.table,
        pk: pk_value.clone(),
        change: false,
    })
    .await;
    let target = post_save_redirect(&state.config.admin_prefix, model.table, &pk_value, &form);
    Ok(Redirect::to(&target).into_response())
}

// ============================================================== EDIT

pub(crate) async fn edit_form(
    parts: axum::http::request::Parts,
    Path((table, pk_raw)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<Html<String>, AdminError> {
    let (model, pk_field, pk_value) = resolve_model_and_pk(&state, &table, &pk_raw)?;

    let edit_fields: Vec<&'static FieldSchema> = model.scalar_fields().collect();
    // `by_pk` defaults to `limit = Some(1)`; the form wants the
    // whole row, so clear it.
    let row = crate::sql::select_one_row_as_json(
        &state.pool,
        &SelectQuery {
            limit: None,
            ..SelectQuery::by_pk(model, pk_field.column, pk_value.clone())
        },
        &edit_fields,
    )
    .await?
    .ok_or(AdminError::RowNotFound {
        table: table.clone(),
        pk: pk_raw.clone(),
    })?;

    // `has_change_permission(request, obj)` hook. Block the edit
    // form before any inline panel loads.
    if !crate::admin::object_permissions::is_allowed(model.table, "change", &parts, Some(&row)) {
        return Err(AdminError::Forbidden {
            table: model.table.to_owned(),
            action: "change",
        });
    }

    let mut prefill = HashMap::new();
    for f in model.scalar_fields() {
        prefill.insert(
            f.name.to_owned(),
            render::render_value_for_input_json(&row, f),
        );
    }
    // Editable inline panels under the parent form. Generic ones
    // come after the regular ones, in registration order.
    // Best-effort: a child-table fetch failure drops the inlines to
    // empty instead of breaking the edit page.
    let mut inline_panels =
        super::inlines::render_form_for_parent(&state.pool, model, pk_value.clone())
            .await
            .unwrap_or_default();
    let generic_panels =
        super::inlines::render_form_generic_for_parent(&state.pool, model, pk_value)
            .await
            .unwrap_or_default();
    inline_panels.extend(generic_panels);
    // Same preload as `create_form`, so the edit form also gets the
    // ContentType `<select>`.
    let gfk_cts = preload_gfk_cts(&state, model).await;
    Ok(Html(super::helpers::render_form_with_inlines_and_picker(
        &state,
        model,
        Some(&prefill),
        true,
        None,
        inline_panels,
        &gfk_cts,
    )))
}

pub(crate) async fn update_submit(
    parts: axum::http::request::Parts,
    Path((table, pk_raw)): Path<(String, String)>,
    State(state): State<AppState>,
    Form(form): Form<HashMap<String, String>>,
) -> Result<Response, AdminError> {
    let model = resolve_model(&state, &table)?;
    if state.is_read_only(model.table) {
        return Err(AdminError::ReadOnly {
            table: model.table.to_owned(),
        });
    }
    let pk_field = primary_key_or_internal(model)?;
    let pk_value = forms::parse_pk_string(pk_field, &pk_raw).map_err(AdminError::Form)?;

    // `has_change_permission(request, obj)`. Fetch the row first so
    // the hook sees the state before the update. A select error is
    // surfaced as-is, so a permission failure is never hidden
    // behind a 500.
    let pre_update_fields: Vec<&'static FieldSchema> = model.scalar_fields().collect();
    let pre_update_row = crate::sql::select_one_row_as_json(
        &state.pool,
        &SelectQuery {
            limit: None,
            ..SelectQuery::by_pk(model, pk_field.column, pk_value.clone())
        },
        &pre_update_fields,
    )
    .await?;
    if !crate::admin::object_permissions::is_allowed(
        model.table,
        "change",
        &parts,
        pre_update_row.as_ref(),
    ) {
        return Err(AdminError::Forbidden {
            table: model.table.to_owned(),
            action: "change",
        });
    }

    // Never SET the PK: identity must stay stable. Same for
    // `readonly_fields`. The form renders them as `readonly` inputs,
    // but a crafted POST can still send them, so they **must** be
    // skipped on the server too.
    let admin_cfg = admin_config_or_default(model);
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
        .map(|(column, value)| Assignment {
            column,
            value: value.into(),
        })
        .collect();

    // Reuse the row the permission hook already fetched: the audit
    // diff wants the same snapshot. The emit writes
    // `{ "field": { "before": v, "after": v } }`. If the fetch
    // returned None, because of a concurrent delete, the emit falls
    // back to a plain snapshot.
    let before_row = pre_update_row.clone();

    let query = UpdateQuery {
        model,
        set: assignments,
        where_clause: WhereExpr::Predicate(Filter {
            column: pk_field.column,
            op: Op::Eq,
            value: pk_value,
        }),
    };
    if let Err(e) = crate::sql::update_pool(&state.pool, &query).await {
        let html = render_form(&state, model, Some(&form), true, Some(&e.to_string()));
        return Ok(Html(html).into_response());
    }
    // "Before" comes from the SELECT, "after" from the form. The
    // per-request `with_source(User { id })` that `tenancy::admin`
    // installs gives a "who changed what" trail for free.
    super::audit::emit_admin_audit_diff(&state, model, &pk_raw, before_row.as_ref(), &form).await;
    // The `post_save` hook fires after the UPDATE and the audit
    // emit. `change = true` marks this as an edit, not a create.
    crate::signals::admin::send_admin_post_save(crate::signals::admin::AdminSaveContext {
        table: model.table,
        pk: pk_raw.clone(),
        change: true,
    })
    .await;

    // Apply the inline FormSet payloads, generic ones last. The
    // parent UPDATE has already committed here, and there is no
    // transaction across rows: a per-row write failure is counted
    // and logged, not rolled back.
    let parent_pk_for_inlines =
        forms::parse_pk_string(pk_field, &pk_raw).map_err(AdminError::Form)?;
    let _ =
        super::inlines::apply_post(&state.pool, model, parent_pk_for_inlines.clone(), &form).await;
    let _ =
        super::inlines::apply_post_generic(&state.pool, model, parent_pk_for_inlines, &form).await;

    let target = post_save_redirect(&state.config.admin_prefix, model.table, &pk_raw, &form);
    Ok(Redirect::to(&target).into_response())
}

// ============================================================== DELETE

pub(crate) async fn delete_submit(
    parts: axum::http::request::Parts,
    Path((table, pk_raw)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<Response, AdminError> {
    let model = resolve_model(&state, &table)?;
    if !state.can_delete(model.table) {
        return Err(AdminError::ReadOnly {
            table: model.table.to_owned(),
        });
    }
    let pk_field = primary_key_or_internal(model)?;
    let pk_value = forms::parse_pk_string(pk_field, &pk_raw).map_err(AdminError::Form)?;

    // Read the row before deleting it, so the audit entry records
    // what was removed. A missing row leaves the changes payload
    // empty, which still logs the operation and the source.
    let delete_fields: Vec<&'static FieldSchema> = model.scalar_fields().collect();
    let before_row = crate::sql::select_one_row_as_json(
        &state.pool,
        &SelectQuery {
            limit: None,
            ..SelectQuery::by_pk(model, pk_field.column, pk_value.clone())
        },
        &delete_fields,
    )
    .await
    .ok()
    .flatten();

    // `has_delete_permission(request, obj)` hook. It **must** run
    // before any soft-delete UPDATE or hard DELETE.
    if !crate::admin::object_permissions::is_allowed(
        model.table,
        "delete",
        &parts,
        before_row.as_ref(),
    ) {
        return Err(AdminError::Forbidden {
            table: model.table.to_owned(),
            action: "delete",
        });
    }

    let audit_op = if model.soft_delete_column.is_some() {
        crate::audit::AuditOp::SoftDelete
    } else {
        crate::audit::AuditOp::Delete
    };

    if let Some(col) = model.soft_delete_column {
        crate::sql::update_pool(
            &state.pool,
            &UpdateQuery {
                model,
                set: vec![Assignment {
                    column: col,
                    value: SqlValue::from(chrono::Utc::now()).into(),
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
        crate::sql::delete_pool(
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
                .map(|f| (f.name, render::read_value_as_json_from_json(row, f)))
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
    if let Err(e) = crate::audit::emit_one_pool(&state.pool, &entry).await {
        tracing::warn!(
            target: "rustango::admin::audit",
            error = %e,
            entity_table = %model.table,
            entity_pk = %pk_raw,
            "admin audit emit failed for delete",
        );
    }
    // The `delete_model` hook fires after the delete and the audit
    // emit, in both soft and hard delete mode.
    crate::signals::admin::send_admin_post_delete(crate::signals::admin::AdminDeleteContext {
        table: model.table,
        pk: pk_raw.clone(),
    })
    .await;
    Ok(Redirect::to(&format!("{}/{}", state.config.admin_prefix, model.table)).into_response())
}

// ============================================================== ACTIONS

/// `POST /<table>/__action`: run a bulk action. Form payload:
///
/// ```text
/// action=<name>&_selected=<pk1>&_selected=<pk2>&...
/// ```
///
/// `<name>` **must** be in the model's `admin.actions` allowlist; an
/// unknown name is rejected. The built-in `delete_selected` runs one
/// `DELETE WHERE pk IN (...)`. With no action or no selected rows,
/// redirect back to the list.
pub(crate) async fn action_submit(
    Path(table): Path<String>,
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Response, AdminError> {
    let model = resolve_model(&state, &table)?;

    // Parse the form keeping repeated keys. `Form<HashMap>` would
    // collapse the duplicate `_selected` keys into one, so read the
    // raw body into a `Vec` of pairs instead.
    let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(&body)
        .map_err(|e| AdminError::Internal(format!("parse action form: {e}")))?;

    // The bottom action bar posts `action_bottom` so it does not
    // clash with the top bar's empty default. The first non-empty
    // value wins, whichever bar sent it.
    let mut action_name: Option<String> = None;
    let mut selected_raw: Vec<String> = Vec::new();
    for (k, v) in pairs {
        if (k == "action" || k == "action_bottom") && !v.is_empty() && action_name.is_none() {
            action_name = Some(v);
        } else if k == "_selected" {
            selected_raw.push(v);
        }
    }
    let Some(action) = action_name.filter(|s| !s.is_empty()) else {
        // No action picked: go back to the list.
        return Ok(
            Redirect::to(&format!("{}/{}", state.config.admin_prefix, model.table)).into_response(),
        );
    };
    if selected_raw.is_empty() {
        return Ok(
            Redirect::to(&format!("{}/{}", state.config.admin_prefix, model.table)).into_response(),
        );
    }

    let admin_cfg = admin_config_or_default(model);
    if !admin_cfg.actions.iter().any(|a| *a == action) {
        return Err(AdminError::Internal(format!(
            "action `{action}` not registered for `{}`",
            model.name
        )));
    }

    let pk_field = primary_key_or_internal(model)?;

    let pk_values: Vec<SqlValue> = selected_raw
        .iter()
        .filter_map(|raw| forms::parse_pk_string(pk_field, raw).ok())
        .collect();
    if pk_values.is_empty() {
        return Ok(
            Redirect::to(&format!("{}/{}", state.config.admin_prefix, model.table)).into_response(),
        );
    }

    // Read every selected row before the action runs, so the audit
    // emit records what it ran against. For `delete_selected` this
    // is the only copy of the rows that are about to go.
    let action_fields: Vec<&'static FieldSchema> = model.scalar_fields().collect();
    let before_rows = crate::sql::select_rows_as_json(
        &state.pool,
        &SelectQuery::by_pk_in(model, pk_field.column, pk_values.clone()),
        &action_fields,
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
            // Soft delete: stamp the column instead of a DELETE.
            crate::sql::update_pool(
                &state.pool,
                &UpdateQuery {
                    model,
                    set: vec![Assignment {
                        column: col,
                        value: SqlValue::from(chrono::Utc::now()).into(),
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
            crate::sql::delete_pool(
                &state.pool,
                &DeleteQuery::by_pk_in(model, pk_field.column, pk_values),
            )
            .await?;
        }
    } else if action == "restore_selected" {
        if state.is_read_only(model.table) {
            return Err(AdminError::ReadOnly {
                table: model.table.to_owned(),
            });
        }
        // Built-in restore: clear the soft-delete column, where NULL
        // means live. A model without that column is a no-op, so
        // callers do not have to guard the action.
        if let Some(col) = model.soft_delete_column {
            crate::sql::update_pool(
                &state.pool,
                &UpdateQuery {
                    model,
                    set: vec![Assignment {
                        column: col,
                        value: SqlValue::Null.into(),
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
        // Handlers get the `Pool` enum, so a user action can match
        // on the backend if it needs to.
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

    // One audit entry per row, emitted in a single batched INSERT.
    // For `delete_selected` the changes JSON is what was deleted.
    // Other actions record the pre-action state plus an `__action`
    // marker, so the panel shows who ran what against which rows.
    let source = crate::audit::current_source();
    let entries: Vec<crate::audit::PendingEntry> = before_rows
        .iter()
        .map(|row| {
            let pk_str = render::read_value_as_string_json(row, pk_field).unwrap_or_default();
            let mut pairs: Vec<(&str, serde_json::Value)> = model
                .scalar_fields()
                .map(|f| (f.name, render::read_value_as_json_from_json(row, f)))
                .collect();
            if action != "delete_selected" {
                // Tag the action name so the audit row reads as
                // "alice ran publish_selected", not a plain edit.
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
        if let Err(e) = crate::audit::emit_many_pool(&state.pool, &entries).await {
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

    Ok(Redirect::to(&format!("{}/{}", state.config.admin_prefix, model.table)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Post-save redirect routing:
    //   _continue   → detail page
    //   _addanother → empty add form
    //   anything else, including _save → list view

    fn form_with(field: &str) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert(field.to_owned(), "1".to_owned());
        m
    }

    #[test]
    fn default_save_redirects_to_list_view() {
        let url = post_save_redirect("/__admin", "post", "42", &form_with("_save"));
        assert_eq!(url, "/__admin/post");
    }

    #[test]
    fn save_with_no_button_name_redirects_to_list_view() {
        // Some clients submit without the button name, such as a
        // JS-driven `form.submit()`. The default is the list.
        let url = post_save_redirect("/__admin", "post", "42", &HashMap::new());
        assert_eq!(url, "/__admin/post");
    }

    #[test]
    fn continue_redirects_to_detail() {
        let url = post_save_redirect("/__admin", "post", "42", &form_with("_continue"));
        assert_eq!(url, "/__admin/post/42");
    }

    #[test]
    fn addanother_redirects_to_create_form() {
        let url = post_save_redirect("/__admin", "post", "42", &form_with("_addanother"));
        assert_eq!(url, "/__admin/post/add");
    }

    #[test]
    fn continue_takes_precedence_over_addanother() {
        // Both fields present, e.g. a double-click race. Prefer the
        // safer choice: stay on the record just saved.
        let mut form = form_with("_continue");
        form.insert("_addanother".to_owned(), "1".to_owned());
        let url = post_save_redirect("/__admin", "post", "42", &form);
        assert_eq!(url, "/__admin/post/42");
    }

    #[test]
    fn admin_prefix_is_honored() {
        // An app that mounts the admin at `/manage` instead of the
        // default `/__admin` gets the right base path everywhere.
        let url = post_save_redirect("/manage", "post", "42", &form_with("_continue"));
        assert_eq!(url, "/manage/post/42");
    }
}
