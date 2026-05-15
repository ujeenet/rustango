//! Admin view handlers — Django's `views.py` shape.
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
    build_fk_joins, chrome_context, fk_map_from_joined_rows_json, lookup_model, pager_suffix,
    render_cell_json, render_form,
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
    let mut entries: Vec<&'static ModelEntry> = super::helpers::inventory_entries_dedup_by_table()
        .into_iter()
        // v0.27.7 — registry-scoped models hidden in tenant mode.
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
const RESERVED_PARAMS: &[&str] = &["page", "q", "facet_show_all", "count"];

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

    // v0.30.9 — skip `SELECT COUNT(*)` for big tables. Triggered by
    // `Builder::skip_count_for(...)` (per-table opt-in) OR
    // `?count=skip` / `?count=0` (per-request override). Skipping
    // means: no pager total, "Page N" instead of "Page N of M",
    // and prev/next driven by has-next-page detection (we fetch
    // `page_size + 1` rows and trim).
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
                // Apply the same ILIKE search the SELECT uses so the
                // pager total matches the visible rows. Pre-v0.30 the
                // count was approximate when ?q was set — fixed alongside
                // the viewset count-with-search bug.
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
    // When count is skipped, fetch one extra row so we can detect
    // "has more" without counting the whole table. We trim the
    // extra row before rendering.
    let fetch_limit = if count_skipped {
        page_size + 1
    } else {
        page_size
    };
    let scalar_fields: Vec<&'static FieldSchema> = model.scalar_fields().collect();
    let mut rows = crate::sql::select_rows_as_json_pool(
        &state.pool,
        &SelectQuery {
            model,
            where_clause,
            search: search.clone(),
            joins,
            order_by,
            limit: Some(fetch_limit),
            offset: Some(offset),
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
        // No total → no last page. Pager renders "Page N" with
        // prev/next driven by `has_next_skipped` instead.
        page
    } else if total == 0 {
        1
    } else {
        ((total - 1) / page_size) + 1
    };
    let read_only = state.is_read_only(model.table);

    // Resolve the columns shown on the list. If `admin.list_display`
    // is set, each entry resolves to one of:
    //
    // 1. a declared scalar field (the column-name path),
    // 2. a registered computed field for this table (the
    //    `register_admin_computed!` path — receives the row and returns
    //    HTML),
    //
    // Names that match neither are silently dropped. Empty
    // `list_display` falls back to every scalar field (today's
    // behavior).
    enum DisplayItem {
        Field(&'static FieldSchema),
        Computed(&'static crate::admin::computed_fields::ComputedField),
    }
    let display_items: Vec<DisplayItem> = if admin_cfg.list_display.is_empty() {
        model.scalar_fields().map(DisplayItem::Field).collect()
    } else {
        admin_cfg
            .list_display
            .iter()
            .filter_map(|name| {
                model.field(name).map(DisplayItem::Field).or_else(|| {
                    crate::admin::computed_fields::find(model.table, name)
                        .map(DisplayItem::Computed)
                })
            })
            .collect()
    };

    // Per-column header label. Scalar columns get a `<small>(pk)</small>`
    // suffix on the PK; computed fields show their declared label
    // (falling back to the bare identifier).
    let columns_ctx: Vec<serde_json::Value> = display_items
        .iter()
        .map(|item| {
            let label = match item {
                DisplayItem::Field(f) => {
                    if f.primary_key {
                        format!("{} <small>(pk)</small>", render::escape(f.name))
                    } else {
                        render::escape(f.name)
                    }
                }
                DisplayItem::Computed(m) => {
                    render::escape(if m.label.is_empty() { m.name } else { m.label })
                }
            };
            serde_json::json!({ "label": label })
        })
        .collect();

    // Per-row payload. Computed-field cells are pre-escaped HTML
    // supplied by the user's closure; scalar cells go through the
    // standard `render_cell_json` path (FK link or escaped scalar).
    //
    // v0.37 — rows are already `serde_json::Value` from the JSON
    // bridge; cell + PK rendering go through `*_json` companions so
    // this loop compiles on any backend.
    let rows_ctx: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let cells: Vec<String> = display_items
                .iter()
                .map(|item| match item {
                    DisplayItem::Field(f) => render_cell_json(row, f, &fk_map),
                    DisplayItem::Computed(m) => (m.render)(row),
                })
                .collect();
            let pk =
                pk_field.map(|pk| render::escape(&render::render_value_for_input_json(row, pk)));
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
        // v0.30.9 — count-skip pager fields. Templates branch on
        // `count_skipped` to render "Page N" + prev/next driven by
        // `has_next` instead of "Page N of M". Existing custom
        // templates that ignore these vars keep working — they
        // just see total=0 and last_page=page, which renders no
        // pager (the existing `if last_page > 1` guard).
        "count_skipped": count_skipped,
        "has_next": has_next_skipped,
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
                crate::core::Relation::Fk { to, on } | crate::core::Relation::O2O { to, on } => {
                    let target = lookup_model(state, to)?;
                    let display_field = target.display_field()?;
                    Some((target.table, on, display_field.column))
                }
            });

        // v0.13.1: order facets by count desc so the most active
        // value floats to the top. Tie-break alphabetically by the
        // displayed value so output stays deterministic across
        // requests.
        //
        // v0.37 — SQL is rendered through the dialect's quote_ident
        // emitter so identifier quoting works on PG/MySQL/SQLite
        // uniformly. The two GROUP BY shapes (FK-joined vs flat)
        // dispatch through the Pool enum via `raw_query_pool::<T>`
        // returning typed `(facet_value, facet_count[, facet_display])`
        // tuples.
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
            // Stringify the value at the `facet_value` column alias.
            // Same shape `parse_form_value` accepts back when the URL
            // round-trips through the filter machinery.
            //
            // v0.37 — `raw_value` is the already-stringified column
            // value (we ask the per-backend fetch to stringify to keep
            // the executor type-erased); `render::read_value_as_string_at`
            // was only ever used to convert PG's typed value, the same
            // type-erasure happens inside `fetch_facet_rows` now.
            let raw = raw_value.clone();
            // Display: for FK fields with a JOIN, prefer the target's
            // display value; otherwise fall back to the raw key.
            let display = if raw.is_empty() {
                "—".to_owned()
            } else if let Some(d) = display_text.as_deref().filter(|s| !s.is_empty()) {
                render::escape(d)
            } else {
                render::escape(&raw)
            };
            let count: i64 = *count;
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
            Some(build_query_url(
                state.config.admin_prefix.as_str(),
                model.table,
                &params,
            ))
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

/// v0.37 — run a `GROUP BY` facet SELECT through the right backend.
/// Returns `(raw_value, optional_display_text, count)` triples,
/// stringifying the typed column value uniformly so the caller can
/// type-erase. `expect_display` is `true` for the FK-joined facet
/// (which also reads the joined display column).
async fn fetch_facet_rows(
    pool: &crate::sql::Pool,
    sql: &str,
    expect_display: bool,
) -> Result<Vec<(String, Option<String>, i64)>, sqlx::Error> {
    use sqlx::Row as _;
    match pool {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => {
            let rows = sqlx::query(sql).fetch_all(pg).await?;
            let mut out = Vec::with_capacity(rows.len());
            for r in rows {
                // facet_value column may be any scalar type. Try the
                // most common shapes; fall back to empty string if
                // every decode fails (matches the legacy unwrap_or_default
                // behaviour).
                let raw = stringify_facet_value_pg(&r);
                let display = if expect_display {
                    r.try_get::<Option<String>, _>("facet_display")
                        .ok()
                        .flatten()
                } else {
                    None
                };
                let count: i64 = r.try_get("facet_count").unwrap_or(0);
                out.push((raw, display, count));
            }
            Ok(out)
        }
        #[cfg(feature = "mysql")]
        crate::sql::Pool::Mysql(my) => {
            let rows = sqlx::query(sql).fetch_all(my).await?;
            let mut out = Vec::with_capacity(rows.len());
            for r in rows {
                let raw = stringify_facet_value_my(&r);
                let display = if expect_display {
                    r.try_get::<Option<String>, _>("facet_display")
                        .ok()
                        .flatten()
                } else {
                    None
                };
                let count: i64 = r.try_get("facet_count").unwrap_or(0);
                out.push((raw, display, count));
            }
            Ok(out)
        }
        #[cfg(feature = "sqlite")]
        crate::sql::Pool::Sqlite(sq) => {
            let rows = sqlx::query(sql).fetch_all(sq).await?;
            let mut out = Vec::with_capacity(rows.len());
            for r in rows {
                let raw = stringify_facet_value_sqlite(&r);
                let display = if expect_display {
                    r.try_get::<Option<String>, _>("facet_display")
                        .ok()
                        .flatten()
                } else {
                    None
                };
                let count: i64 = r.try_get("facet_count").unwrap_or(0);
                out.push((raw, display, count));
            }
            Ok(out)
        }
    }
}

/// Try decoding the `facet_value` column as text first, then as the
/// numeric / boolean scalars admin facets commonly hit. Returns an
/// empty string when every shape fails — matches the v0.13.x
/// `unwrap_or_default()` legacy behaviour.
#[cfg(feature = "postgres")]
fn stringify_facet_value_pg(row: &sqlx::postgres::PgRow) -> String {
    use sqlx::Row as _;
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

#[cfg(feature = "mysql")]
fn stringify_facet_value_my(row: &sqlx::mysql::MySqlRow) -> String {
    use sqlx::Row as _;
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

#[cfg(feature = "sqlite")]
fn stringify_facet_value_sqlite(row: &sqlx::sqlite::SqliteRow) -> String {
    use sqlx::Row as _;
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

// v0.46 — Django save-and-X redirect target.
//
// The admin's change/add form ships three submit buttons. Their
// `name="..."` attribute tells the handler where to send the user
// after a successful save:
//
//   <button name="_save"        … >Save</button>
//   <button name="_continue"    … >Save and continue editing</button>
//   <button name="_addanother"  … >Save and add another</button>
//
// Matching Django's `BaseModelAdmin.response_post_save_*` conventions
// down to the literal field names so muscle memory carries over.
pub(crate) fn post_save_redirect(
    admin_prefix: &str,
    table: &str,
    pk_value: &str,
    form: &HashMap<String, String>,
) -> String {
    if form.contains_key("_continue") {
        // Stay on the detail page for further edits.
        format!("{admin_prefix}/{table}/{pk_value}")
    } else if form.contains_key("_addanother") {
        // Land on the empty create form.
        format!("{admin_prefix}/{table}/add")
    } else {
        // Default `_save` → list view.
        format!("{admin_prefix}/{table}")
    }
}

// v0.31.1 (#5): take `admin_prefix` instead of hardcoding `/__admin`.
// On the v0.29+ friendly default the facet toggle / clear / show-all
// URLs all 404'd until the caller corrected them by hand.
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

    let detail_fields: Vec<&'static FieldSchema> = model.scalar_fields().collect();
    let row = crate::sql::select_one_row_as_json_pool(
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
        &detail_fields,
    )
    .await?
    .ok_or(AdminError::RowNotFound {
        table: table.clone(),
        pk: pk_raw.clone(),
    })?;

    // Read joined FK display values from the same row — no extra queries.
    let fk_map = fk_map_from_joined_rows_json(&state, model, std::slice::from_ref(&row));

    let mut cells_ctx: Vec<serde_json::Value> = model
        .scalar_fields()
        .map(|f| {
            serde_json::json!({
                "label": f.name,
                "value": render_cell_json(&row, f, &fk_map),
            })
        })
        .collect();

    // v0.32 — append one row per registered admin computed field. The
    // detail view shows every computed column the user declared for
    // this table, mirroring the list-view behavior so authors don't
    // have to hunt for word counts / derived flags / etc. in the
    // single-row view.
    for cf in crate::admin::computed_fields::for_table(model.table) {
        cells_ctx.push(serde_json::json!({
            "label": if cf.label.is_empty() { cf.name } else { cf.label },
            "value": (cf.render)(&row),
        }));
    }

    // F.4b — append one row per #[rustango(generic_fk(...))]
    // declaration. Reads the (content_type_id, object_pk) pair off
    // the row and renders a clickable target link via
    // `contenttypes::render_generic_fk_link`. Stale references
    // (CT not seeded, target deleted) render as a `(ct=N, pk=M)`
    // fallback rather than failing the whole page.
    //
    // v0.37 — `ct_id` / `object_pk` come from the JSON row via
    // `as_i64()`; the generic-FK render helper has a tri-dialect
    // `_pool` companion that dispatches per backend.
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

    // v0.12.2: Audit trail panel for this row. Best-effort — if the
    // audit table doesn't exist yet (project hasn't called
    // `audit::ensure_table` per tenant), the lookup returns Err and
    // we render an empty section instead of failing the whole page.
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

    // v0.28 — for `rustango_users`, render the user's roles +
    // effective permissions in a side panel. Best-effort: if the
    // permission tables don't exist (project hasn't seeded them) we
    // render an empty section instead of failing the whole detail
    // page — same posture as the audit panel above. Gated behind
    // the `tenancy` feature since the panel reads tenant tables.
    #[cfg(feature = "tenancy")]
    let user_roles_ctx: Option<serde_json::Value> = if model.table == "rustango_users" {
        user_roles_panel_ctx(&state, &pk_raw).await
    } else {
        None
    };
    #[cfg(not(feature = "tenancy"))]
    let user_roles_ctx: Option<serde_json::Value> = None;

    let mut ctx = serde_json::json!({
        "model": { "name": model.name, "table": model.table },
        "pk": pk_raw,
        "cells": cells_ctx,
        "read_only": state.is_read_only(model.table),
        "audit_entries": audit_entries_ctx,
        "user_roles_panel": user_roles_ctx,
    });
    let html = render_with_chrome(
        "detail.html",
        &mut ctx,
        chrome_context(&state, Some(model.table)),
    );
    Ok(Html(html))
}

/// Build the roles + effective-permissions panel for a `rustango_users`
/// detail page. Returns `None` when either lookup fails (e.g. tables
/// not yet ensured) — the template hides the section in that case.
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
    // v0.37 — tri-dialect insert + PK extraction. PG/SQLite emit
    // RETURNING and we read the PK column off the row; MySQL has no
    // RETURNING so the helper hands back `LAST_INSERT_ID()` directly.
    let pk_value = match crate::sql::insert_returning_pool(&state.pool, &query).await {
        #[cfg(feature = "postgres")]
        Ok(crate::sql::InsertReturningPool::PgRow(row)) => {
            render::read_value_as_string(&row, pk_field).unwrap_or_default()
        }
        #[cfg(feature = "mysql")]
        Ok(crate::sql::InsertReturningPool::MySqlAutoId(id)) => id.to_string(),
        #[cfg(feature = "sqlite")]
        Ok(crate::sql::InsertReturningPool::SqliteRow(row)) => {
            // SQLite returns a typed row via RETURNING — convert it
            // to JSON once and reuse the JSON reader so the path
            // matches the rest of the admin's tri-dialect rendering.
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
    let target = post_save_redirect(&state.config.admin_prefix, model.table, &pk_value, &form);
    Ok(Redirect::to(&target).into_response())
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

    let edit_fields: Vec<&'static FieldSchema> = model.scalar_fields().collect();
    let row = crate::sql::select_one_row_as_json_pool(
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
        &edit_fields,
    )
    .await?
    .ok_or(AdminError::RowNotFound {
        table: table.clone(),
        pk: pk_raw.clone(),
    })?;

    let mut prefill = HashMap::new();
    for f in model.scalar_fields() {
        prefill.insert(
            f.name.to_owned(),
            render::render_value_for_input_json(&row, f),
        );
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
        .map(|(column, value)| Assignment {
            column,
            value: value.into(),
        })
        .collect();

    // v0.12.3: SELECT the row's pre-update state so the audit emit
    // can produce a `{ "field": { "before": v, "after": v } }`
    // diff. Best-effort — if the SELECT fails (race, concurrent
    // delete), we fall back to the snapshot path so the data write
    // still emits something useful.
    //
    // v0.37 — SELECT runs through the JSON bridge and the audit emit
    // takes `Option<&serde_json::Value>` directly, no shim needed.
    let before_fields: Vec<&'static FieldSchema> = model.scalar_fields().collect();
    let before_row = crate::sql::select_one_row_as_json_pool(
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
        &before_fields,
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
    if let Err(e) = crate::sql::update_pool(&state.pool, &query).await {
        let html = render_form(&state, model, Some(&form), true, Some(&e.to_string()));
        return Ok(Html(html).into_response());
    }
    // Diff path: before from the SELECT, after from the form. Picks
    // up the per-request `with_source(User { id })` install from
    // `tenancy::admin`, so operators get a "who changed what" trail
    // automatically.
    super::audit::emit_admin_audit_diff(&state, model, &pk_raw, before_row.as_ref(), &form).await;
    let target = post_save_redirect(&state.config.admin_prefix, model.table, &pk_raw, &form);
    Ok(Redirect::to(&target).into_response())
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
    let delete_fields: Vec<&'static FieldSchema> = model.scalar_fields().collect();
    let before_row = crate::sql::select_one_row_as_json_pool(
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
        &delete_fields,
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
    Ok(Redirect::to(&format!("{}/{}", state.config.admin_prefix, model.table)).into_response())
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
        return Ok(
            Redirect::to(&format!("{}/{}", state.config.admin_prefix, model.table)).into_response(),
        );
    };
    if selected_raw.is_empty() {
        return Ok(
            Redirect::to(&format!("{}/{}", state.config.admin_prefix, model.table)).into_response(),
        );
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
        return Ok(
            Redirect::to(&format!("{}/{}", state.config.admin_prefix, model.table)).into_response(),
        );
    }

    // v0.12.4: SELECT every selected row's pre-action state so the
    // audit emit can record what the action ran against. For
    // delete_selected this snapshots the gone rows; for user-defined
    // actions it records the row state at the time of action.
    let action_fields: Vec<&'static FieldSchema> = model.scalar_fields().collect();
    let before_rows = crate::sql::select_rows_as_json_pool(
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
            // Soft model — stamp the deleted_at column instead of hard DELETE.
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
        // v0.36 — `state.pool` is the tri-dialect `Pool` enum; action
        // handlers receive it directly so user-defined actions can
        // pattern-match on the backend.
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
            let pk_str = render::read_value_as_string_json(row, pk_field).unwrap_or_default();
            let mut pairs: Vec<(&str, serde_json::Value)> = model
                .scalar_fields()
                .map(|f| (f.name, render::read_value_as_json_from_json(row, f)))
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

    // v0.46.9 — post-save redirect routing matches Django's
    // `BaseModelAdmin.response_post_save_*` table:
    //   _continue   → detail (stay for further edits)
    //   _addanother → add form (empty)
    //   anything else (including _save) → list view

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
        // Some browsers / clients submit forms without picking up the
        // button name (e.g. JS-driven form.submit()). Default = list.
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
        // Both fields present (synthetic JS, double-click race, etc.)
        // → prefer the safer choice: stay on the just-saved record.
        let mut form = form_with("_continue");
        form.insert("_addanother".to_owned(), "1".to_owned());
        let url = post_save_redirect("/__admin", "post", "42", &form);
        assert_eq!(url, "/__admin/post/42");
    }

    #[test]
    fn admin_prefix_is_honored() {
        // Apps that mount the admin at `/manage` instead of the
        // default `/__admin` (#74 in the backlog) get the right
        // base path everywhere.
        let url = post_save_redirect("/manage", "post", "42", &form_with("_continue"));
        assert_eq!(url, "/manage/post/42");
    }
}
