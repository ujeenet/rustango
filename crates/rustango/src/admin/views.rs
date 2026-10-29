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
use axum::extract::{Form, Path, Query};
use axum::response::{Html, IntoResponse, Redirect, Response};

use super::errors::AdminError;
use super::forms;
use super::helpers::{
    admin_config_or_default, build_fk_joins, chrome_context, fk_display_targets,
    fk_map_from_joined_rows_json, is_secret_field, lookup_model, parse_pk, primary_key_or_internal,
    render_cell_json, render_secret_cell, resolve_model, resolve_model_and_pk, search_columns,
    url_filterable, FormLayout, ListQuery,
};
use super::object_permissions::AdminWrite;
use super::queryset_hooks::RowScope;
use super::render;
use super::templates::render_with_chrome;
use super::urls::{ActionHandler, ActionPerm, AppState, CREATE_SEGMENT};

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
/// Synchronous: the list and detail views preload the ContentTypes
/// with [`gfk_ct_map`] first, so there is no DB I/O per cell.
fn render_gfk_cell(
    row: &serde_json::Value,
    gr: &crate::core::GenericRelation,
    ct_map: &HashMap<i64, crate::contenttypes::ContentType>,
    admin_prefix: &str,
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
        // CT stale, not seeded, or a table the user may not view.
        return format!("<em>(ct={ct_id}, pk={object_pk})</em>");
    };
    let label = format!("{}.{}", ct.app_label, ct.model_name);
    let table_esc = render::escape(&ct.table);
    let label_esc = render::escape(&label);
    format!(
        r#"<a href="{prefix}/{table}/{pk}">{label} #{pk}</a>"#,
        prefix = render::escape(admin_prefix),
        table = table_esc,
        pk = object_pk,
        label = label_esc,
    )
}

/// Content types of the GFK targets in `rows`, by id. A target table the
/// user may not view is left out, so its cell gets no label or link (#2341).
async fn gfk_ct_map(
    state: &AppState,
    rows: &[serde_json::Value],
    relations: impl Iterator<Item = &'static crate::core::GenericRelation>,
) -> HashMap<i64, crate::contenttypes::ContentType> {
    use crate::sql::FetcherPool as _;
    let mut needed: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
    for gr in relations {
        needed.extend(
            rows.iter()
                .filter_map(|row| row.get(gr.ct_column).and_then(serde_json::Value::as_i64)),
        );
    }
    if needed.is_empty() {
        return HashMap::new();
    }
    // One round trip; ids are bounded by the page size.
    let ids = needed.into_iter().map(SqlValue::I64).collect();
    let cts: Vec<crate::contenttypes::ContentType> = crate::contenttypes::ContentType::objects()
        .filter_op("id", Op::In, SqlValue::List(ids))
        .fetch(&state.pool)
        .await
        .unwrap_or_default();
    cts.into_iter()
        .filter(|ct| lookup_model(state, &ct.table).is_some())
        .filter_map(|ct| Some((*ct.id.get()?, ct)))
        .collect()
}

// ============================================================== INDEX

pub(crate) async fn index(state: AppState) -> Html<String> {
    // Group registered models by app label. `resolved_app_label()`
    // returns the `#[rustango(app = "...")]` override, or infers one
    // from the module path. Models with no app label go to "Project".
    let mut entries: Vec<&'static ModelEntry> = super::helpers::inventory_entries_dedup_by_table()
        .into_iter()
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
    // Same permission gate and row scope as the `/__audit` feed.
    let recent = match state.audit_reader() {
        Some(reader) => reader
            .list(&crate::audit::AuditFilter::default(), 10, 0)
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let recent_actions_ctx: Vec<serde_json::Value> = recent
        .into_iter()
        .map(|entry| {
            let action_url = format!(
                "{}/{}/{}",
                state.config.admin_prefix,
                crate::url_codec::url_encode(&entry.entity_table),
                crate::url_codec::url_encode(&entry.entity_pk),
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
    // `trashed=1` lists the soft-deleted rows (#1918).
    "trashed",
];

/// Most keys one `IN` list binds: under every dialect's bind cap (#2049).
const MAX_IN_KEYS: usize = 10_000;

/// The explicit equality suffix, for a field named like a reserved param.
const EXACT: &str = "__exact";

/// How a list URL param selects rows on one field. NULL and `""` get a
/// `=1` suffix form, since `?field=` reads as "no filter" (#2006, #2081).
#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldLookup {
    Eq,
    IsNull,
    IsEmpty,
}

impl FieldLookup {
    const ALL: [Self; 3] = [Self::Eq, Self::IsNull, Self::IsEmpty];

    fn suffix(self) -> &'static str {
        match self {
            Self::Eq => "",
            Self::IsNull => "__isnull",
            Self::IsEmpty => "__isempty",
        }
    }

    /// Splits a URL key into the field name and its lookup.
    fn parse(key: &str) -> (&str, Self) {
        [Self::IsNull, Self::IsEmpty]
            .into_iter()
            .find_map(|l| Some((key.strip_suffix(l.suffix())?, l)))
            .or_else(|| Some((key.strip_suffix(EXACT)?, Self::Eq)))
            .unwrap_or((key, Self::Eq))
    }

    /// The URL key filtering `field` with this lookup. A field named like a
    /// reserved param (`page`, `year`, ...) is spelled `{field}__exact`.
    fn key(self, field: &str) -> String {
        let suffix = match self {
            Self::Eq if RESERVED_PARAMS.contains(&field) => EXACT,
            l => l.suffix(),
        };
        format!("{field}{suffix}")
    }

    /// Every URL key that can filter `field`, to clear them all.
    fn keys(field: &str) -> [String; 3] {
        Self::ALL.map(|l| l.key(field))
    }

    /// The URL pair that selects one facet value.
    fn param(field: &str, key: &SqlValue, raw: &str) -> (String, String) {
        let lookup = match key {
            SqlValue::Null => Self::IsNull,
            SqlValue::String(s) if s.is_empty() => Self::IsEmpty,
            _ => Self::Eq,
        };
        let value = if lookup == Self::Eq { raw } else { "1" };
        (lookup.key(field), value.to_owned())
    }
}

/// Default cap on how many values one facet shows. Keeps the right
/// rail compact on columns with many distinct values. The rest
/// collapse into a "+N more" link (`?facet_show_all=<field>`).
const FACET_TRUNCATE: usize = 15;

/// Most values `?facet_show_all=<field>` reads, so a high-cardinality
/// column never loads every distinct value.
const FACET_SHOW_ALL_CAP: usize = 200;

#[allow(clippy::too_many_lines)] // mostly linear HTML emission; splitting hurts readability
pub(crate) async fn table_view(
    parts: axum::http::request::Parts,
    Path(table): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    state: AppState,
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
    let page = crate::list_params::parse_page(&params);
    let offset = crate::list_params::page_offset(page, page_size);
    let q = params
        .get("q")
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    // Build per-field filters from extra query params. Unknown fields
    // and unparseable values are dropped: a bad URL should not 500.
    // Field filters stay apart so a facet can count without its own (#2004).
    let mut field_filters: Vec<(&'static str, Filter)> = Vec::new();
    let mut filters: Vec<Filter> = Vec::new();
    // The URL pairs of the field filters in force.
    let mut active_field_filters: Vec<(String, String)> = Vec::new();
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
        let (name, lookup) = FieldLookup::parse(key);
        // Only fields the list shows or filters on (#2031).
        let Some(field) = model
            .field(name)
            .filter(|f| url_filterable(model, &admin_cfg, f))
        else {
            continue;
        };
        if lookup != FieldLookup::Eq && value != "1" {
            continue;
        }
        let (op, v) = match lookup {
            FieldLookup::IsNull => (Op::IsNull, SqlValue::Bool(true)),
            FieldLookup::IsEmpty if field.ty == crate::core::FieldType::String => {
                (Op::Eq, SqlValue::String(String::new()))
            }
            FieldLookup::IsEmpty => continue,
            FieldLookup::Eq => {
                let Ok(v) = forms::parse_form_value(field, Some(value)) else {
                    continue;
                };
                (Op::Eq, v)
            }
        };
        field_filters.push((
            field.name,
            Filter {
                column: field.column,
                op,
                value: v,
            },
        ));
        active_field_filters.push((lookup.key(field.name), value.clone()));
    }
    active_field_filters.sort();

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

    // Queryset hooks only add WHERE conjuncts, so they compose with
    // search, facets, the date hierarchy and pagination.
    let trashed = model.soft_delete_column.is_some()
        && params.get("trashed").map(String::as_str) == Some("1");
    let scope = if trashed {
        RowScope::trashed(model, &parts)
    } else {
        RowScope::of(model, &parts)
    };
    filters.extend(scope.filters().iter().cloned());

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

    let search_columns = search_columns(model, &admin_cfg);
    let search = q.as_ref().map(|qstr| SearchClause {
        columns: search_columns.clone(),
        query: qstr.clone(),
    });

    // The "view" hook as a PK filter, so totals, pages, facets, date
    // buckets and search all leave its denied rows out. Every list query
    // shares `filters` and the search, so the scan reads only those rows.
    let aggregates_withheld =
        match view_hook_rows(&state, &parts, model, &filters, search.as_ref()).await? {
            ViewHookRows::All => false,
            ViewHookRows::Only(f) => {
                filters.push(f);
                false
            }
            ViewHookRows::TooMany => true,
        };

    let where_clause = WhereExpr::and_predicates(
        field_filters
            .iter()
            .map(|(_, f)| f.clone())
            .chain(filters.iter().cloned())
            .collect(),
    );

    // Skip `SELECT COUNT(*)` on big tables. Turned on per table by
    // `Builder::skip_count_for(...)`, or per request by `?count=skip`
    // or `?count=0`. Without a total the pager shows "Page N" instead
    // of "Page N of M", and prev/next come from fetching one extra
    // row and trimming it.
    let count_skipped = aggregates_withheld
        || state.count_skipped_for_table(model.table)
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
                source: None,
            },
        )
        .await?
    };
    let joins = build_fk_joins(&state, model, &parts);
    let order_by = list_order_by(model, &admin_cfg);
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
            where_clause: where_clause.clone(),
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
    let mut has_next_skipped = if count_skipped && rows.len() as i64 > page_size {
        rows.truncate(page_size as usize);
        true
    } else {
        false
    };
    // Past the scan cap no PK filter applies: "next" only when a row the
    // hook admits follows, else it reveals a denied one.
    if aggregates_withheld && has_next_skipped {
        let cap = i64::try_from(state.view_hook_scan_cap()).unwrap_or(i64::MAX);
        let after = crate::sql::select_rows_as_json(
            &state.pool,
            &SelectQuery {
                where_clause: where_clause.clone(),
                search: search.clone(),
                joins: build_fk_joins(&state, model, &parts),
                order_by: list_order_by(model, &admin_cfg),
                limit: Some(cap.max(1)),
                offset: Some(offset + page_size),
                ..SelectQuery::new(model)
            },
            &scalar_fields,
        )
        .await?;
        has_next_skipped = after.iter().any(|row| {
            crate::admin::object_permissions::is_allowed(model.table, "view", &parts, Some(row))
        });
    }
    // The detail page's "view" hook hides a row here too (#2231); the
    // PK filter above already did, unless the table was too big to scan.
    rows.retain(|row| {
        crate::admin::object_permissions::is_allowed(model.table, "view", &parts, Some(row))
    });

    let fk_map = fk_map_for_rows(&state, &parts, model, &rows).await?;

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
    // lookup per cell.
    let gfk_ct_map = gfk_ct_map(
        &state,
        &rows,
        model.generic_relations.iter().filter(|gr| {
            display_items
                .iter()
                .any(|i| matches!(i, DisplayItem::GenericFk(g) if g.name == gr.name))
        }),
    )
    .await;

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
            // A trashed row has no detail page.
            let pk_path =
                (!pk_raw.is_empty() && !trashed).then(|| crate::url_codec::url_encode(&pk_raw));
            let detail_href = pk_path.as_ref().map(|pk| {
                format!(
                    "{prefix}/{table}/{pk}",
                    prefix = state.config.admin_prefix,
                    table = model.table,
                )
            });
            let cells: Vec<String> = display_items
                .iter()
                .enumerate()
                .map(|(idx, item)| {
                    let inner = match item {
                        DisplayItem::Field(f) if is_secret_field(&admin_cfg, f.name) => {
                            render_secret_cell(row, f)
                        }
                        DisplayItem::Field(f) => {
                            render_cell_json(row, f, &fk_map, &state.config.admin_prefix)
                        }
                        DisplayItem::Computed(m) => (m.render)(row),
                        DisplayItem::GenericFk(gr) => {
                            render_gfk_cell(row, gr, &gfk_ct_map, &state.config.admin_prefix)
                        }
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
            serde_json::json!({ "cells": cells, "pk": pk, "pk_path": pk_path })
        })
        .collect();

    let active_filters_ctx: Vec<serde_json::Value> = active_field_filters
        .iter()
        .map(|(k, v)| serde_json::json!({ "key": k, "value": v }))
        .collect();
    // The whole filter state; every link below derives from it (#1916).
    let mut list_query = ListQuery::new(format!("{}/{}", state.config.admin_prefix, model.table));
    if let Some(qv) = q.as_deref() {
        list_query.push("q", qv);
    }
    for (k, v) in &active_field_filters {
        list_query.push(k.clone(), v.clone());
    }
    for (k, v) in &active_custom_filters {
        list_query.push(*k, v.clone());
    }
    if !admin_cfg.date_hierarchy.is_empty() {
        push_date(&mut list_query, date_sel.year, date_sel.month, date_sel.day);
    }
    for key in ["count", "facet_show_all"] {
        if let Some(v) = params.get(key).filter(|v| !v.is_empty()) {
            list_query.push(key, v.clone());
        }
    }
    if trashed {
        list_query.push("trashed", "1");
    }
    let pager_suffix_str = list_query.suffix();
    let hidden_params: Vec<serde_json::Value> = list_query
        .without(&["q"])
        .pairs()
        .iter()
        .map(|(k, v)| serde_json::json!({ "key": k, "value": v }))
        .collect();

    // Facet filters. For each `admin.list_filter` field, query its
    // distinct values and counts for a right-rail card. Each link
    // toggles `?<col>=<value>`: clicking the active value clears the
    // filter, clicking another value swaps to it.
    let show_all_facet = params.get("facet_show_all").map(String::as_str);
    let facets_ctx: Vec<serde_json::Value> = if aggregates_withheld {
        Vec::new()
    } else {
        compute_facets(
            &state,
            &parts,
            model,
            &field_filters,
            &filters,
            search.as_ref(),
            &admin_cfg,
            &active_field_filters,
            &list_query,
            show_all_facet,
        )
        .await?
    };

    // Date-hierarchy strip: the breadcrumb plus the child buckets at
    // the current drill level (year, month or day). One GROUP BY
    // query, narrowed by the parent level's WHERE.
    let date_hierarchy_ctx: Option<serde_json::Value> =
        if admin_cfg.date_hierarchy.is_empty() || aggregates_withheld {
            None
        } else {
            let source = SelectQuery {
                where_clause: where_clause.clone(),
                search: search.clone(),
                ..SelectQuery::new(model)
            };
            compute_date_hierarchy(&state, model, &admin_cfg, date_sel, &list_query, source).await?
        };

    // Right-rail card per custom list filter: the filter title and
    // its clickable options, with the active value marked.
    let custom_filters_ctx: Vec<serde_json::Value> =
        crate::admin::list_filters::for_table(model.table)
            .map(|cf| {
                let active_value: Option<&str> = active_custom_filters
                    .iter()
                    .find(|(k, _)| *k == cf.parameter_name)
                    .map(|(_, v)| v.as_str());
                let cleared = list_query.without(&[cf.parameter_name]);
                let clear_url = cleared.url();
                let values: Vec<serde_json::Value> = cf
                    .lookups
                    .iter()
                    .map(|(value, label)| {
                        serde_json::json!({
                            "value": value,
                            "label": label,
                            "active": active_value == Some(*value),
                            "url": cleared.clone().with(cf.parameter_name, *value).url(),
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
    // `admin.actions`, which hides the picker. The trash list offers only
    // `restore_selected`: every other action skips soft-deleted rows.
    let actions_ctx: Vec<serde_json::Value> = admin_cfg
        .actions
        .iter()
        .filter(|name| !trashed || **name == "restore_selected")
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
        "can_add": state.can_add(model.table)
            && crate::admin::object_permissions::is_allowed(model.table, "add", &parts, None),
        "has_searchable": !search_columns.is_empty(),
        // An empty `search_help_text` hides the caption.
        "search_help_text": admin_cfg.search_help_text,
        // Action-bar position flags. Default: top on, bottom off.
        "actions_on_top": admin_cfg.actions_on_top,
        "actions_on_bottom": admin_cfg.actions_on_bottom,
        "q": q.unwrap_or_default(),
        "active_filters": active_filters_ctx,
        // The filter state minus `q`, as the search form's hidden inputs.
        "hidden_params": hidden_params,
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
        "trashed": trashed,
        "trash_toggle_url": model.soft_delete_column.map(|_| if trashed {
            list_query.without(&["trashed", "page"]).url()
        } else {
            list_query.without(&["page"]).with("trashed", "1").url()
        }),
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
/// Counts are within the list's filters and search, minus the facet's
/// own field filter. One ORM `GROUP BY` per
/// facet capped past `FACET_TRUNCATE`, plus one lookup for an FK facet's
/// display values. A capped facet also counts its values and reads the
/// active one.
///
/// Clicking the active value clears that filter. Clicking another
/// value sets it.
#[allow(clippy::too_many_arguments)]
async fn compute_facets(
    state: &AppState,
    parts: &axum::http::request::Parts,
    model: &'static crate::core::ModelSchema,
    field_filters: &[(&'static str, Filter)],
    other_filters: &[Filter],
    search: Option<&SearchClause>,
    admin_cfg: &crate::core::AdminConfig,
    active_field_filters: &[(String, String)],
    list_query: &ListQuery,
    show_all_facet: Option<&str>,
) -> Result<Vec<serde_json::Value>, AdminError> {
    if admin_cfg.list_filter.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(admin_cfg.list_filter.len());
    for filter_name in admin_cfg.list_filter {
        // A secret's facet would list its values.
        let Some(field) = model
            .field(filter_name)
            .filter(|f| !is_secret_field(admin_cfg, f.name))
        else {
            continue;
        };
        let lookup_keys = FieldLookup::keys(field.name);
        let lookup_keys: Vec<&str> = lookup_keys.iter().map(String::as_str).collect();
        let facet_filters: Vec<Filter> = field_filters
            .iter()
            .filter(|(name, _)| *name != field.name)
            .map(|(_, f)| f.clone())
            .chain(other_filters.iter().cloned())
            .collect();
        let source = SelectQuery {
            where_clause: WhereExpr::and_predicates(facet_filters.clone()),
            search: search.cloned(),
            ..SelectQuery::new(model)
        };
        let is_bool = field.ty == crate::core::FieldType::Bool;
        let show_all = show_all_facet == Some(field.name);
        let cap = if show_all {
            FACET_SHOW_ALL_CAP
        } else {
            FACET_TRUNCATE
        };
        // One past the cap tells whether there are more (#2344).
        let limit = Some(cap + 1);
        let mut facet_rows =
            fetch_facet_counts(&state.pool, source.clone(), field, is_bool, limit).await?;
        let is_active = |r: &FacetRow| {
            active_field_filters.contains(&FieldLookup::param(field.name, &r.key, &r.raw))
        };
        let mut total_values = facet_rows.len();
        if !show_all && total_values > FACET_TRUNCATE {
            total_values = fetch_facet_value_total(&state.pool, source, field).await?;
            // The active value may sit past the cut: read it on its own.
            // The field's own filters select at most one value.
            if !facet_rows.iter().any(is_active) {
                let own: Vec<Filter> = field_filters
                    .iter()
                    .filter(|(name, _)| *name == field.name)
                    .map(|(_, f)| f.clone())
                    .collect();
                if !own.is_empty() {
                    let active_source = SelectQuery {
                        where_clause: WhereExpr::and_predicates(
                            facet_filters.into_iter().chain(own).collect(),
                        ),
                        search: search.cloned(),
                        ..SelectQuery::new(model)
                    };
                    let extra =
                        fetch_facet_counts(&state.pool, active_source, field, is_bool, Some(1))
                            .await?;
                    // `?author=01` names a shown value in another spelling.
                    let extra: Vec<FacetRow> = extra
                        .into_iter()
                        .filter(|e| facet_rows.iter().all(|r| r.raw != e.raw))
                        .collect();
                    facet_rows.extend(extra);
                }
            }
        }
        // An FK facet shows the target's display value: "Dr. Maeve O'Hara (3)", not "1 (3)".
        let fk_target = field.relation.and_then(|rel| match rel {
            crate::core::Relation::Fk { to, on } | crate::core::Relation::O2O { to, on } => {
                let target = lookup_model(state, to)?;
                Some((target, target.field_by_column(on)?, target.display_field()?))
            }
        });
        let is_fk = fk_target.is_some();
        if let Some((target, on_field, display_field)) = fk_target {
            let keys: Vec<SqlValue> = facet_rows
                .iter()
                .map(|r| r.key.clone())
                .filter(|v| !matches!(v, SqlValue::Null))
                .collect();
            let names =
                fk_display_names(state, parts, target, on_field, display_field, keys).await?;
            for r in &mut facet_rows {
                r.display = names.get(&r.raw).cloned();
            }
            // Most used first, then by the shown name, as the old JOIN ordered.
            facet_rows.sort_by(|a, b| {
                b.count
                    .cmp(&a.count)
                    .then_with(|| a.display.cmp(&b.display))
            });
        }
        let mut values = Vec::with_capacity(facet_rows.len());
        for FacetRow {
            key,
            raw,
            display: display_text,
            count,
        } in &facet_rows
        {
            let raw = raw.clone();
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
            let param = FieldLookup::param(field.name, key, &raw);
            let is_active = active_field_filters.contains(&param);
            // Toggle URL: drop this filter when it is active, else
            // set it. The rest of the filter state is kept.
            let mut toggle = list_query.without(&lookup_keys);
            if !is_active {
                toggle = toggle.with(param.0, param.1);
            }
            let toggle_url = toggle.url();

            values.push(serde_json::json!({
                "raw": raw,
                "display": display,
                "count": count,
                "active": is_active,
                "toggle_url": toggle_url,
            }));
        }
        // Truncate to `cap` values. Active values always render, so
        // one never hides behind the cutoff. They count toward the budget.
        let mut more_count: usize = 0;
        if total_values > cap {
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
            let room = cap.saturating_sub(active_first.len());
            let kept_rest_len = rest.len().min(room);
            // "Show all" has no further page to link to.
            if !show_all {
                more_count = total_values - active_first.len() - kept_rest_len;
            }
            active_first.extend(rest.into_iter().take(room));
            values = active_first;
        }
        let show_all_url = if more_count > 0 {
            // Keep the current filters and add
            // `facet_show_all=<field>`, which swaps the truncated
            // list for the full one.
            Some(
                list_query
                    .without(&["facet_show_all"])
                    .with("facet_show_all", field.name)
                    .url(),
            )
        } else {
            None
        };
        // FK facets render as a `<select>`; this is the "All" option,
        // which removes the filter.
        let clear_url = if is_fk {
            Some(list_query.without(&lookup_keys).url())
        } else {
            None
        };
        out.push(serde_json::json!({
            "field": field.name,
            "is_fk": is_fk,
            "values": values,
            "more_count": more_count,
            "show_all_url": show_all_url,
            "clear_url": clear_url,
        }));
    }
    Ok(out)
}

/// The rows a "view" hook admits, as a filter SQL can apply.
enum ViewHookRows {
    /// No hook, or it denies nothing.
    All,
    /// The admitted rows, by PK.
    Only(Filter),
    /// Too many rows to scan: counts, facets and dates are withheld.
    TooMany,
}

/// Run the table's "view" hook over every row matching `filters` and
/// `search`, and fold the answer into one PK filter: the shorter of `IN`
/// admitted or `NOT IN` denied. The hook takes the whole row, so all
/// scalar columns are read.
async fn view_hook_rows(
    state: &AppState,
    parts: &axum::http::request::Parts,
    model: &'static crate::core::ModelSchema,
    filters: &[Filter],
    search: Option<&SearchClause>,
) -> Result<ViewHookRows, AdminError> {
    if !crate::admin::object_permissions::has_hook(model.table, "view") {
        return Ok(ViewHookRows::All);
    }
    let Some(pk_field) = model.primary_key() else {
        return Ok(ViewHookRows::TooMany);
    };
    let cap = state.view_hook_scan_cap();
    let fields: Vec<&'static FieldSchema> = model.scalar_fields().collect();
    let rows = crate::sql::select_rows_as_json(
        &state.pool,
        &SelectQuery {
            where_clause: WhereExpr::and_predicates(filters.to_vec()),
            search: search.cloned(),
            limit: Some(i64::try_from(cap).unwrap_or(i64::MAX).saturating_add(1)),
            ..SelectQuery::new(model)
        },
        &fields,
    )
    .await?;
    if rows.len() > cap {
        warn_scan_cap(model.table, cap);
        return Ok(ViewHookRows::TooMany);
    }
    let (mut admitted, mut denied) = (Vec::new(), Vec::new());
    for row in &rows {
        let pk = render::read_value_as_string_json(row, pk_field)
            .and_then(|raw| forms::parse_pk_string(pk_field, &raw).ok());
        let Some(pk) = pk else {
            return Ok(ViewHookRows::TooMany);
        };
        if crate::admin::object_permissions::is_allowed(model.table, "view", parts, Some(row)) {
            admitted.push(pk);
        } else {
            denied.push(pk);
        }
    }
    let filter = |op, value| Filter {
        column: pk_field.column,
        op,
        value,
    };
    Ok(if denied.is_empty() {
        ViewHookRows::All
    } else if admitted.is_empty() {
        // A PK is never NULL: no row.
        ViewHookRows::Only(filter(Op::IsNull, SqlValue::Bool(true)))
    } else if denied.len() <= admitted.len() {
        ViewHookRows::Only(filter(Op::NotIn, SqlValue::List(denied)))
    } else {
        ViewHookRows::Only(filter(Op::In, SqlValue::List(admitted)))
    })
}

/// Log once per table that the scan cap hid its counts.
fn warn_scan_cap(table: &'static str, cap: usize) {
    static WARNED: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    let mut warned = WARNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !warned.contains(&table) {
        warned.push(table);
        tracing::warn!(
            target: "rustango::admin",
            table,
            cap,
            "the \"view\" hook matches more rows than the scan cap: total, facets and \
             dates are hidden. Use register_admin_queryset! or raise view_hook_scan_cap"
        );
    }
}

/// One facet value: the stored key, its URL form, the FK display name and the count.
pub(crate) struct FacetRow {
    key: SqlValue,
    pub(crate) raw: String,
    display: Option<String>,
    pub(crate) count: i64,
}

/// How many distinct values (NULL included) `field` takes over `source`'s rows.
pub(crate) async fn fetch_facet_value_total(
    pool: &crate::sql::Pool,
    source: SelectQuery,
    field: &'static FieldSchema,
) -> Result<usize, AdminError> {
    use crate::core::{AggregateExpr, AggregateQuery};
    let agg = AggregateQuery::over_select(
        source,
        vec![
            (
                "facet_distinct".into(),
                AggregateExpr::CountDistinct(field.column),
            ),
            ("facet_rows".into(), AggregateExpr::Count(None)),
            (
                "facet_non_null".into(),
                AggregateExpr::Count(Some(field.column)),
            ),
        ],
    );
    let row = crate::sql::fetch_aggregate_dict(pool, &agg)
        .await?
        .into_iter()
        .next()
        .unwrap_or_default();
    // COUNT(DISTINCT) skips NULL, but GROUP BY gives it a value of its own.
    let has_null = sql_int(row.get("facet_rows")) > sql_int(row.get("facet_non_null"));
    Ok(usize::try_from(sql_int(row.get("facet_distinct"))).unwrap_or(0) + usize::from(has_null))
}

/// `SELECT <col>, COUNT(*) … GROUP BY <col>` over `source`'s rows, most used
/// first, at most `limit` values.
pub(crate) async fn fetch_facet_counts(
    pool: &crate::sql::Pool,
    source: SelectQuery,
    field: &'static FieldSchema,
    is_bool: bool,
    limit: Option<usize>,
) -> Result<Vec<FacetRow>, AdminError> {
    use crate::core::{AggregateExpr, AggregateQuery, Expr, OrderItem};
    let mut agg = AggregateQuery::over_select(
        source,
        vec![("facet_count".into(), AggregateExpr::Count(None))],
    );
    agg.group_by = vec![field.column];
    agg.order_by = vec![
        OrderItem::expr(Expr::Aggregate(Box::new(AggregateExpr::Count(None))), true),
        OrderItem::column(field.column, false),
    ];
    agg.limit = limit.and_then(|n| i64::try_from(n).ok());
    let rows = crate::sql::fetch_aggregate_dict(pool, &agg).await?;
    Ok(rows
        .into_iter()
        .map(|mut row| {
            let key = row.remove(field.column).unwrap_or(SqlValue::Null);
            FacetRow {
                raw: facet_value_string(&key, is_bool),
                key,
                display: None,
                count: sql_int(row.get("facet_count")),
            }
        })
        .collect())
}

/// FK cell names for `rows`: joined ones, plus the names of targets with a
/// "view" hook, read through it so a denied row keeps its raw key (#2267).
async fn fk_map_for_rows(
    state: &AppState,
    parts: &axum::http::request::Parts,
    model: &'static crate::core::ModelSchema,
    rows: &[serde_json::Value],
) -> Result<super::helpers::FkMap, AdminError> {
    let mut map = fk_map_from_joined_rows_json(state, model, rows);
    for t in fk_display_targets(state, model) {
        if !crate::admin::object_permissions::has_hook(t.target.table, "view") {
            continue;
        }
        let Some(on_field) = t.target.field_by_column(t.on) else {
            continue;
        };
        let mut raws: Vec<String> = rows
            .iter()
            .filter_map(|row| render::read_value_as_string_json(row, t.field))
            .collect();
        raws.sort();
        raws.dedup();
        let keys: Vec<SqlValue> =
            super::helpers::parse_keys(state, t.target, on_field, raws.iter().map(String::as_str))
                .await?
                .into_iter()
                .map(|(_, key)| key)
                .collect();
        let names =
            fk_display_names(state, parts, t.target, on_field, t.display_field, keys).await?;
        for (raw, name) in names {
            map.insert((t.to.to_owned(), raw), render::escape(&name));
        }
    }
    Ok(map)
}

/// `on value -> display value` for an FK facet's keys, inside the
/// target's queryset hooks (#2029) and in bind-capped chunks (#2049).
async fn fk_display_names(
    state: &AppState,
    parts: &axum::http::request::Parts,
    target: &'static crate::core::ModelSchema,
    on_field: &'static FieldSchema,
    display_field: &'static FieldSchema,
    keys: Vec<SqlValue>,
) -> Result<HashMap<String, String>, AdminError> {
    let scope = RowScope::of(target, parts);
    // A target row its "view" hook denies keeps its raw key (#2231);
    // the hook needs the whole row, not just the two columns.
    let view_hooked = crate::admin::object_permissions::has_hook(target.table, "view");
    let columns: Vec<&'static FieldSchema> = if view_hooked {
        target.scalar_fields().collect()
    } else {
        vec![on_field, display_field]
    };
    let mut names = HashMap::new();
    for chunk in keys.chunks(MAX_IN_KEYS) {
        let rows = crate::sql::select_rows_as_json(
            &state.pool,
            &scope.by_pk_in(target, on_field.column, chunk.to_vec()),
            &columns,
        )
        .await?;
        names.extend(rows.iter().filter_map(|row| {
            if !crate::admin::object_permissions::is_allowed(target.table, "view", parts, Some(row))
            {
                return None;
            }
            Some((
                render::read_value_as_string_json(row, on_field)?,
                render::read_value_as_string_json(row, display_field)?,
            ))
        }));
    }
    Ok(names)
}

/// The URL form of a facet key, the shape `parse_form_value` reads back.
/// SQLite and MySQL hand a bool back as `1`/`0` (#1730).
fn facet_value_string(v: &SqlValue, is_bool: bool) -> String {
    match v {
        SqlValue::Null => String::new(),
        SqlValue::I16(n) if is_bool => (*n != 0).to_string(),
        SqlValue::I32(n) if is_bool => (*n != 0).to_string(),
        SqlValue::I64(n) if is_bool => (*n != 0).to_string(),
        other => other.to_display_string(),
    }
}

/// An aggregate's integer result; `0` when missing.
fn sql_int(v: Option<&SqlValue>) -> i64 {
    match v {
        Some(SqlValue::I64(n)) => *n,
        Some(SqlValue::I32(n)) => i64::from(*n),
        Some(SqlValue::I16(n)) => i64::from(*n),
        _ => 0,
    }
}

// ============================================================== DATE HIERARCHY
//
// Breadcrumb and drill children for the list view's clickable
// year/month/day strip. Each child bucket is one filtered `COUNT(*)`
// over the list's rows, so the counts match the filtered list (#2004).

/// Years before the newest this many are not listed in the strip.
const MAX_YEAR_BUCKETS: i32 = 200;

async fn compute_date_hierarchy(
    state: &AppState,
    model: &'static crate::core::ModelSchema,
    admin_cfg: &crate::core::AdminConfig,
    sel: crate::admin::date_hierarchy::DateSelection,
    list_query: &ListQuery,
    source: SelectQuery,
) -> Result<Option<serde_json::Value>, AdminError> {
    use crate::admin::date_hierarchy::{range, DateSelection, DrillLevel};

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
    let undated = list_query.without(&["year", "month", "day"]);
    let url_for = |year: Option<i32>, month: Option<u32>, day: Option<u32>| -> String {
        let mut q = undated.clone();
        push_date(&mut q, year, month, day);
        q.url()
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

    let children: Vec<DateSelection> = match level {
        DrillLevel::Year => match year_span(state, &source, field).await? {
            Some((lo, hi)) => (lo.max(hi - MAX_YEAR_BUCKETS + 1)..=hi)
                .rev()
                .map(|y| DateSelection {
                    year: Some(y),
                    month: None,
                    day: None,
                })
                .collect(),
            None => Vec::new(),
        },
        DrillLevel::Month => (1..=12)
            .map(|m| DateSelection {
                month: Some(m),
                ..sel
            })
            .collect(),
        DrillLevel::Day => (1..=31)
            .map(|d| DateSelection {
                day: Some(d),
                ..sel
            })
            .collect(),
    };
    // A day the month lacks has no range, and an empty filter would count every row.
    let children: Vec<DateSelection> = children
        .into_iter()
        .filter(|c| range(*c).is_some())
        .collect();
    let bucket_of = |c: &DateSelection| match level {
        DrillLevel::Year => c.year.unwrap_or(0),
        DrillLevel::Month => c.month.map_or(0, |m| m as i32),
        DrillLevel::Day => c.day.map_or(0, |d| d as i32),
    };
    let counts = bucket_counts(state, model, field_name, source, &children).await?;
    let buckets_raw: Vec<(i32, i64)> = children
        .iter()
        .zip(counts)
        .filter(|(_, n)| *n > 0)
        .map(|(c, n)| (bucket_of(c), n))
        .collect();

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

/// The first and last year `field` holds among `source`'s rows.
async fn year_span(
    state: &AppState,
    source: &SelectQuery,
    field: &'static FieldSchema,
) -> Result<Option<(i32, i32)>, AdminError> {
    use crate::core::{AggregateExpr, AggregateQuery};
    let agg = AggregateQuery::over_select(
        source.clone(),
        vec![
            ("lo".into(), AggregateExpr::Min(field.column)),
            ("hi".into(), AggregateExpr::Max(field.column)),
        ],
    );
    let row = crate::sql::fetch_aggregate_dict(&state.pool, &agg)
        .await?
        .into_iter()
        .next();
    let year = |key: &str| row.as_ref().and_then(|r| year_of(r.get(key)?));
    Ok(year("lo").zip(year("hi")))
}

/// The year of a `MIN`/`MAX` over a date column. SQLite returns its stored text.
fn year_of(v: &SqlValue) -> Option<i32> {
    use chrono::Datelike as _;
    match v {
        SqlValue::Date(d) => Some(d.year()),
        SqlValue::DateTime(dt) => Some(dt.year()),
        SqlValue::String(s) => s.get(..4)?.parse().ok(),
        _ => None,
    }
}

/// One `COUNT(*) FILTER (WHERE <bucket range>)` per child, in one query.
async fn bucket_counts(
    state: &AppState,
    model: &'static crate::core::ModelSchema,
    field_name: &str,
    source: SelectQuery,
    children: &[crate::admin::date_hierarchy::DateSelection],
) -> Result<Vec<i64>, AdminError> {
    if children.is_empty() {
        return Ok(Vec::new());
    }
    let aggregates = children
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let range = crate::admin::date_hierarchy::predicates(model, field_name, *c);
            let count =
                crate::core::aggregates::count_all().filter(WhereExpr::and_predicates(range));
            (format!("b{i}").into(), count.into())
        })
        .collect();
    let agg = crate::core::AggregateQuery::over_select(source, aggregates);
    let row = crate::sql::fetch_aggregate_dict(&state.pool, &agg)
        .await?
        .into_iter()
        .next()
        .unwrap_or_default();
    Ok((0..children.len())
        .map(|i| sql_int(row.get(&format!("b{i}"))))
        .collect())
}

/// `admin.ordering`, else the model's `default_order`, then the PK as a
/// tiebreak so paging is stable (#1917).
fn list_order_by(
    model: &'static crate::core::ModelSchema,
    admin_cfg: &crate::core::AdminConfig,
) -> Vec<crate::core::OrderItem> {
    let spec = if admin_cfg.ordering.is_empty() {
        model.default_order
    } else {
        admin_cfg.ordering
    };
    let order = spec
        .iter()
        .filter_map(|(name, desc)| {
            model
                .field(name)
                .or_else(|| model.field_by_column(name))
                .map(|f| crate::core::OrderItem::column(f.column, *desc))
        })
        .collect();
    model.with_pk_tiebreak(order)
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
        // Encoded: a raw CR/LF in a string PK panics `Redirect::to`.
        let pk = crate::url_codec::url_encode(pk_value);
        format!("{admin_prefix}/{table}/{pk}")
    } else if form.contains_key("_addanother") {
        // `/new`, not `/add` — that is the route `urls.rs` mounts.
        format!("{admin_prefix}/{table}/{CREATE_SEGMENT}")
    } else {
        format!("{admin_prefix}/{table}")
    }
}

/// `year[&month[&day]]`, each level only under its parent.
fn push_date(q: &mut ListQuery, year: Option<i32>, month: Option<u32>, day: Option<u32>) {
    if let Some(y) = year {
        q.push("year", y.to_string());
        if let Some(m) = month {
            q.push("month", m.to_string());
            if let Some(d) = day {
                q.push("day", d.to_string());
            }
        }
    }
}

// ============================================================== AUTOCOMPLETE
//
// Backs `autocomplete_fields`. A widget on the parent form calls
// `GET <admin>/<target>/__autocomplete?q=…` and puts the matches in
// a `<datalist>`.
//
// The route lives on the *target* model, not on the field's owner, so
// one route serves every form that points at it. A target with no
// searchable columns returns an empty list rather than every row.

/// Pages autocomplete reads to fill `limit` when the "view" hook denies
/// rows; past them it returns fewer.
const AUTOCOMPLETE_MAX_PAGES: i64 = 5;

pub(crate) async fn autocomplete_view(
    parts: axum::http::request::Parts,
    Path(table): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    state: AppState,
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

    // With no search column, a query matches nothing (#2391).
    let search = (!q.is_empty()).then(|| SearchClause {
        columns: search_columns(model, &admin_cfg),
        query: q.clone(),
    });

    // The "view" hook runs after the read (#2231), so read up to
    // `AUTOCOMPLETE_MAX_PAGES` pages to fill `limit` past denied rows.
    let scalar_fields: Vec<&'static FieldSchema> = model.scalar_fields().collect();
    let mut results: Vec<serde_json::Value> = Vec::new();
    for page in 0..AUTOCOMPLETE_MAX_PAGES {
        let rows = crate::sql::select_rows_as_json(
            &state.pool,
            &SelectQuery {
                where_clause: RowScope::of(model, &parts).constrain(WhereExpr::And(Vec::new())),
                search: search.clone(),
                // The pk breaks ties, so pages do not overlap.
                order_by: vec![
                    crate::core::OrderItem::column(display_field.column, false),
                    crate::core::OrderItem::column(pk_field.column, false),
                ],
                limit: Some(limit),
                offset: Some(page * limit),
                ..SelectQuery::new(model)
            },
            &scalar_fields,
        )
        .await?;
        let last = (rows.len() as i64) < limit;
        results.extend(
            rows.into_iter()
                .filter(|row| {
                    crate::admin::object_permissions::is_allowed(
                        model.table,
                        "view",
                        &parts,
                        Some(row),
                    )
                })
                .filter_map(|row| {
                    let id = row.get(pk_field.column)?.clone();
                    let text = row
                        .get(display_field.column)
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_else(|| id.to_string());
                    Some(serde_json::json!({ "id": id, "text": text }))
                }),
        );
        if last || results.len() as i64 >= limit {
            break;
        }
    }
    results.truncate(limit as usize);

    Ok(axum::Json(serde_json::json!({ "results": results })))
}

// ============================================================== AUDIT LOG
//
// The audit route handlers and the emit helpers live in
// `super::audit`. The submit handlers below call
// `super::audit::admin_audit_*entry` and `emit_best_effort`.
// ============================================================== DETAIL

pub(crate) async fn detail_view(
    parts: axum::http::request::Parts,
    Path((table, pk_raw)): Path<(String, String)>,
    state: AppState,
) -> Result<Html<String>, AdminError> {
    let (model, pk_field, pk_value) = resolve_model_and_pk(&state, &table, &pk_raw).await?;

    let detail_fields: Vec<&'static FieldSchema> = model.scalar_fields().collect();
    // Single PK lookup, plus LEFT JOINs that carry the FK display
    // names.
    let row = crate::sql::select_one_row_as_json(
        &state.pool,
        &SelectQuery {
            joins: build_fk_joins(&state, model, &parts),
            ..RowScope::of(model, &parts).by_pk(model, pk_field.column, pk_value.clone())
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

    let fk_map = fk_map_for_rows(&state, &parts, model, std::slice::from_ref(&row)).await?;

    let detail_cfg = admin_config_or_default(model);
    let mut cells_ctx: Vec<serde_json::Value> = model
        .scalar_fields()
        .map(|f| {
            let value = if is_secret_field(&detail_cfg, f.name) {
                render_secret_cell(&row, f)
            } else {
                render_cell_json(&row, f, &fk_map, &state.config.admin_prefix)
            };
            serde_json::json!({ "label": f.display_label(), "value": value })
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
    let gfk_cts = gfk_ct_map(
        &state,
        std::slice::from_ref(&row),
        model.generic_relations.iter(),
    )
    .await;
    for gfk in model.generic_relations {
        cells_ctx.push(serde_json::json!({
            "label": gfk.name,
            "value": render_gfk_cell(&row, gfk, &gfk_cts, &state.config.admin_prefix),
        }));
    }

    // Inline panels: for every `register_admin_inline!` on this
    // parent table, fetch the child rows and render a panel.
    // Generic (ContentType-keyed) inlines come after the FK ones, in
    // registration order. Best-effort: a fetch error, such as a
    // child table missing on this tenant, drops the panels to empty
    // instead of failing the page.
    // Each panel lists its first rows and links the child list for the rest.
    let cap = Some(super::inlines::DETAIL_ROWS);
    let mut inline_panels = super::inlines::render_for_parent_in(
        &state.pool,
        model,
        pk_value.clone(),
        Some(&parts),
        cap,
    )
    .await
    .unwrap_or_default();
    let generic_panels = super::inlines::render_generic_for_parent_in(
        &state.pool,
        model,
        pk_value,
        Some(&parts),
        cap,
    )
    .await
    .unwrap_or_default();
    inline_panels.extend(generic_panels);
    inline_panels.retain(|(p, _)| lookup_model(&state, &p.child_table).is_some());
    let inline_panels_ctx: Vec<serde_json::Value> = inline_panels
        .into_iter()
        .map(|(p, more)| {
            let mut v = serde_json::to_value(p).unwrap_or(serde_json::Value::Null);
            if let Some(obj) = v.as_object_mut() {
                obj.insert("more_rows_filter".to_owned(), more.into());
            }
            v
        })
        .collect();

    // Audit-trail panel for this row, only for users who may read the
    // log. Best-effort: a missing audit table renders no panel. The
    // newest entries only; the full history is one link away.
    let audit_entries = match state.audit_reader() {
        Some(reader) => {
            reader
                .recent_for_entity(model.table, &pk_raw, super::audit::DETAIL_ROWS)
                .await
        }
        None => Ok(Vec::new()),
    };
    let audit_entries_ctx: Vec<serde_json::Value> = match audit_entries {
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
        // Links carry the key as one encoded segment or query value.
        "pk_path": crate::url_codec::url_encode(&pk_raw),
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
    state: AppState,
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

/// The create form again after a refused save, with the ContentType picker.
async fn create_form_again(
    state: &AppState,
    model: &'static crate::core::ModelSchema,
    form: &HashMap<String, String>,
    msg: &str,
) -> Response {
    let gfk_cts = preload_gfk_cts(state, model).await;
    Html(super::helpers::render_form_with_inlines_and_picker(
        state,
        model,
        Some(form),
        false,
        Some(msg),
        Vec::new(),
        &gfk_cts,
    ))
    .into_response()
}

pub(crate) async fn create_submit(
    parts: axum::http::request::Parts,
    Path(table): Path<String>,
    state: AppState,
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
    // A refused save shows the form again with what was typed.
    let (state_ref, form_ref) = (&state, &form);
    let refused =
        |msg: String| async move { create_form_again(state_ref, model, form_ref, &msg).await };
    // `readonly_fields` are display-only, so they are not form input.
    // An auto PK never is; a `default_uuid_v7` one is stamped (#1725).
    let admin_cfg = admin_config_or_default(model);
    // Locked fields are left out, so the column default applies.
    let locked = state.locked_fields(model, AdminWrite::Add);
    let mut skip: Vec<&str> = locked.all().collect();
    skip.extend(FormLayout::of(model, &admin_cfg, false).unrendered(model));
    // A defaulted field the form does not send, or sends blank where blank
    // is no value, is left to its column default; else "" means "" or NULL.
    skip.extend(
        model
            .scalar_fields()
            .filter(|f| crate::forms::blank_takes_default(f, form.get(f.name).map(String::as_str)))
            .map(|f| f.name),
    );
    let mut collected = match forms::collect_insert_values(model, &form, &skip) {
        Ok(v) => v,
        Err(e) => {
            // Re-render the form with the error instead of a 4xx.
            return Ok(refused(e.to_string()).await);
        }
    };
    stamp_readonly_timestamps(model, &admin_cfg, &mut collected);
    super::object_permissions::default_locked_flags(model, &locked.superuser_only, &mut collected);
    if let Err(e) = super::derived_fields::apply(model.table, &mut collected, None).await {
        return Ok(refused(e.as_str().to_owned()).await);
    }
    // A tenant's host, path prefix and port, claimed as the console does.
    #[cfg(feature = "tenancy")]
    let claims = match crate::tenancy::org_host::AdminClaims::of(model.table, &mut collected, None)
    {
        Ok(c) => c,
        Err(e) => {
            return Ok(refused(e.as_str().to_owned()).await);
        }
    };
    let audit_form = super::audit::audit_form(model, &admin_cfg, &form, &collected);
    let (columns, values): (Vec<&'static str>, Vec<SqlValue>) = collected.into_iter().unzip();

    let query = InsertQuery {
        model,
        columns,
        values,
        returning: vec![pk_field.column],
        on_conflict: None,
        generated: vec![],
    };
    // A database-assigned key is not known yet, so `pk` is empty then.
    let typed_pk = query
        .columns
        .iter()
        .position(|c| *c == pk_field.column)
        .map(|i| query.values[i].to_display_string())
        .unwrap_or_default();
    crate::signals::admin::send_admin_pre_save(crate::signals::admin::AdminSaveContext {
        table: model.table,
        pk: typed_pk,
        change: false,
        tenant: state.config.tenant.clone(),
    })
    .await;
    // An `audit(...)` model's entry commits with the INSERT, as on edit (#2101).
    let emit = crate::audit::DiffEmit::for_model(model);
    // MySQL breaks two claims of one free value with a deadlock: retry once.
    #[cfg(feature = "tenancy")]
    let mut retry = claims.retries();
    let (mut tx, pk, deferred) = loop {
        let mut tx = match crate::audit::PoolBoundTx::begin_write(&state.pool).await {
            Ok(tx) => tx,
            Err(e) => return Ok(refused(write_error(model, &e)).await),
        };
        let written = crate::audit::insert_one_with_entry_tx(
            &mut tx,
            &query,
            pk_field,
            |pk| {
                let pk = pk.to_display_string();
                super::audit::admin_audit_entry(
                    model,
                    &pk,
                    crate::audit::AuditOp::Create,
                    &audit_form,
                )
            },
            emit,
        )
        .await;
        let (pk, deferred) = match written {
            Ok(w) => w,
            Err(e) => {
                rollback_quietly(tx, model.table).await;
                #[cfg(feature = "tenancy")]
                if std::mem::take(&mut retry) && e.is_deadlock() {
                    continue;
                }
                return Ok(refused(write_error(model, &e)).await);
            }
        };
        #[cfg(feature = "tenancy")]
        if let Err(e) = claims.after_write(&mut tx, &pk).await {
            rollback_quietly(tx, model.table).await;
            if std::mem::take(&mut retry) && e.is_deadlock() {
                continue;
            }
            return Ok(refused(claim_refusal(model, e)).await);
        }
        break (tx, pk, deferred);
    };
    match admits_written(
        &mut tx,
        &parts,
        model,
        pk_field,
        pk.clone(),
        Written::Created,
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => {
            rollback_quietly(tx, model.table).await;
            return Err(AdminError::Forbidden {
                table: model.table.to_owned(),
                action: "add",
            });
        }
        Err(e) => {
            rollback_quietly(tx, model.table).await;
            return Ok(refused(write_error(model, &e)).await);
        }
    }
    if let Err(e) = tx.commit().await {
        return Ok(refused(write_error(model, &e.into())).await);
    }
    if let Some(entry) = deferred {
        super::audit::emit_best_effort(&state, &entry).await;
    }
    let pk_value = pk.to_display_string();
    // `save_model` hook. It fires only on admin writes, not on every
    // ORM insert, so it is a seam for admin-only side effects.
    crate::signals::admin::send_admin_post_save(crate::signals::admin::AdminSaveContext {
        table: model.table,
        pk: pk_value.clone(),
        change: false,
        tenant: state.config.tenant.clone(),
    })
    .await;
    let target = post_save_redirect(&state.config.admin_prefix, model.table, &pk_value, &form);
    Ok(Redirect::to(&target).into_response())
}

/// The form error for a routing claim another tenant holds.
#[cfg(feature = "tenancy")]
fn claim_refusal(
    model: &'static crate::core::ModelSchema,
    e: crate::tenancy::org_host::ClaimError,
) -> String {
    use crate::tenancy::org_host::ClaimError;
    match e {
        ClaimError::Taken(key) => format!("{key} is already used by another tenant."),
        ClaimError::Driver(e) => write_error(model, &e),
    }
}

/// The form error for a failed write. Never the driver's text: it holds
/// table, constraint and SQL (#2345); that goes to the log under an id.
fn write_error(model: &'static crate::core::ModelSchema, e: &crate::sql::ExecError) -> String {
    use crate::sql::Refusal;
    if super::errors::missing_table(e).is_some_and(|t| t == crate::audit::AUDIT_TABLE) {
        return "audit table missing — run `manage migrate`".to_owned();
    }
    // Checked before any SQL ran (max_length, min/max, validators): no driver text.
    if let crate::sql::ExecError::Query(q) = e {
        return q.to_string();
    }
    let refusal = e.refusal();
    let id = super::errors::log_with_id("admin write refused", e, refusal.is_some());
    let msg = match refusal {
        Some(Refusal::Unique) => {
            let unique: Vec<&str> = model
                .scalar_fields()
                .filter(|f| f.unique && !f.primary_key)
                .map(|f| f.name)
                .collect();
            // A typed PK or a composite unique may be what clashed instead.
            let other_keys = model.primary_key().is_none_or(|pk| !pk.auto)
                || model.indexes.iter().any(|i| i.unique);
            if unique.is_empty() || other_keys {
                format!("A {} with these values already exists.", model.name)
            } else {
                format!(
                    "A {} with this {} already exists.",
                    model.name,
                    unique.join(" or ")
                )
            }
        }
        Some(r) => r.message().to_owned(),
        None => "The change could not be saved.".to_owned(),
    };
    format!("{msg} (error id {id})")
}

/// Fill read-only, NOT NULL timestamps with no default: the form never
/// sends them and the INSERT would fail (#1763).
fn stamp_readonly_timestamps(
    model: &'static crate::core::ModelSchema,
    admin_cfg: &crate::core::AdminConfig,
    values: &mut Vec<(&'static str, SqlValue)>,
) {
    let now = chrono::Utc::now();
    for f in model.scalar_fields() {
        if f.ty == crate::core::FieldType::DateTime
            && !f.nullable
            && !f.auto
            && f.default.is_none()
            && admin_cfg.readonly_fields.contains(&f.name)
            && !values.iter().any(|(c, _)| *c == f.column)
        {
            values.push((f.column, SqlValue::DateTime(now)));
        }
    }
}

/// Whether `shown`, the prefill of `f`, may parse back to another value
/// than the stored one.
fn prefill_is_lossy(f: &FieldSchema, shown: &str) -> bool {
    use crate::core::FieldType as T;
    match f.ty {
        // A time shows only milliseconds.
        T::DateTime
        | T::Time
        | T::Array(_)
        | T::Range(_)
        | T::HStore
        | T::Vector(_)
        | T::Geometry(_) => true,
        // `{}` and JSON null both show empty.
        T::Json => shown.is_empty(),
        // So do a nullable `""` and NULL; the form reads both back as NULL.
        T::String if f.nullable && shown.is_empty() => true,
        _ => f
            .choices
            .is_some_and(|c| !shown.is_empty() && !c.iter().any(|(v, _)| *v == shown)),
    }
}

/// Whether this edit sets the signed-in user's own account inactive; the
/// next request would then be refused, as the operator console warns.
fn deactivates_self(
    parts: &axum::http::request::Parts,
    model: &'static crate::core::ModelSchema,
    before: Option<&serde_json::Value>,
    values: &[(&'static str, SqlValue)],
) -> bool {
    use crate::core::Model as _;
    let user_tables = [
        super::user::AdminUser::SCHEMA.table,
        #[cfg(feature = "tenancy")]
        crate::tenancy::auth::User::SCHEMA.table,
    ];
    let (Some(session), Some(row)) = (super::session::from_extensions(&parts.extensions), before)
    else {
        return false;
    };
    let truthy = |v: &serde_json::Value| v.as_bool() == Some(true) || v.as_i64() == Some(1);
    session.impersonated_by.is_none()
        && user_tables.contains(&model.table)
        && row.get("id").and_then(serde_json::Value::as_i64) == Some(session.user_id)
        && row.get("username").and_then(serde_json::Value::as_str)
            == Some(session.username.as_str())
        && row.get("active").is_some_and(truthy)
        && values
            .iter()
            .any(|(c, v)| *c == "active" && *v == SqlValue::Bool(false))
}

/// On update: a lossy prefill the form echoed back as it was shown is not
/// rewritten; other fields are, so a stale form still wins.
/// `auto_now` columns are restamped.
fn keep_unchanged(
    model: &'static crate::core::ModelSchema,
    form: &HashMap<String, String>,
    before: Option<&serde_json::Value>,
    values: &mut Vec<(&'static str, SqlValue)>,
) {
    keep_echoed_lossy(model, form, before, values);
    crate::forms::stamp_auto_now(model, values);
}

/// Drop each lossy prefill (a time shown to the millisecond, say) that
/// `form` sent back as it was shown, so the stored value stays.
pub(super) fn keep_echoed_lossy(
    model: &'static crate::core::ModelSchema,
    form: &HashMap<String, String>,
    before: Option<&serde_json::Value>,
    values: &mut Vec<(&'static str, SqlValue)>,
) {
    for f in model.scalar_fields() {
        let submitted = form.get(f.name).map(String::as_str);
        let unchanged = before.zip(submitted).is_some_and(|(row, sent)| {
            let shown = render::render_value_for_input_json(row, f);
            if !prefill_is_lossy(f, &shown) {
                return false;
            }
            // A textarea submits CRLF line breaks.
            let sent = sent.replace("\r\n", "\n");
            sent == shown
                // Browsers may drop a zero seconds part.
                || (matches!(f.ty, crate::core::FieldType::DateTime | crate::core::FieldType::Time)
                    && shown.strip_suffix(":00") == Some(sent.as_str()))
        });
        if unchanged {
            values.retain(|(c, _)| *c != f.column);
        }
    }
}

// ============================================================== EDIT

pub(crate) async fn edit_form(
    parts: axum::http::request::Parts,
    Path((table, pk_raw)): Path<(String, String)>,
    state: AppState,
) -> Result<Response, AdminError> {
    // No `{table}.change`: the save would be refused, so show the read-only detail.
    if state.is_read_only(resolve_model(&state, &table)?.table) {
        let pk = crate::url_codec::url_encode(&pk_raw);
        let detail = format!("{}/{table}/{pk}", state.config.admin_prefix);
        return Ok(Redirect::to(&detail).into_response());
    }
    let (model, pk_field, pk_value) = resolve_model_and_pk(&state, &table, &pk_raw).await?;

    let edit_fields: Vec<&'static FieldSchema> = model.scalar_fields().collect();
    let row = crate::sql::select_one_row_as_json(
        &state.pool,
        &RowScope::of(model, &parts).by_pk(model, pk_field.column, pk_value.clone()),
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
    let inline_panels = edit_inline_panels(&state, &parts, model, pk_value, None).await;
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
    ))
    .into_response())
}

/// Editable inline panels under the parent form, generic ones last.
/// `submitted` re-shows a refused POST's rows. Best-effort: a child-table
/// fetch failure drops the inlines instead of breaking the edit page.
async fn edit_inline_panels(
    state: &AppState,
    parts: &axum::http::request::Parts,
    model: &'static crate::core::ModelSchema,
    pk_value: SqlValue,
    submitted: Option<&HashMap<String, String>>,
) -> Vec<super::inlines::InlineFormPanel> {
    let mut inline_panels = super::inlines::render_form_for_parent_in(
        &state.pool,
        model,
        pk_value.clone(),
        Some(parts),
        submitted,
    )
    .await
    .unwrap_or_default();
    let generic_panels = super::inlines::render_form_generic_for_parent_in(
        &state.pool,
        model,
        pk_value,
        Some(parts),
        submitted,
    )
    .await
    .unwrap_or_default();
    inline_panels.extend(generic_panels);
    // Only children this user may edit get a FormSet.
    inline_panels.retain(|p| {
        lookup_model(state, &p.child_table).is_some() && !state.is_read_only(&p.child_table)
    });
    inline_panels
}

/// The edit form again after a refused save: what was typed, inline
/// rows included, with the error and the ContentType picker.
async fn edit_form_again(
    state: &AppState,
    parts: &axum::http::request::Parts,
    model: &'static crate::core::ModelSchema,
    (pk_value, pk_raw): (&SqlValue, &str),
    form: &HashMap<String, String>,
    msg: &str,
) -> Response {
    let inline_panels = edit_inline_panels(state, parts, model, pk_value.clone(), Some(form)).await;
    let gfk_cts = preload_gfk_cts(state, model).await;
    // The form action is built from the PK, which a crafted POST may omit.
    let mut prefill = form.clone();
    if let Some(pk) = model.primary_key() {
        prefill.insert(pk.name.to_owned(), pk_raw.to_owned());
    }
    Html(super::helpers::render_form_with_inlines_and_picker(
        state,
        model,
        Some(&prefill),
        true,
        Some(msg),
        inline_panels,
        &gfk_cts,
    ))
    .into_response()
}

pub(crate) async fn update_submit(
    parts: axum::http::request::Parts,
    Path((table, pk_raw)): Path<(String, String)>,
    state: AppState,
    Form(form): Form<HashMap<String, String>>,
) -> Result<Response, AdminError> {
    let model = resolve_model(&state, &table)?;
    if state.is_read_only(model.table) {
        return Err(AdminError::ReadOnly {
            table: model.table.to_owned(),
        });
    }
    let pk_field = primary_key_or_internal(model)?;
    let pk_value = parse_pk(&state, model, pk_field, &pk_raw).await?;
    // A refused save shows the form again with what was typed.
    let again_pk = (pk_value.clone(), pk_raw.clone());
    let (state_ref, parts_ref, form_ref, again_pk_ref) = (&state, &parts, &form, &again_pk);
    let refused = |msg: String| async move {
        edit_form_again(
            state_ref,
            parts_ref,
            model,
            (&again_pk_ref.0, &again_pk_ref.1),
            form_ref,
            &msg,
        )
        .await
    };

    // `has_change_permission(request, obj)`. Fetch the row first so
    // the hook sees the state before the update. A select error is
    // surfaced as-is, so a permission failure is never hidden
    // behind a 500.
    let pre_update_fields: Vec<&'static FieldSchema> = model.scalar_fields().collect();
    let pre_update_row = crate::sql::select_one_row_as_json(
        &state.pool,
        &RowScope::of(model, &parts).by_pk(model, pk_field.column, pk_value.clone()),
        &pre_update_fields,
    )
    .await?;
    // Missing, or outside the queryset hooks' scope.
    if pre_update_row.is_none() {
        return Err(AdminError::RowNotFound { table, pk: pk_raw });
    }
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
    skip.extend(state.locked_fields(model, AdminWrite::Change).all());
    // Fields the edit form hides are left as they are.
    skip.extend(FormLayout::of(model, &admin_cfg, true).unrendered(model));
    // An empty secret keeps the stored one.
    skip.extend(
        model
            .scalar_fields()
            .filter(|f| is_secret_field(&admin_cfg, f.name))
            .filter(|f| form.get(f.name).is_none_or(String::is_empty))
            .map(|f| f.name),
    );
    let mut collected = match forms::collect_values(model, &form, &skip) {
        Ok(v) => v,
        Err(e) => {
            return Ok(refused(e.to_string()).await);
        }
    };
    keep_unchanged(model, &form, pre_update_row.as_ref(), &mut collected);
    if deactivates_self(&parts, model, pre_update_row.as_ref(), &collected) {
        let msg = "You cannot deactivate yourself: your next request would be refused. \
                   Ask another admin to do it.";
        return Ok(refused(msg.to_owned()).await);
    }
    if let Err(e) =
        super::derived_fields::apply(model.table, &mut collected, pre_update_row.as_ref()).await
    {
        return Ok(refused(e.as_str().to_owned()).await);
    }
    #[cfg(feature = "tenancy")]
    let claims = match crate::tenancy::org_host::AdminClaims::of(
        model.table,
        &mut collected,
        pre_update_row.as_ref(),
    ) {
        Ok(c) => c,
        Err(e) => {
            return Ok(refused(e.as_str().to_owned()).await);
        }
    };
    let audit_form = super::audit::audit_form(model, &admin_cfg, &form, &collected);
    let assignments: Vec<Assignment> = collected
        .into_iter()
        .map(|(column, value)| Assignment {
            column,
            value: value.into(),
        })
        .collect();

    // Gate every inline row before anything is written, the parent included.
    let inline_plan = match super::inlines::plan_post(&state, &parts, model, &pk_value, &form).await
    {
        Ok(plan) => plan,
        Err(super::inlines::InlinePlanError::Admin(e)) => return Err(e),
        Err(super::inlines::InlinePlanError::Rejected(msg)) => {
            return Ok(refused(msg).await);
        }
        Err(super::inlines::InlinePlanError::BadFormset(e)) => {
            return Ok(refused(e.to_string()).await);
        }
    };

    // The diff's "before" is re-read under lock in the UPDATE's tx.
    let before_select = RowScope::of(model, &parts).by_pk(model, pk_field.column, pk_value.clone());
    let query = UpdateQuery {
        model,
        set: assignments,
        where_clause: WhereExpr::Predicate(Filter {
            column: pk_field.column,
            op: Op::Eq,
            value: pk_value.clone(),
        }),
    };
    crate::signals::admin::send_admin_pre_save(crate::signals::admin::AdminSaveContext {
        table: model.table,
        pk: pk_raw.clone(),
        change: true,
        tenant: state.config.tenant.clone(),
    })
    .await;
    // One signal per inline row too, as for bulk actions.
    for row in inline_plan.planned_rows() {
        row.send(true, &state).await;
    }
    // "Before" is the locked row, "after" the form. The per-request
    // `with_source(User { id })` gives a "who changed what" trail. An
    // `audit(...)` model's entry commits with the UPDATE (#2060); others
    // keep the best-effort emit after it.
    let emit = crate::audit::DiffEmit::for_model(model);
    // Parent and inline writes share one transaction: a refused inline
    // row rolls the parent back too (#2339).
    // An extra host is checked after its UPDATE: retried once, as on create.
    #[cfg(feature = "tenancy")]
    let mut retry = claims.retries();
    // The operator lock can deadlock with another admin write: retried once.
    #[cfg(feature = "tenancy")]
    let mut lock_retry = true;
    let (mut tx, deferred) = loop {
        #[cfg(feature = "tenancy")]
        let mut tx = match claims.begin(&state.pool, &pk_value).await {
            Ok(tx) => crate::audit::PoolBoundTx::opened_on(&state.pool, tx),
            Err(e) => return Ok(refused(claim_refusal(model, e)).await),
        };
        #[cfg(not(feature = "tenancy"))]
        let mut tx = match crate::audit::PoolBoundTx::begin_write(&state.pool).await {
            Ok(tx) => tx,
            Err(e) => return Ok(refused(write_error(model, &e)).await),
        };
        #[cfg(feature = "tenancy")]
        let operators = match crate::tenancy::operators::AdminOperatorWrite::lock(
            &mut tx,
            model.table,
            pre_update_row.as_slice(),
        )
        .await
        {
            Ok(w) => w,
            Err(e) => {
                rollback_quietly(tx, model.table).await;
                if e.is_deadlock() && std::mem::take(&mut lock_retry) {
                    continue;
                }
                return Err(e.into());
            }
        };
        let written = crate::audit::update_one_with_row_diff_tx(
            &mut tx,
            &query,
            before_select.clone(),
            &pre_update_fields,
            |row| super::audit::admin_audit_diff_entry(model, &pk_raw, row, &audit_form),
            emit,
        )
        .await;
        let deferred = match written {
            Ok(crate::audit::RowDiffWrite::Written { deferred }) => deferred,
            Ok(crate::audit::RowDiffWrite::Gone) => {
                rollback_quietly(tx, model.table).await;
                return Err(AdminError::RowNotFound { table, pk: pk_raw });
            }
            Err(e) => {
                rollback_quietly(tx, model.table).await;
                #[cfg(feature = "tenancy")]
                if std::mem::take(&mut retry) && e.is_deadlock() {
                    continue;
                }
                return Ok(refused(write_error(model, &e)).await);
            }
        };
        #[cfg(feature = "tenancy")]
        if let Err(e) = claims.after_write(&mut tx, &pk_value).await {
            rollback_quietly(tx, model.table).await;
            if std::mem::take(&mut retry) && e.is_deadlock() {
                continue;
            }
            return Ok(refused(claim_refusal(model, e)).await);
        }
        #[cfg(feature = "tenancy")]
        match operators.check(&mut tx).await {
            Ok(()) => {}
            Err(crate::tenancy::operators::OperatorError::Driver(e)) => {
                rollback_quietly(tx, model.table).await;
                if e.is_deadlock() && std::mem::take(&mut lock_retry) {
                    continue;
                }
                return Err(e.into());
            }
            Err(e) => {
                rollback_quietly(tx, model.table).await;
                return Ok(refused(e.to_string()).await);
            }
        }
        break (tx, deferred);
    };
    match admits_written(&mut tx, &parts, model, pk_field, pk_value, Written::Edited).await {
        Ok(true) => {}
        Ok(false) => {
            rollback_quietly(tx, model.table).await;
            return Err(AdminError::Forbidden {
                table: model.table.to_owned(),
                action: "change",
            });
        }
        Err(e) => {
            rollback_quietly(tx, model.table).await;
            return Ok(refused(write_error(model, &e)).await);
        }
    }
    let applied = match super::inlines::apply_plan_tx(&mut tx, &parts, inline_plan).await {
        Ok(applied) => applied,
        Err(e) => {
            use super::inlines::InlineApplyError as E;
            rollback_quietly(tx, model.table).await;
            let why = match e {
                E::Write { child, error } => write_error(child, &error),
                E::MaxNum { child } => format!("{} allows no more rows here.", child.name),
                #[cfg(feature = "tenancy")]
                E::Claim { child, error } => claim_refusal(child, error),
                E::Forbidden { child, action } => {
                    return Err(AdminError::Forbidden {
                        table: child.table.to_owned(),
                        action,
                    })
                }
            };
            return Ok(refused(format!("Nothing was saved: {why}")).await);
        }
    };
    if let Err(e) = tx.commit().await {
        return Ok(refused(write_error(model, &e.into())).await);
    }
    if let Some(entry) = deferred {
        super::audit::emit_best_effort(&state, &entry).await;
    }
    super::audit::emit_many_best_effort(&state, &applied.audit, "inline").await;
    // The `post_save` hook fires after the commit and the audit
    // emit. `change = true` marks this as an edit, not a create.
    crate::signals::admin::send_admin_post_save(crate::signals::admin::AdminSaveContext {
        table: model.table,
        pk: pk_raw.clone(),
        change: true,
        tenant: state.config.tenant.clone(),
    })
    .await;
    for row in applied.signals {
        row.send(false, &state).await;
    }

    let target = post_save_redirect(&state.config.admin_prefix, model.table, &pk_raw, &form);
    Ok(Redirect::to(&target).into_response())
}

/// How a written row was made, which picks its re-check.
#[derive(Clone, Copy)]
pub(super) enum Written {
    /// Scope and the `add` hook: no row existed before.
    Created,
    /// Scope only: `change` already judged the row before the edit.
    Edited,
}

impl Written {
    /// The action a refusal reports.
    pub(super) fn action(self) -> &'static str {
        match self {
            Self::Created => "add",
            Self::Edited => "change",
        }
    }

    /// The hook to run on the written row, if any.
    fn hook(self) -> Option<&'static str> {
        match self {
            Self::Created => Some("add"),
            Self::Edited => None,
        }
    }
}

/// Whether the row just written in `tx` is still in the request's scope
/// (and, for a create, passes the `add` hook); a form must not move a row outside it.
pub(super) async fn admits_written(
    tx: &mut crate::sql::PoolTx<'_>,
    parts: &axum::http::request::Parts,
    model: &'static crate::core::ModelSchema,
    pk_field: &'static FieldSchema,
    pk: SqlValue,
    written: Written,
) -> Result<bool, crate::sql::ExecError> {
    let fields: Vec<&'static FieldSchema> = model.scalar_fields().collect();
    let select = RowScope::of(model, parts).by_pk(model, pk_field.column, pk);
    let row = crate::sql::select_one_row_as_json_tx(tx, &select, &fields).await?;
    Ok(row.is_some_and(|row| {
        written.hook().is_none_or(|action| {
            crate::admin::object_permissions::is_allowed(model.table, action, parts, Some(&row))
        })
    }))
}

/// A failed ROLLBACK must not replace the refusal with a 500; dropping
/// the connection discards the tx anyway.
async fn rollback_quietly(tx: crate::audit::PoolBoundTx<'_>, table: &str) {
    if let Err(e) = tx.rollback().await {
        tracing::warn!(target: "rustango::admin", error = %e, table, "admin edit rollback failed");
    }
}

// ============================================================== DELETE

pub(crate) async fn delete_submit(
    parts: axum::http::request::Parts,
    Path((table, pk_raw)): Path<(String, String)>,
    state: AppState,
) -> Result<Response, AdminError> {
    let model = resolve_model(&state, &table)?;
    if !state.can_delete(model.table) {
        return Err(AdminError::ReadOnly {
            table: model.table.to_owned(),
        });
    }
    let pk_field = primary_key_or_internal(model)?;
    let pk_value = parse_pk(&state, model, pk_field, &pk_raw).await?;

    // Read the row before deleting it, so the audit entry records
    // what was removed. A row missing or outside the queryset hooks'
    // scope is a 404.
    let delete_fields: Vec<&'static FieldSchema> = model.scalar_fields().collect();
    let before_row = crate::sql::select_one_row_as_json(
        &state.pool,
        &RowScope::of(model, &parts).by_pk(model, pk_field.column, pk_value.clone()),
        &delete_fields,
    )
    .await?;
    let Some(before_row) = before_row else {
        return Err(AdminError::RowNotFound { table, pk: pk_raw });
    };

    // `has_delete_permission(request, obj)` hook. It **must** run
    // before any soft-delete UPDATE or hard DELETE.
    if !crate::admin::object_permissions::is_allowed(
        model.table,
        "delete",
        &parts,
        Some(&before_row),
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

    crate::signals::admin::send_admin_pre_delete(crate::signals::admin::AdminDeleteContext {
        table: model.table,
        pk: pk_raw.clone(),
        tenant: state.config.tenant.clone(),
    })
    .await;
    let list_url = format!("{}/{}", state.config.admin_prefix, model.table);
    let entry =
        super::audit::admin_row_snapshot_entry(model, pk_raw.clone(), audit_op, &before_row, None);
    // An `audit(...)` model's entry commits with the write, as on edit (#2390).
    let emit = crate::audit::DiffEmit::for_model(model);
    let mut tx = crate::audit::PoolBoundTx::begin_write(&state.pool).await?;
    #[cfg(feature = "tenancy")]
    let operators = match crate::tenancy::operators::AdminOperatorWrite::lock(
        &mut tx,
        model.table,
        std::slice::from_ref(&before_row),
    )
    .await
    {
        Ok(w) => w,
        Err(e) => {
            rollback_quietly(tx, model.table).await;
            return Err(e.into());
        }
    };
    let written = if let Some(col) = model.soft_delete_column {
        let now = Some(chrono::Utc::now());
        let stamp = crate::soft_delete::__mark_query(model, col, pk_field.column, pk_value, now);
        crate::sql::update_tx(&mut tx, &stamp).await
    } else {
        let query = DeleteQuery::by_pk(model, pk_field.column, pk_value);
        crate::sql::delete_tx(&mut tx, &query).await
    };
    let affected = match written {
        Ok(n) => n,
        Err(e) => {
            rollback_quietly(tx, model.table).await;
            return refused_delete(&state, model, e);
        }
    };
    // Deleted by someone else since the read: keep their stamp and audit row (#1929).
    if affected == 0 {
        rollback_quietly(tx, model.table).await;
        return Ok(Redirect::to(&list_url).into_response());
    }
    #[cfg(feature = "tenancy")]
    if let Err(e) = operators.check(&mut tx).await {
        rollback_quietly(tx, model.table).await;
        return Err(operator_refusal(e, pk_raw));
    }
    if emit == crate::audit::DiffEmit::InTx {
        if let Err(e) = crate::audit::emit_in_tx(&mut tx, std::slice::from_ref(&entry)).await {
            rollback_quietly(tx, model.table).await;
            return Err(e.into());
        }
    }
    tx.commit().await.map_err(crate::sql::ExecError::from)?;
    if emit == crate::audit::DiffEmit::Deferred {
        super::audit::emit_best_effort(&state, &entry).await;
    }
    // The `delete_model` hook fires after the delete and the audit
    // emit, in both soft and hard delete mode.
    crate::signals::admin::send_admin_post_delete(crate::signals::admin::AdminDeleteContext {
        table: model.table,
        pk: pk_raw.clone(),
        tenant: state.config.tenant.clone(),
    })
    .await;
    Ok(Redirect::to(&list_url).into_response())
}

/// A failed hard delete: a 409 naming who still points at the row when
/// the database refused on an FK (#2340), else the usual error.
fn refused_delete(
    state: &AppState,
    model: &'static crate::core::ModelSchema,
    e: crate::sql::ExecError,
) -> Result<Response, AdminError> {
    if e.refusal() != Some(crate::sql::Refusal::ForeignKey) {
        return Err(e.into());
    }
    // PG names the table; elsewhere list the models whose FK could block.
    let named = e.fk_referencing_table().map(str::to_owned);
    let exact = named.is_some();
    let mut tables: Vec<String> =
        named.map_or_else(|| blocking_referrers(model.table), |t| vec![t]);
    let id = super::errors::log_with_id(
        &format!("admin delete refused; referrers {tables:?}"),
        &e,
        true,
    );
    // Name only tables this user may open in the admin.
    tables.retain(|t| lookup_model(state, t).is_some());
    let by = match (tables.is_empty(), exact) {
        (true, _) => "other rows".to_owned(),
        (false, true) => format!("rows in {}", tables.join(", ")),
        (false, false) => format!("other rows, possibly in {}", tables.join(", ")),
    };
    Ok(crate::api_errors::ApiError::conflict(format!(
        "{} is still referenced by {by}; delete or change those first.",
        model.name
    ))
    .with_details(serde_json::json!({
        "table": model.table,
        "referenced_by": tables,
        "correlation_id": id,
    }))
    .into_response())
}

/// Tables whose FK onto `table` refuses a parent delete.
fn blocking_referrers(table: &str) -> Vec<String> {
    use crate::core::{OnDeleteAction as A, Relation};
    let mut out: Vec<String> = super::helpers::inventory_entries_dedup_by_table()
        .into_iter()
        .filter(|entry| {
            entry.schema.scalar_fields().any(|f| {
                matches!(f.relation, Some(Relation::Fk { to, .. } | Relation::O2O { to, .. }) if to == table)
                    && !matches!(f.fk_on_delete, Some(A::Cascade | A::SetNull | A::SetDefault))
            })
        })
        .map(|entry| entry.schema.table.to_owned())
        .collect();
    out.sort();
    out
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
/// `DELETE WHERE pk IN (...)`. Every action first needs the per-row hooks
/// ([`row_perms`]) to allow each selected row. With no action or no selected
/// rows, redirect back to the list.
pub(crate) async fn action_submit(
    parts: axum::http::request::Parts,
    Path(table): Path<String>,
    state: AppState,
    body: axum::body::Bytes,
) -> Result<Response, AdminError> {
    let model = resolve_model(&state, &table)?;

    // Parse the form keeping repeated keys. `Form<HashMap>` would
    // collapse the duplicate `_selected` keys into one, so read the
    // raw body into a `Vec` of pairs instead.
    let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(&body)
        .map_err(|e| bad_action_form("body", "form", String::new(), e.to_string()))?;

    // The bottom action bar posts `action_bottom` so it does not
    // clash with the top bar's empty default. The first non-empty
    // value wins, whichever bar sent it.
    let mut action_name: Option<String> = None;
    let mut selected_raw: Vec<String> = Vec::new();
    let mut trashed = false;
    for (k, v) in pairs {
        if (k == "action" || k == "action_bottom") && !v.is_empty() && action_name.is_none() {
            action_name = Some(v);
        } else if k == "_selected" {
            selected_raw.push(v);
        } else if k == "trashed" {
            trashed = v == "1";
        }
    }
    if selected_raw.len() > MAX_IN_KEYS {
        return Err(AdminError::Form(forms::FormError::Parse {
            field: "_selected".to_owned(),
            ty: "selection",
            value: selected_raw.len().to_string(),
            detail: format!("an action takes at most {MAX_IN_KEYS} rows"),
        }));
    }
    // Back to the list the action was run from (#1918).
    let list_url = if trashed && model.soft_delete_column.is_some() {
        format!("{}/{}?trashed=1", state.config.admin_prefix, model.table)
    } else {
        format!("{}/{}", state.config.admin_prefix, model.table)
    };
    let back = || Ok(Redirect::to(&list_url).into_response());
    let Some(action) = action_name.filter(|s| !s.is_empty()) else {
        // No action picked: go back to the list.
        return back();
    };
    if selected_raw.is_empty() {
        return back();
    }

    let admin_cfg = admin_config_or_default(model);
    if !admin_cfg.actions.iter().any(|a| *a == action) {
        // A client error (#2346): the allowlist is the model's own.
        return Err(bad_action_form(
            "action",
            "action",
            action,
            format!("not an action of `{}`", model.name),
        ));
    }
    // Pick and permission-check the write before any row signal (#1928).
    let Some(write) = BulkWrite::plan(&state, model, &action)? else {
        return back();
    };

    let pk_field = primary_key_or_internal(model)?;

    let pk_values: Vec<SqlValue> = super::helpers::parse_keys(
        &state,
        model,
        pk_field,
        selected_raw.iter().map(String::as_str),
    )
    .await?
    .into_iter()
    .map(|(_, key)| key)
    .collect();
    if pk_values.is_empty() {
        return back();
    }

    // Read every selected row before the action runs, so the audit
    // emit records what it ran against. For `delete_selected` this
    // is the only copy of the rows that are about to go.
    let action_fields: Vec<&'static FieldSchema> = model.scalar_fields().collect();
    let mut before_rows = crate::sql::select_rows_as_json(
        &state.pool,
        // Only `restore_selected` acts on soft-deleted rows.
        &if matches!(write, BulkWrite::Restore(_)) {
            RowScope::trashed(model, &parts)
        } else {
            RowScope::of(model, &parts)
        }
        .by_pk_in(model, pk_field.column, pk_values.clone()),
        &action_fields,
    )
    .await?;

    // Every action runs the per-row hooks, as the single-row pages do. One
    // refused row refuses the whole action.
    let (perm, named) = row_perms(&write, &action);
    let allowed = |row: &serde_json::Value| {
        let is_allowed = |p: &str| {
            crate::admin::object_permissions::is_allowed(model.table, p, &parts, Some(row))
        };
        is_allowed(perm) && named.map_or(true, is_allowed)
    };
    if !before_rows.iter().all(allowed) {
        return Err(AdminError::Forbidden {
            table: model.table.to_owned(),
            action: perm,
        });
    }
    // Write only the rows that were checked, and audit exactly those.
    let pk_values = rows_with_pk(&mut before_rows, pk_field, &pk_values);
    if pk_values.is_empty() {
        return back();
    }

    let is_delete = matches!(write, BulkWrite::Delete);
    let audit_op = match (is_delete, model.soft_delete_column) {
        (true, Some(_)) => crate::audit::AuditOp::SoftDelete,
        (true, None) => crate::audit::AuditOp::Delete,
        (false, _) => crate::audit::AuditOp::Update,
    };

    // Per-row admin signals: delete for `delete_selected`, an edit for every other action.
    let row_pks: Vec<String> = before_rows
        .iter()
        .map(|row| render::read_value_as_string_json(row, pk_field).unwrap_or_default())
        .collect();
    send_row_signals(&state, model.table, &row_pks, is_delete, true).await;

    // One snapshot per row. For `delete_selected` it is what was deleted;
    // other actions tag the action name, so the panel shows who ran what.
    let tag = (!is_delete).then_some(action.as_str());
    let entries_for = |rows: &[serde_json::Value], pks: &[String]| {
        rows.iter()
            .zip(pks)
            .map(|(row, pk)| {
                super::audit::admin_row_snapshot_entry(model, pk.clone(), audit_op, row, tag)
            })
            .collect::<Vec<_>>()
    };
    let builtin = match &write {
        BulkWrite::Custom(a) => {
            let entries = entries_for(&before_rows, &row_pks);
            match &a.handler {
                // A pool handler writes on its own connection, so its audit follows it.
                ActionHandler::Pool(handler) => {
                    handler(&state.pool, &pk_values).await?;
                    super::audit::emit_many_best_effort(&state, &entries, &action).await;
                }
                ActionHandler::Tx(handler) => {
                    let emit = crate::audit::DiffEmit::for_model(model);
                    let mut tx = crate::audit::PoolBoundTx::begin_write(&state.pool).await?;
                    if let Err(e) = handler(&mut tx, &pk_values).await {
                        rollback_quietly(tx, model.table).await;
                        return Err(e);
                    }
                    if emit == crate::audit::DiffEmit::InTx {
                        if let Err(e) = crate::audit::emit_in_tx(&mut tx, &entries).await {
                            rollback_quietly(tx, model.table).await;
                            return Err(e.into());
                        }
                    }
                    tx.commit().await.map_err(crate::sql::ExecError::from)?;
                    if emit == crate::audit::DiffEmit::Deferred {
                        super::audit::emit_many_best_effort(&state, &entries, &action).await;
                    }
                }
            }
            send_row_signals(&state, model.table, &row_pks, is_delete, false).await;
            return back();
        }
        BulkWrite::Delete => match model.soft_delete_column {
            // Soft delete: stamp the column instead of a DELETE.
            Some(col) => BuiltinWrite::Mark(col, Some(chrono::Utc::now())),
            None => BuiltinWrite::HardDelete,
        },
        // Built-in restore: clear the soft-delete column, where NULL means live.
        BulkWrite::Restore(col) => BuiltinWrite::Mark(*col, None),
    };
    // An `audit(...)` model's entries commit with the write (#2390).
    let emit = crate::audit::DiffEmit::for_model(model);
    let mut tx = crate::audit::PoolBoundTx::begin_write(&state.pool).await?;
    #[cfg(feature = "tenancy")]
    let operators = match crate::tenancy::operators::AdminOperatorWrite::lock(
        &mut tx,
        model.table,
        &before_rows,
    )
    .await
    {
        Ok(w) => w,
        Err(e) => {
            rollback_quietly(tx, model.table).await;
            return Err(e.into());
        }
    };
    let changed = match builtin.run(&mut tx, model, pk_field, &pk_values).await {
        Ok(changed) => changed,
        Err(e) => {
            rollback_quietly(tx, model.table).await;
            // Only a hard delete can hit an FK; else this is `Err(e.into())`.
            return refused_delete(&state, model, e);
        }
    };
    #[cfg(feature = "tenancy")]
    if let Err(e) = operators.check(&mut tx).await {
        rollback_quietly(tx, model.table).await;
        return Err(operator_refusal(e, String::new()));
    }
    // A row someone else deleted or marked since the read keeps their
    // stamp and audit row (#1929).
    let (before_rows, row_pks): (Vec<_>, Vec<_>) = before_rows
        .into_iter()
        .zip(row_pks)
        .filter(|(_, pk)| changed.contains(pk))
        .unzip();
    let entries = entries_for(&before_rows, &row_pks);
    if emit == crate::audit::DiffEmit::InTx {
        if let Err(e) = crate::audit::emit_in_tx(&mut tx, &entries).await {
            rollback_quietly(tx, model.table).await;
            return Err(e.into());
        }
    }
    tx.commit().await.map_err(crate::sql::ExecError::from)?;
    if emit == crate::audit::DiffEmit::Deferred {
        super::audit::emit_many_best_effort(&state, &entries, &action).await;
    }
    send_row_signals(&state, model.table, &row_pks, is_delete, false).await;

    back()
}

/// A refused last-active-operator write: a 400; a driver error stays one.
#[cfg(feature = "tenancy")]
fn operator_refusal(e: crate::tenancy::operators::OperatorError, pk: String) -> AdminError {
    match e {
        crate::tenancy::operators::OperatorError::Driver(e) => e.into(),
        e => bad_action_form("active", "operator", pk, e.to_string()),
    }
}

/// A malformed action POST: a 400, not a 500 (#2346).
fn bad_action_form(field: &str, ty: &'static str, value: String, detail: String) -> AdminError {
    AdminError::Form(forms::FormError::Parse {
        field: field.to_owned(),
        ty,
        value,
        detail,
    })
}

/// The write a bulk action makes, chosen and permission-checked before
/// any row is read or signalled.
enum BulkWrite {
    /// `delete_selected`: a soft delete where the model has the column.
    Delete,
    /// `restore_selected`: clear this soft-delete column.
    Restore(&'static str),
    Custom(super::urls::RegisteredAction),
}

impl BulkWrite {
    /// `None` is a no-op: `restore_selected` on a model without soft delete.
    fn plan(
        state: &AppState,
        model: &'static crate::core::ModelSchema,
        action: &str,
    ) -> Result<Option<Self>, AdminError> {
        let read_only = || AdminError::ReadOnly {
            table: model.table.to_owned(),
        };
        match action {
            "delete_selected" if !state.can_delete(model.table) => Err(read_only()),
            "delete_selected" => Ok(Some(Self::Delete)),
            _ if state.is_read_only(model.table) => Err(read_only()),
            "restore_selected" => Ok(model.soft_delete_column.map(Self::Restore)),
            _ => match state.action_handler(model.table, action) {
                Some(a) if a.perm == ActionPerm::Delete && !state.can_delete(model.table) => {
                    Err(read_only())
                }
                Some(a) => Ok(Some(Self::Custom(a))),
                None => Err(AdminError::Internal(format!(
                    "action `{action}` is in `admin.actions` but no handler is registered \
                     on the admin builder; register it via \
                     `admin::Builder::register_action(\"{}\", \"{action}\", ...)` (built-ins: \
                     delete_selected, restore_selected)",
                    model.table
                ))),
            },
        }
    }
}

/// One admin pre/post signal per row of a bulk action, in row order (#1928).
async fn send_row_signals(
    state: &AppState,
    table: &'static str,
    pks: &[String],
    is_delete: bool,
    pre: bool,
) {
    use crate::signals::admin::{self as sig, AdminDeleteContext, AdminSaveContext};
    let tenant = || state.config.tenant.clone();
    for pk in pks {
        let pk = pk.clone();
        match (is_delete, pre) {
            (true, true) => {
                let ctx = AdminDeleteContext::new(table, pk).with_tenant(tenant());
                sig::send_admin_pre_delete(ctx).await;
            }
            (true, false) => {
                let ctx = AdminDeleteContext::new(table, pk).with_tenant(tenant());
                sig::send_admin_post_delete(ctx).await;
            }
            (false, true) => {
                let ctx = AdminSaveContext::new(table, pk, true).with_tenant(tenant());
                sig::send_admin_pre_save(ctx).await;
            }
            (false, false) => {
                let ctx = AdminSaveContext::new(table, pk, true).with_tenant(tenant());
                sig::send_admin_post_save(ctx).await;
            }
        }
    }
}

/// The object-permission hooks a bulk action must pass on every row. A custom
/// action needs its declared permission plus a hook named after it.
fn row_perms<'a>(write: &BulkWrite, action: &'a str) -> (&'static str, Option<&'a str>) {
    match write {
        BulkWrite::Delete => ("delete", None),
        BulkWrite::Restore(_) => ("change", None),
        BulkWrite::Custom(a) => match a.perm {
            ActionPerm::Delete => ("delete", Some(action)),
            ActionPerm::Change => ("change", Some(action)),
        },
    }
}

/// Keys a bulk write locks and writes per statement.
const BULK_WRITE_CHUNK: usize = 500;

/// The write of a built-in bulk action.
#[derive(Clone, Copy)]
enum BuiltinWrite {
    HardDelete,
    /// Set the soft-delete column to this stamp; `None` restores.
    Mark(&'static str, Option<chrono::DateTime<chrono::Utc>>),
}

impl BuiltinWrite {
    /// Run in `tx`: per chunk, lock the rows it would change, then write
    /// those by key. Returns the keys written, as the PK strings.
    async fn run(
        self,
        tx: &mut crate::sql::PoolTx<'_>,
        model: &'static crate::core::ModelSchema,
        pk_field: &'static FieldSchema,
        pks: &[SqlValue],
    ) -> Result<std::collections::HashSet<String>, crate::sql::ExecError> {
        let pk_in =
            |keys: Vec<SqlValue>| Filter::new(pk_field.column, Op::In, SqlValue::List(keys));
        let mut written = std::collections::HashSet::new();
        for chunk in pks.chunks(BULK_WRITE_CHUNK) {
            let mut filters = vec![pk_in(chunk.to_vec())];
            if let Self::Mark(col, deleted_at) = self {
                // Only rows the mark changes: live ones to delete, trashed to restore.
                filters.push(Filter::new(
                    col,
                    Op::IsNull,
                    SqlValue::Bool(deleted_at.is_some()),
                ));
            }
            let select = SelectQuery {
                where_clause: WhereExpr::and_predicates(filters),
                lock_mode: Some(crate::core::LockMode {
                    silent_on_sqlite: true,
                    ..crate::core::LockMode::default()
                }),
                ..SelectQuery::new(model)
            };
            let mut rows = crate::sql::select_rows_as_json_tx(tx, &select, &[pk_field]).await?;
            let keys = rows_with_pk(&mut rows, pk_field, chunk);
            if keys.is_empty() {
                continue;
            }
            written.extend(
                rows.iter()
                    .filter_map(|row| render::read_value_as_string_json(row, pk_field)),
            );
            let target = WhereExpr::Predicate(pk_in(keys));
            match self {
                Self::HardDelete => {
                    crate::sql::delete_tx(tx, &DeleteQuery::new(model, target)).await?;
                }
                Self::Mark(col, deleted_at) => {
                    let value = deleted_at.map_or(SqlValue::Null, SqlValue::DateTime);
                    let set = vec![Assignment::new(col, value)];
                    crate::sql::update_tx(tx, &UpdateQuery::new(model, set, target)).await?;
                }
            }
        }
        Ok(written)
    }
}

/// Keep the rows whose PK round-trips and return those PKs, so the rows
/// a bulk action writes and the rows it audits are the same list.
/// A PK in `selected` (the keys the rows were read by) keeps its stored form.
fn rows_with_pk(
    rows: &mut Vec<serde_json::Value>,
    pk_field: &FieldSchema,
    selected: &[SqlValue],
) -> Vec<SqlValue> {
    let selected: HashMap<String, &SqlValue> = selected
        .iter()
        .map(|k| (k.to_display_string(), k))
        .collect();
    let mut pks = Vec::with_capacity(rows.len());
    rows.retain(|row| {
        let pk = render::read_value_as_string_json(row, pk_field).and_then(|raw| {
            selected.get(&raw).map_or_else(
                || forms::parse_pk_string(pk_field, &raw).ok(),
                |k| Some((*k).clone()),
            )
        });
        pks.extend(pk.clone());
        pk.is_some()
    });
    pks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_with_pk_drops_the_audit_row_with_its_pk() {
        let pk = FieldSchema::new("id", "id", crate::core::FieldType::I64);
        let mut rows = vec![
            serde_json::json!({"id": 1}),
            serde_json::json!({"id": null}),
            serde_json::json!({"id": 3}),
        ];
        let pks = rows_with_pk(&mut rows, &pk, &[]);
        assert_eq!(pks, vec![SqlValue::I64(1), SqlValue::I64(3)]);
        assert_eq!(
            rows,
            vec![serde_json::json!({"id": 1}), serde_json::json!({"id": 3})]
        );
    }

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
        // Spelled out rather than built from `CREATE_SEGMENT`: against the
        // const this would pass whatever the const said. This used to
        // assert `/add`, which no route has ever served (#1635).
        assert_eq!(url, "/__admin/post/new");
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

    fn order_cols(order: &[crate::core::OrderItem]) -> Vec<(&'static str, bool)> {
        order
            .iter()
            .map(|o| (o.column_name().unwrap(), o.is_desc()))
            .collect()
    }

    /// A time shown to the millisecond and posted back as shown keeps its
    /// stored microseconds; an edited time is written.
    #[test]
    fn echoed_time_prefill_keeps_the_stored_micros() {
        use crate::core::{FieldSchema, FieldType, ModelSchema};
        const FIELDS: &[FieldSchema] = &[FieldSchema::new("at", "at", FieldType::Time)];
        const M: &ModelSchema = &{
            let mut s = ModelSchema::new("T", "t");
            s.fields = FIELDS;
            s
        };
        let before = serde_json::json!({ "at": "10:00:00.123456" });
        for (sent, kept) in [("10:00:00.123", false), ("11:00:00", true)] {
            let form = HashMap::from([("at".to_owned(), sent.to_owned())]);
            let mut values = vec![(
                "at",
                crate::forms::parse_form_value(&FIELDS[0], Some(sent)).unwrap(),
            )];
            keep_unchanged(M, &form, Some(&before), &mut values);
            assert_eq!(!values.is_empty(), kept, "{sent}");
        }
    }

    /// Admin ordering wins, then `default_order`, and the PK always
    /// closes the order so ties page stably (#1917).
    #[test]
    fn list_order_falls_back_and_ends_on_the_pk() {
        use crate::core::{AdminConfig, FieldSchema, FieldType, ModelSchema};
        const FIELDS: &[FieldSchema] = &[
            {
                let mut f = FieldSchema::new("id", "id", FieldType::I64);
                f.primary_key = true;
                f
            },
            FieldSchema::new("rank", "rank", FieldType::I64),
            FieldSchema::new("author", "author_id", FieldType::I64),
        ];
        const BARE: &ModelSchema = &{
            let mut s = ModelSchema::new("P", "p");
            s.fields = FIELDS;
            s
        };
        const DEFAULTED: &ModelSchema = &{
            let mut s = ModelSchema::new("P", "p");
            s.fields = FIELDS;
            s.default_order = &[("rank", true)];
            s
        };
        let none = AdminConfig::DEFAULT;
        let by_rank = AdminConfig {
            ordering: &[("rank", false)],
            ..AdminConfig::DEFAULT
        };
        let by_pk = AdminConfig {
            ordering: &[("id", true)],
            ..AdminConfig::DEFAULT
        };
        assert_eq!(order_cols(&list_order_by(BARE, &none)), [("id", false)]);
        assert_eq!(
            order_cols(&list_order_by(DEFAULTED, &none)),
            [("rank", true), ("id", false)]
        );
        assert_eq!(
            order_cols(&list_order_by(DEFAULTED, &by_rank)),
            [("rank", false), ("id", false)]
        );
        assert_eq!(order_cols(&list_order_by(BARE, &by_pk)), [("id", true)]);
        // A spec naming the column, not the field, still sorts.
        let by_column = AdminConfig {
            ordering: &[("author_id", true)],
            ..AdminConfig::DEFAULT
        };
        assert_eq!(
            order_cols(&list_order_by(BARE, &by_column)),
            [("author_id", true), ("id", false)]
        );
    }

    #[test]
    fn admin_prefix_is_honored() {
        // An app that mounts the admin at `/manage` instead of the
        // default `/__admin` gets the right base path everywhere.
        let url = post_save_redirect("/manage", "post", "42", &form_with("_continue"));
        assert_eq!(url, "/manage/post/42");
    }
}
