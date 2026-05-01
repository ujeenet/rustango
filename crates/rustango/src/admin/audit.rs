//! Admin-side audit handlers and helpers.
//!
//! Extracted from `admin/views.rs` in v0.13.0 — the audit slices
//! (v0.12.5 activity feed, v0.12.6 retention, v0.12.7 per-row
//! history link, v0.12.8 keep-last cleanup) had grown the views
//! module past 1400 lines. This module owns:
//!
//! * `audit_log_view` — `GET /__audit` cross-row activity feed.
//! * `audit_cleanup_submit` — `POST /__audit/cleanup` retention.
//! * `emit_admin_audit` — snapshot-shaped audit emit for create + bulk.
//! * `emit_admin_audit_diff` — diff-shaped emit for update.
//!
//! View handlers in `views.rs` (`create_submit`, `update_submit`,
//! `delete_submit`, `action_submit`) call into this module after
//! the data write, so behaviour and JSON shape match what shipped
//! in v0.12.x — only the file layout changed.

use std::collections::HashMap;

use axum::extract::{Form, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde_json::Value;

use crate::sql::sqlx;

use super::errors::AdminError;
use super::render;
use super::templates::render_with_chrome;
use super::urls::AppState;
use super::helpers::chrome_context;

/// Page size for the `/__audit` activity feed. Matches the per-table
/// admin list views (50 by default).
const AUDIT_PAGE_SIZE: i64 = 50;

/// Cap on facet values rendered per column on `/__audit`. Mirrors
/// the `FACET_TRUNCATE` knob used by `compute_facets` for per-table
/// list views; opt out per-column with `?facet_show_all=<col>`.
const AUDIT_FACET_TRUNCATE: usize = 15;

/// `GET /__audit` — cross-row activity feed of `rustango_audit_log`.
/// Newest first, paginated. Supports `?entity_table=...`,
/// `?entity_pk=...`, `?operation=...`, `?source=...` filter params;
/// the right rail shows distinct values + counts and toggle URLs
/// (mirrors the `list_filter` UI shape).
pub(crate) async fn audit_log_view(
    Query(params): Query<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Result<Html<String>, AdminError> {
    let page = params
        .get("page")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(1)
        .max(1);
    let offset = (page - 1) * AUDIT_PAGE_SIZE;

    // Build dynamic WHERE from the per-facet params. Each (column,
    // value) pair is appended to the SQL with a $N placeholder; sqlx
    // binds them positionally. `entity_pk` is filterable too — used
    // by the "View full history" link on each row's detail page —
    // but not in the facet rail (per-PK distinct-value cardinality
    // is unbounded; appears only as an active-filter pill).
    let mut where_sql = String::new();
    let mut binds: Vec<String> = Vec::new();
    let mut active_field_filters: Vec<(&'static str, String)> = Vec::new();
    for (col_static, key) in [
        ("entity_table", "entity_table"),
        ("entity_pk", "entity_pk"),
        ("operation", "operation"),
        ("source", "source"),
    ] {
        if let Some(v) = params.get(key).filter(|s| !s.is_empty()) {
            binds.push(v.clone());
            let placeholder = binds.len();
            if where_sql.is_empty() {
                where_sql.push_str(" WHERE ");
            } else {
                where_sql.push_str(" AND ");
            }
            use std::fmt::Write as _;
            let _ = write!(
                where_sql,
                r#""{col_static}" = ${placeholder}"#,
            );
            active_field_filters.push((col_static, v.clone()));
        }
    }

    // Total count for the pager.
    let count_sql = format!(
        r#"SELECT COUNT(*) FROM "rustango_audit_log"{where_sql}"#,
    );
    let mut count_q = sqlx::query_scalar::<_, i64>(&count_sql);
    for v in &binds {
        count_q = count_q.bind(v);
    }
    let total: i64 = count_q.fetch_one(&state.pool).await.unwrap_or(0);

    // Page of rows.
    let rows_sql = format!(
        r#"SELECT "id", "entity_table", "entity_pk", "operation", "source",
                  "changes", "occurred_at"
           FROM "rustango_audit_log"{where_sql}
           ORDER BY "occurred_at" DESC, "id" DESC
           LIMIT $%LIMIT% OFFSET $%OFFSET%"#,
    )
    .replace("$%LIMIT%", &format!("${}", binds.len() + 1))
    .replace("$%OFFSET%", &format!("${}", binds.len() + 2));
    let mut rows_q = sqlx::query(&rows_sql);
    for v in &binds {
        rows_q = rows_q.bind(v);
    }
    rows_q = rows_q.bind(AUDIT_PAGE_SIZE).bind(offset);
    let rows = rows_q.fetch_all(&state.pool).await.unwrap_or_default();

    // Per-facet distinct values + counts. One small GROUP BY query
    // per facet column. Always runs against the unfiltered table so
    // operators can navigate to any value without losing them.
    let mut facets_ctx: Vec<Value> = Vec::new();
    let show_all_facet = params
        .get("facet_show_all")
        .map(String::as_str);
    for col in ["entity_table", "operation", "source"] {
        let active_value = active_field_filters
            .iter()
            .find(|(k, _)| *k == col)
            .map(|(_, v)| v.as_str());
        // v0.13.1: count desc with alpha tie-break — most active
        // value first, deterministic output across requests.
        let q_sql = format!(
            r#"SELECT "{col}" AS facet_value, COUNT(*) AS facet_count
               FROM "rustango_audit_log"
               GROUP BY "{col}"
               ORDER BY facet_count DESC, "{col}""#,
        );
        let facet_rows = sqlx::query(&q_sql)
            .fetch_all(&state.pool)
            .await
            .unwrap_or_default();
        let mut values: Vec<Value> = facet_rows
            .iter()
            .map(|r| {
                use sqlx::Row;
                let raw: String = r.try_get("facet_value").unwrap_or_default();
                let count: i64 = r.try_get("facet_count").unwrap_or(0);
                let is_active = active_value.map(|v| v == raw).unwrap_or(false);
                let mut params: Vec<(String, String)> = Vec::new();
                for (k, v) in &active_field_filters {
                    if *k == col {
                        continue;
                    }
                    params.push(((*k).into(), v.clone()));
                }
                if !is_active {
                    params.push((col.into(), raw.clone()));
                }
                let mut url = String::from("/__audit");
                if !params.is_empty() {
                    url.push('?');
                    let qs: Vec<String> = params
                        .iter()
                        .map(|(k, v)| format!("{}={}", url_encode_q(k), url_encode_q(v)))
                        .collect();
                    url.push_str(&qs.join("&"));
                }
                serde_json::json!({
                    "raw": raw.clone(),
                    "display": render::escape(&raw),
                    "count": count,
                    "active": is_active,
                    "toggle_url": url,
                })
            })
            .collect();
        let show_all = show_all_facet == Some(col);
        let total_values = values.len();
        let mut more_count: usize = 0;
        if !show_all && total_values > AUDIT_FACET_TRUNCATE {
            let mut active_first: Vec<Value> = Vec::new();
            let mut rest: Vec<Value> = Vec::new();
            for v in values.into_iter() {
                if v.get("active").and_then(|b| b.as_bool()).unwrap_or(false) {
                    active_first.push(v);
                } else {
                    rest.push(v);
                }
            }
            let cap = AUDIT_FACET_TRUNCATE.saturating_sub(active_first.len());
            let kept_rest_len = rest.len().min(cap);
            more_count = total_values - active_first.len() - kept_rest_len;
            active_first.extend(rest.into_iter().take(cap));
            values = active_first;
        }
        let show_all_url = if more_count > 0 {
            let mut params: Vec<(String, String)> = Vec::new();
            for (k, v) in &active_field_filters {
                params.push(((*k).into(), v.clone()));
            }
            params.push(("facet_show_all".into(), col.into()));
            let mut url = String::from("/__audit?");
            let qs: Vec<String> = params
                .iter()
                .map(|(k, v)| format!("{}={}", url_encode_q(k), url_encode_q(v)))
                .collect();
            url.push_str(&qs.join("&"));
            Some(url)
        } else {
            None
        };
        facets_ctx.push(serde_json::json!({
            "field": col,
            "values": values,
            "more_count": more_count,
            "show_all_url": show_all_url,
        }));
    }

    let last_page = if total == 0 {
        1
    } else {
        ((total - 1) / AUDIT_PAGE_SIZE) + 1
    };

    let entries_ctx: Vec<Value> = rows
        .iter()
        .map(|r| {
            use sqlx::Row;
            let id: i64 = r.try_get("id").unwrap_or(0);
            let entity_table: String = r.try_get("entity_table").unwrap_or_default();
            let entity_pk: String = r.try_get("entity_pk").unwrap_or_default();
            let operation: String = r.try_get("operation").unwrap_or_default();
            let source: String = r.try_get("source").unwrap_or_default();
            let raw_changes: Value = r.try_get("changes").unwrap_or(Value::Null);
            let (action_name, cleaned) = split_action_marker(&raw_changes);
            let occurred_at: chrono::DateTime<chrono::Utc> = r
                .try_get("occurred_at")
                .unwrap_or_else(|_| chrono::Utc::now());
            serde_json::json!({
                "id": id,
                "entity_table": entity_table,
                "entity_pk": entity_pk,
                "detail_url": format!("/__admin/{entity_table}/{entity_pk}"),
                "operation": operation,
                "action_name": action_name,
                "source": source,
                "changes": serde_json::to_string_pretty(&cleaned).unwrap_or_default(),
                "occurred_at": occurred_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
            })
        })
        .collect();

    // Pager URL preserves the active facets.
    let mut pager_extras = String::new();
    for (k, v) in &active_field_filters {
        use std::fmt::Write as _;
        let _ = write!(pager_extras, "&{}={}", url_encode_q(k), url_encode_q(v));
    }

    let active_filters_ctx: Vec<Value> = active_field_filters
        .iter()
        .map(|(k, v)| serde_json::json!({ "key": k, "value": v }))
        .collect();

    let mut ctx = serde_json::json!({
        "total": total,
        "plural": if total == 1 { "" } else { "s" },
        "entries": entries_ctx,
        "facets": facets_ctx,
        "active_filters": active_filters_ctx,
        "page": page,
        "last_page": last_page,
        "pager_extras": pager_extras,
    });
    Ok(Html(render_with_chrome(
        "audit_log.html",
        &mut ctx,
        chrome_context(&state, Some("__audit")),
    )))
}

/// `POST /__audit/cleanup` — apply a retention policy to the audit
/// log. Reads `mode` from the form (`"older_than"` or `"keep_last"`,
/// default `"older_than"`) and the corresponding numeric input. The
/// cleanup itself emits an audit entry so the trail is
/// self-describing.
pub(crate) async fn audit_cleanup_submit(
    State(state): State<AppState>,
    Form(form): Form<HashMap<String, String>>,
) -> Result<Response, AdminError> {
    let mode = form.get("mode").map(String::as_str).unwrap_or("older_than");
    let (_removed, changes) = match mode {
        "keep_last" => {
            let keep: i64 = form
                .get("keep")
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(50)
                .max(0);
            let removed = crate::audit::cleanup_keep_last_n(&state.pool, keep)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(target: "rustango::admin::audit",
                        error = %e, "cleanup_keep_last_n failed");
                    0
                });
            (
                removed,
                serde_json::json!({
                    "__action": "audit_cleanup",
                    "mode": "keep_last",
                    "keep": keep,
                    "removed": removed,
                }),
            )
        }
        _ => {
            let days: i64 = form
                .get("days")
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(90)
                .max(0);
            let removed = crate::audit::cleanup_older_than(&state.pool, days)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(target: "rustango::admin::audit",
                        error = %e, "cleanup_older_than failed");
                    0
                });
            (
                removed,
                serde_json::json!({
                    "__action": "audit_cleanup",
                    "mode": "older_than",
                    "cutoff_days": days,
                    "removed": removed,
                }),
            )
        }
    };
    let entry = crate::audit::PendingEntry {
        entity_table: "rustango_audit_log",
        entity_pk: "*".into(),
        operation: crate::audit::AuditOp::Delete,
        source: crate::audit::current_source(),
        changes,
    };
    if let Err(e) = crate::audit::emit_one(&state.pool, &entry).await {
        tracing::warn!(target: "rustango::admin::audit",
            error = %e, "audit_cleanup self-audit emit failed");
    }
    Ok(Redirect::to("/__audit").into_response())
}

/// Diff-shaped audit emit for admin UPDATE writes. Reads the
/// pre-update row that `update_submit` SELECTed before the UPDATE,
/// builds a `{ "field": { "before": v, "after": v } }` JSON via
/// `audit::diff_changes`, and emits an Update entry. Falls back to
/// the snapshot path when the before-row is missing (rare:
/// concurrent delete between SELECT and UPDATE).
pub(crate) async fn emit_admin_audit_diff(
    state: &AppState,
    model: &'static crate::core::ModelSchema,
    pk_str: &str,
    before_row: Option<&sqlx::postgres::PgRow>,
    form: &HashMap<String, String>,
) {
    let Some(row) = before_row else {
        emit_admin_audit(state, model, pk_str, crate::audit::AuditOp::Update, form).await;
        return;
    };
    // v0.13.0: typed JSON values (numbers as numbers, bools as
    // bools) match the macro-emit path's shape so app-code writes
    // and admin form POSTs produce the same diff JSON. `before` reads
    // typed values from the SELECTed row; `after` parses form
    // payloads back to the typed shape via `coerce_form_to_json`,
    // falling back to the row's prior value for absent keys.
    let before_pairs: Vec<(&str, Value)> = model
        .scalar_fields()
        .map(|f| (f.name, render::read_value_as_json(row, f)))
        .collect();
    let after_pairs: Vec<(&str, Value)> = model
        .scalar_fields()
        .map(|f| {
            let v = match form.get(f.name) {
                Some(s) => render::coerce_form_to_json(f, s),
                None => render::read_value_as_json(row, f),
            };
            (f.name, v)
        })
        .collect();
    let entry = crate::audit::PendingEntry {
        entity_table: model.table,
        entity_pk: pk_str.to_owned(),
        operation: crate::audit::AuditOp::Update,
        source: crate::audit::current_source(),
        changes: crate::audit::diff_changes(&before_pairs, &after_pairs),
    };
    if let Err(e) = crate::audit::emit_one(&state.pool, &entry).await {
        tracing::warn!(
            target: "rustango::admin::audit",
            error = %e,
            entity_table = %model.table,
            entity_pk = %pk_str,
            "admin audit emit failed (data write already committed)",
        );
    }
}

/// Build an audit `PendingEntry` from a form submission and emit it
/// to `rustango_audit_log`. Best-effort — failures here log a
/// warning but don't fail the user-visible request, since the data
/// write already succeeded.
pub(crate) async fn emit_admin_audit(
    state: &AppState,
    model: &'static crate::core::ModelSchema,
    pk_str: &str,
    op: crate::audit::AuditOp,
    form: &HashMap<String, String>,
) {
    // Snapshot every field present in the form (skip anything that
    // didn't show up — e.g. `_selected` / unchecked checkboxes).
    // v0.13.0: form values coerce to typed JSON so create-snapshots
    // match the diff/macro shapes (numbers as numbers, bools as
    // bools, strings as strings).
    let pairs: Vec<(&str, Value)> = model
        .scalar_fields()
        .filter_map(|f| {
            form.get(f.name)
                .map(|v| (f.name, render::coerce_form_to_json(f, v)))
        })
        .collect();
    let entry = crate::audit::PendingEntry {
        entity_table: model.table,
        entity_pk: pk_str.to_owned(),
        operation: op,
        source: crate::audit::current_source(),
        changes: crate::audit::snapshot_changes(&pairs),
    };
    if let Err(e) = crate::audit::emit_one(&state.pool, &entry).await {
        tracing::warn!(
            target: "rustango::admin::audit",
            error = %e,
            entity_table = %model.table,
            entity_pk = %pk_str,
            "admin audit emit failed (data write already committed)",
        );
    }
}

/// Pull the `__action` marker out of a `changes` JSON object so the
/// audit panel can render it as a distinct badge rather than treating
/// it as a regular changed field. Returns `(action_name,
/// cleaned_changes)` — the cleaned JSON has `__action` removed if it
/// was present, leaving the actual user-visible changes.
///
/// Bulk-action audit rows (v0.12.4) carry `__action: "<name>"`
/// alongside the row's pre-action snapshot. Self-audit rows from
/// `/__audit/cleanup` (v0.12.6) carry it on a synthetic
/// `entity_table = "rustango_audit_log"` row. Both want the badge
/// rendering, neither wants the marker shown as a "change".
pub(crate) fn split_action_marker(changes: &Value) -> (Option<String>, Value) {
    if let Value::Object(map) = changes {
        if let Some(Value::String(name)) = map.get("__action") {
            let mut cleaned = map.clone();
            cleaned.remove("__action");
            return (Some(name.clone()), Value::Object(cleaned));
        }
    }
    (None, changes.clone())
}

/// Tiny ASCII-safe percent encoder for query-string values.
fn url_encode_q(s: &str) -> String {
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
