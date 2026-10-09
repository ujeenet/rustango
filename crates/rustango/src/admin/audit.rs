//! Admin-side audit handlers and helpers:
//!
//! * `audit_log_view`: `GET /__audit`, the cross-row activity feed.
//! * `audit_cleanup_submit`: `POST /__audit/cleanup` retention.
//! * `admin_audit_entry`: snapshot-shaped entry for create.
//! * `admin_audit_diff_entry`: diff-shaped entry for update.
//!
//! An `audit(...)` model's create or edit writes its entry in the
//! data write's transaction (#2060, #2101).

use std::collections::HashMap;

use axum::extract::{Form, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde_json::Value;

use super::errors::AdminError;
use super::helpers::{chrome_context, ListQuery};
use super::render;
use super::templates::render_with_chrome;
use super::urls::AppState;
use crate::audit::AuditPerm;

/// Page size for the `/__audit` activity feed. Matches the per-table
/// admin list views (50 by default).
const AUDIT_PAGE_SIZE: i64 = 50;

/// Cap on facet values rendered per column on `/__audit`. Same knob as
/// `FACET_TRUNCATE` in the per-table list views. Opt out for one column
/// with `?facet_show_all=<col>`.
const AUDIT_FACET_TRUNCATE: usize = 15;

/// Read access to the audit feed. Only [`AppState::audit_reader`] builds
/// one, so every admin read of the log goes through the permission gate.
pub(crate) struct AuditReader<'a> {
    pool: &'a crate::sql::Pool,
    /// Tables whose rows the user may see. `None` = every table.
    tables: Option<Vec<String>>,
}

impl AuditReader<'_> {
    pub(crate) async fn list(
        &self,
        filter: &crate::audit::AuditFilter,
        page_size: i64,
        offset: i64,
    ) -> Result<Vec<crate::audit::AuditEntry>, sqlx::Error> {
        crate::audit::list_in(self.pool, filter, self.tables.as_deref(), page_size, offset).await
    }

    pub(crate) async fn count(
        &self,
        filter: &crate::audit::AuditFilter,
    ) -> Result<i64, sqlx::Error> {
        crate::audit::count_in(self.pool, filter, self.tables.as_deref()).await
    }

    /// One row's history. The caller already checked `{table}.view` and
    /// the row's scope and `view` hook.
    pub(crate) async fn for_entity(
        &self,
        table: &str,
        pk: &str,
    ) -> Result<Vec<crate::audit::AuditEntry>, sqlx::Error> {
        crate::audit::fetch_for_entity_pool(self.pool, table, pk).await
    }

    pub(crate) async fn facet_counts(
        &self,
        column: &str,
    ) -> Result<Vec<(String, i64)>, sqlx::Error> {
        crate::audit::facet_counts_in(self.pool, column, self.tables.as_deref()).await
    }
}

impl AppState {
    /// `None` unless the user is a superuser or holds [`AuditPerm::View`];
    /// then rows are limited to tables they hold `{table}.view` on.
    ///
    /// A table with a queryset or `view` hook is superuser-only in the feed:
    /// a deleted row's snapshot has no row left to re-scope per entry (#2342).
    pub(crate) fn audit_reader(&self) -> Option<AuditReader<'_>> {
        let tables = match &self.config.user_perms {
            None => None,
            Some(perms) if AuditPerm::View.granted_by(perms) => Some(
                perms
                    .iter()
                    .filter_map(|c| c.strip_suffix(".view"))
                    .filter(|t| self.is_visible(t) && !row_limited(t))
                    .map(str::to_owned)
                    .collect(),
            ),
            Some(_) => return None,
        };
        Some(AuditReader {
            pool: &self.pool,
            tables,
        })
    }

    /// `true` for a superuser or a holder of [`AuditPerm::Delete`].
    /// Cleanup is not table-scoped: it trims every table's rows.
    pub(crate) fn can_clean_audit(&self) -> bool {
        self.config
            .user_perms
            .as_ref()
            .is_none_or(|p| AuditPerm::Delete.granted_by(p))
    }
}

/// `true` when a hook narrows which of `table`'s rows a request may see.
fn row_limited(table: &str) -> bool {
    !super::queryset_hooks::for_table(table).is_empty()
        || super::object_permissions::has_hook(table, "view")
}

/// The feed's mounted path: `audit_url` is relative to the admin prefix.
fn audit_path(state: &AppState) -> String {
    format!("{}{}", state.config.admin_prefix, state.config.audit_url)
}

fn audit_forbidden(action: &'static str) -> AdminError {
    AdminError::Forbidden {
        table: "rustango_audit_log".to_owned(),
        action,
    }
}

/// `GET /__audit`: cross-row activity feed of `rustango_audit_log`,
/// newest first and paginated. Filters on `?entity_table=`,
/// `?entity_pk=`, `?operation=` and `?source=`. The right rail shows
/// distinct values, counts and toggle URLs, like `list_filter` does.
pub(crate) async fn audit_log_view(
    Query(params): Query<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Result<Html<String>, AdminError> {
    let reader = state
        .audit_reader()
        .ok_or_else(|| audit_forbidden("view"))?;
    let page = crate::list_params::parse_page(&params);
    let offset = crate::list_params::page_offset(page, AUDIT_PAGE_SIZE);

    // Collect the active filters into an `AuditFilter` for the listing
    // helpers in `crate::audit`. `entity_pk` is filterable (it drives
    // the "View full history" link on a row's detail page) but it is
    // not in the facet rail: per-PK cardinality is unbounded, so it
    // only shows up as an active-filter pill.
    let filter = crate::audit::AuditFilter {
        entity_table: params
            .get("entity_table")
            .filter(|s| !s.is_empty())
            .cloned(),
        entity_pk: params.get("entity_pk").filter(|s| !s.is_empty()).cloned(),
        operation: params.get("operation").filter(|s| !s.is_empty()).cloned(),
        source: params.get("source").filter(|s| !s.is_empty()).cloned(),
    };
    // Stable ordering for the active-filter pill render + pager
    // extras builder below.
    let mut active_field_filters: Vec<(&'static str, String)> = Vec::new();
    for (col, val) in [
        ("entity_table", &filter.entity_table),
        ("entity_pk", &filter.entity_pk),
        ("operation", &filter.operation),
        ("source", &filter.source),
    ] {
        if let Some(v) = val.as_deref().filter(|s| !s.is_empty()) {
            active_field_filters.push((col, v.to_owned()));
        }
    }

    // Every feed link derives from this, prefix included (#1916).
    let mut feed_query = ListQuery::new(audit_path(&state));
    for (k, v) in &active_field_filters {
        feed_query.push(*k, v.clone());
    }

    // Count, page of rows and facet group-bys all go through the
    // helpers in `crate::audit`, which render SQL per dialect, so this
    // view works on any supported backend.
    let total = reader.count(&filter).await.unwrap_or(0);
    let entries = reader
        .list(&filter, AUDIT_PAGE_SIZE, offset)
        .await
        .unwrap_or_default();

    // Distinct values and counts per facet. Always read from the
    // unfiltered table, so an operator can still reach any value.
    // Ordered by count, then alphabetically, for a stable render.
    let mut facets_ctx: Vec<Value> = Vec::new();
    let show_all_facet = params.get("facet_show_all").map(String::as_str);
    for col in ["entity_table", "operation", "source"] {
        let active_value = active_field_filters
            .iter()
            .find(|(k, _)| *k == col)
            .map(|(_, v)| v.as_str());
        let facet_pairs = reader.facet_counts(col).await.unwrap_or_default();
        let mut values: Vec<Value> = facet_pairs
            .iter()
            .map(|(raw, count)| {
                let is_active = active_value.map(|v| v == raw).unwrap_or(false);
                let mut toggle = feed_query.without(&[col]);
                if !is_active {
                    toggle = toggle.with(col, raw.clone());
                }
                let url = toggle.url();
                serde_json::json!({
                    "raw": raw.clone(),
                    "display": render::escape(raw),
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
            Some(feed_query.clone().with("facet_show_all", col).url())
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

    let entries_ctx: Vec<Value> = entries
        .iter()
        .map(|e| {
            let (action_name, cleaned) = split_action_marker(&e.changes);
            // Use the configured prefix, not a hardcoded `/__admin`,
            // so "view this record" links work on any prefix.
            let admin_prefix = state.config.admin_prefix.as_str();
            serde_json::json!({
                "id": e.id,
                "entity_table": e.entity_table,
                "entity_pk": e.entity_pk,
                "detail_url": format!(
                    "{admin_prefix}/{}/{}",
                    url_encode_q(&e.entity_table),
                    url_encode_q(&e.entity_pk)
                ),
                "operation": e.operation,
                "action_name": action_name,
                "source": e.source,
                "changes": serde_json::to_string_pretty(&cleaned).unwrap_or_default(),
                "occurred_at": e.occurred_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
            })
        })
        .collect();

    // Pager URL preserves the active facets.
    let pager_extras = feed_query.suffix();

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
        "can_clean_audit": state.can_clean_audit(),
    });
    Ok(Html(render_with_chrome(
        "audit_log.html",
        &mut ctx,
        chrome_context(&state, Some("__audit")),
    )))
}

/// `POST /__audit/cleanup`: apply a retention policy to the audit log.
/// The form's `mode` is `"older_than"` (the default) or `"keep_last"`,
/// with the matching numeric input. The cleanup emits its own audit
/// entry, so the trail records that it ran.
pub(crate) async fn audit_cleanup_submit(
    State(state): State<AppState>,
    Form(form): Form<HashMap<String, String>>,
) -> Result<Response, AdminError> {
    if !state.can_clean_audit() {
        return Err(audit_forbidden("delete"));
    }
    let mode = form.get("mode").map(String::as_str).unwrap_or("older_than");
    let (_removed, changes) = match mode {
        "keep_last" => {
            let keep: i64 = form
                .get("keep")
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(50)
                .max(0);
            let removed = crate::audit::cleanup_keep_last_n_pool(&state.pool, keep)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(target: "rustango::admin::audit",
                        error = %e, "cleanup_keep_last_n_pool failed");
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
            let removed = crate::audit::cleanup_older_than_pool(&state.pool, days)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(target: "rustango::admin::audit",
                        error = %e, "cleanup_older_than_pool failed");
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
    if let Err(e) = crate::audit::emit_one_pool(&state.pool, &entry).await {
        tracing::warn!(target: "rustango::admin::audit",
            error = %e, "audit_cleanup self-audit emit failed");
    }
    Ok(Redirect::to(&audit_path(&state)).into_response())
}

/// Diff-shaped audit entry for an admin UPDATE: `{ "field": { "before": v,
/// "after": v } }` from the row locked before the write, secrets masked.
/// `None` when nothing changed.
pub(crate) fn admin_audit_diff_entry(
    model: &'static crate::core::ModelSchema,
    pk_str: &str,
    row: &serde_json::Value,
    form: &HashMap<String, String>,
) -> Option<crate::audit::PendingEntry> {
    let row = &mask_secrets(model, &super::helpers::admin_config_or_default(model), row);
    // Both sides use typed JSON (numbers as numbers, bools as bools),
    // so an app-code write and an admin form POST produce the same
    // diff. `before` reads the SELECTed row; `after` coerces the form
    // values, and falls back to the row for keys the form omits.
    // Row reads go through `render::read_value_as_json_from_json`,
    // which is dialect-agnostic and normalizes per `FieldType`.
    let before_pairs: Vec<(&str, Value)> = model
        .scalar_fields()
        .filter(|f| {
            model
                .audit_track
                .map_or(true, |names| names.is_empty() || names.contains(&f.name))
        })
        .map(|f| (f.name, render::read_value_as_json_from_json(row, f)))
        .collect();
    let after_pairs: Vec<(&str, Value)> = model
        .scalar_fields()
        .filter(|f| {
            model
                .audit_track
                .map_or(true, |names| names.is_empty() || names.contains(&f.name))
        })
        .map(|f| {
            let v = match form.get(f.name) {
                Some(s) => render::coerce_form_to_json(f, s),
                None => render::read_value_as_json_from_json(row, f),
            };
            (f.name, v)
        })
        .collect();
    crate::audit::PendingEntry::update_diff(
        model.table,
        pk_str.to_owned(),
        &before_pairs,
        &after_pairs,
    )
}

/// Stands in for a secret in the audit log.
const SECRET_CHANGED: &str = "[changed]";
const SECRET_SET: &str = "[set]";

/// What the audit log records for a write: the form without secrets, a
/// marker for each secret written, and timestamps the server stamped.
pub(super) fn audit_form(
    model: &'static crate::core::ModelSchema,
    admin_cfg: &crate::core::AdminConfig,
    form: &HashMap<String, String>,
    written: &[(&'static str, crate::core::SqlValue)],
) -> HashMap<String, String> {
    // Only written fields: a POSTed readonly or hidden value is skipped,
    // so the diff must fall back to the row for it (#1939).
    let is_written = |name: &str| {
        model
            .field(name)
            .is_some_and(|f| written.iter().any(|(c, _)| *c == f.column))
    };
    let mut out: HashMap<String, String> = form
        .iter()
        .filter(|(k, _)| !super::helpers::is_secret_field(admin_cfg, k) && is_written(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (column, value) in written {
        let Some(f) = model.scalar_fields().find(|f| f.column == *column) else {
            continue;
        };
        if super::helpers::is_secret_field(admin_cfg, f.name) {
            out.insert(f.name.to_owned(), SECRET_CHANGED.to_owned());
        } else if let crate::core::SqlValue::DateTime(at) = value {
            out.insert(f.name.to_owned(), at.to_rfc3339());
        }
    }
    out
}

/// `row` with each set secret replaced by a marker, for the audit log.
fn mask_secrets(
    model: &'static crate::core::ModelSchema,
    admin_cfg: &crate::core::AdminConfig,
    row: &Value,
) -> Value {
    let mut row = row.clone();
    for f in model.scalar_fields() {
        if super::helpers::is_secret_field(admin_cfg, f.name) {
            if let Some(v) = row.get_mut(f.name).filter(|v| !v.is_null()) {
                *v = Value::String(SECRET_SET.to_owned());
            }
        }
    }
    row
}

/// Write `entry` after its data write committed; a failure only warns.
pub(crate) async fn emit_best_effort(state: &AppState, entry: &crate::audit::PendingEntry) {
    if let Err(e) = crate::audit::emit_one_pool(&state.pool, entry).await {
        tracing::warn!(
            target: "rustango::admin::audit",
            error = %e,
            entity_table = %entry.entity_table,
            entity_pk = %entry.entity_pk,
            "admin audit emit failed (data write already committed)",
        );
    }
}

/// [`emit_best_effort`] for a bulk action's entries.
pub(crate) async fn emit_many_best_effort(
    state: &AppState,
    entries: &[crate::audit::PendingEntry],
    action: &str,
) {
    let Some(first) = entries.first() else {
        return;
    };
    if let Err(e) = crate::audit::emit_many_pool(&state.pool, entries).await {
        tracing::warn!(
            target: "rustango::admin::audit",
            error = %e,
            entity_table = %first.entity_table,
            action = %action,
            count = entries.len(),
            "admin bulk-action audit emit failed",
        );
    }
}

/// Snapshot audit entry from a form submission.
pub(crate) fn admin_audit_entry(
    model: &'static crate::core::ModelSchema,
    pk_str: &str,
    op: crate::audit::AuditOp,
    form: &HashMap<String, String>,
) -> crate::audit::PendingEntry {
    // Snapshot every field the form carries and skip the rest, such as
    // unchecked checkboxes. Values coerce to typed JSON so a create
    // snapshot has the same shape as a diff.
    let pairs: Vec<(&str, Value)> = model
        .scalar_fields()
        .filter(|f| {
            model
                .audit_track
                .map_or(true, |names| names.is_empty() || names.contains(&f.name))
        })
        .filter_map(|f| {
            form.get(f.name)
                .map(|v| (f.name, render::coerce_form_to_json(f, v)))
        })
        .collect();
    crate::audit::PendingEntry {
        entity_table: model.table,
        entity_pk: pk_str.to_owned(),
        operation: op,
        source: crate::audit::current_source(),
        changes: crate::audit::snapshot_changes(&pairs),
    }
}

/// Snapshot of a whole stored row, secrets masked, as a delete or a bulk
/// action records it. `action` tags a custom action's name.
pub(crate) fn admin_row_snapshot_entry(
    model: &'static crate::core::ModelSchema,
    pk_str: String,
    op: crate::audit::AuditOp,
    row: &Value,
    action: Option<&str>,
) -> crate::audit::PendingEntry {
    let cfg = super::helpers::admin_config_or_default(model);
    let row = mask_secrets(model, &cfg, row);
    let mut pairs: Vec<(&str, Value)> = model
        .scalar_fields()
        .map(|f| (f.name, render::read_value_as_json_from_json(&row, f)))
        .collect();
    if let Some(name) = action {
        pairs.push(("__action", Value::String(name.to_owned())));
    }
    crate::audit::PendingEntry {
        entity_table: model.table,
        entity_pk: pk_str,
        operation: op,
        source: crate::audit::current_source(),
        changes: crate::audit::snapshot_changes(&pairs),
    }
}

/// Split the `__action` marker out of a `changes` object. Returns
/// `(action_name, cleaned_changes)`, so the panel can show the action
/// as a badge instead of as a changed field.
///
/// Bulk-action rows and the self-audit row from `/__audit/cleanup` both
/// carry this marker.
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

// The canonical encoder under the local name. An earlier local version
// left `/`, `@` and non-ASCII bytes unencoded.
use crate::url_codec::url_encode as url_encode_q;

#[cfg(test)]
mod tests {
    use super::*;

    /// #1939: a POSTed field the update skipped is not an audit "after".
    #[test]
    fn the_audit_form_holds_only_written_fields() {
        use crate::core::Model as _;
        let model = crate::admin::user::AdminUser::SCHEMA;
        let form: HashMap<String, String> = [
            ("username", "alice"),
            ("sessions_revoked_at", "2020-01-01T00:00"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let written = vec![("username", crate::core::SqlValue::String("alice".into()))];
        let out = audit_form(model, &crate::core::AdminConfig::DEFAULT, &form, &written);
        assert_eq!(out.get("username").map(String::as_str), Some("alice"));
        assert!(!out.contains_key("sessions_revoked_at"), "{out:?}");
    }
}
