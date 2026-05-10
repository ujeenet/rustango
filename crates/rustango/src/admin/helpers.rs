//! View-side helpers shared across handlers — model lookup, FK join
//! composition, FK display-value mapping, list-cell rendering, form
//! rendering, and pager URL composition.

use std::collections::HashMap;
use std::fmt::Write as _;

use crate::core::{inventory, FieldSchema, Join, ModelEntry, ModelSchema, Relation};
use crate::sql::sqlx;

use super::render;
#[allow(unused_imports)]
use super::templates::render_template;
use super::urls::AppState;

/// Map of `(target_table, source_value_string) → display_value_html`.
/// Populated from joined rows so list/detail rendering needs no extra
/// per-FK queries.
pub(crate) type FkMap = HashMap<(String, String), String>;

/// Iterate the model inventory, deduplicating entries that share a SQL
/// table name. When two models point at the same `table`, the one with
/// **more fields** wins; ties resolve to the first inventory order.
///
/// This is what makes a project-side override like
/// [`crate::tenancy::TenantUserModel`] visible to the admin even when
/// the framework's own model is also registered for the same table —
/// e.g. `AppUser` (9 fields) shadows the framework's `User` (7 fields)
/// on `rustango_users`. The richer schema is also what we want for
/// list/detail rendering since the user explicitly added columns by
/// declaring it.
pub(crate) fn inventory_entries_dedup_by_table() -> Vec<&'static ModelEntry> {
    let mut by_table: indexmap::IndexMap<&'static str, &'static ModelEntry> =
        indexmap::IndexMap::new();
    for entry in inventory::iter::<ModelEntry> {
        let table = entry.schema.table;
        match by_table.get(table) {
            Some(existing) if existing.schema.fields.len() >= entry.schema.fields.len() => {
                // Existing is at least as rich — keep it.
            }
            _ => {
                by_table.insert(table, entry);
            }
        }
    }
    by_table.into_values().collect()
}

/// Build the standard chrome context (sidebar + active-link state)
/// that every admin page renders. Pass the active table (or `None` on
/// the index page) so the matching sidebar link gets `class="active"`.
///
/// Brand fields are layered: `brand_name` falls back to `admin_title`
/// (set by [`crate::admin::Builder::title`]) which falls back to
/// `"Rustango Admin"`. Same chain for `brand_tagline` → `admin_subtitle`.
/// Logos / theme mode / per-tenant CSS overrides come straight off
/// the config — they're set per-request by the tenancy admin from
/// the resolved [`crate::tenancy::Org`].
pub(crate) fn chrome_context(state: &AppState, active_table: Option<&str>) -> serde_json::Value {
    let admin_title = state.config.title.as_deref().unwrap_or("Rustango Admin");
    let brand_name = state.config.brand_name.as_deref().unwrap_or(admin_title);
    let brand_tagline = state
        .config
        .brand_tagline
        .as_deref()
        .or(state.config.subtitle.as_deref());
    serde_json::json!({
        "sidebar_groups": sidebar_context(state, active_table),
        "active_table": active_table.unwrap_or(""),
        "admin_title": admin_title,
        "admin_subtitle": state.config.subtitle.as_deref(),
        "brand_name": brand_name,
        "brand_tagline": brand_tagline,
        "brand_logo_url": state.config.brand_logo_url.as_deref(),
        "theme_mode": state.config.theme_mode.as_deref().unwrap_or("auto"),
        "tenant_brand_css": state.config.tenant_brand_css.as_deref(),
        // v0.27.8 (#78) — impersonation banner. Templates render
        // an unmissable warning when this is non-null so the
        // operator can't accidentally mutate tenant data while
        // forgetting they're impersonating.
        "impersonated_by_operator_id": state.config.impersonated_by,
        // v0.27.9 (#59) — URL prefix the admin Router is mounted
        // under. Templates use `{{ admin_prefix }}{{ audit_url }}` etc.
        // so hrefs resolve correctly regardless of mount path.
        "admin_prefix": &state.config.admin_prefix,
        // v0.30.19 — URL prefix for embedded static assets
        // (logo + favicon). Templates use {{ static_url }}/icon.png
        // for favicons.
        "static_url": &state.config.static_url,
        // Audit-log path suffix. Threaded from
        // `RouteConfig::audit_url`; default `/__audit` for
        // standalone admins. Templates compose the full
        // audit URL as `{{ admin_prefix }}{{ audit_url }}`.
        "audit_url": &state.config.audit_url,
        // v0.28.2 (#77) — sidebar "Change password" link target.
        // Threaded from the tenant admin's RouteConfig.
        "change_password_url": &state.config.change_password_url,
    })
}

/// Build the sidebar context — every visible model the admin exposes,
/// grouped by Django-shape app label. Pass `active_table` so the
/// matching link gets `class="active"`.
///
/// Sidebar shape mirrors the operator console's left rail
/// (`tenancy/templates/op_layout.html`) so tenant operators see a
/// consistent navigation surface across both consoles.
pub(crate) fn sidebar_context(
    state: &AppState,
    active_table: Option<&str>,
) -> Vec<serde_json::Value> {
    let mut entries: Vec<&'static ModelEntry> = inventory_entries_dedup_by_table()
        .into_iter()
        // v0.27.7 — filter registry-scoped models out of tenant
        // admins (Org / Operator etc. don't live in the tenant
        // pool and must not surface in the tenant sidebar).
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
    let mut groups: Vec<(String, Vec<&'static ModelEntry>)> = by_app.into_iter().collect();
    groups.sort_by(|a, b| match (a.0.as_str(), b.0.as_str()) {
        ("Project", _) => std::cmp::Ordering::Greater,
        (_, "Project") => std::cmp::Ordering::Less,
        _ => a.0.cmp(&b.0),
    });

    groups
        .into_iter()
        .map(|(label, items)| {
            let models: Vec<serde_json::Value> = items
                .into_iter()
                .map(|e| {
                    serde_json::json!({
                        "name": e.schema.name,
                        "table": e.schema.table,
                        "active": active_table == Some(e.schema.table),
                    })
                })
                .collect();
            serde_json::json!({ "app": label, "models": models })
        })
        .collect()
}

/// Resolve `table` to a `ModelSchema`, but only if the admin is configured
/// to expose it. A model that exists but is filtered out via `show_only`
/// returns `None` here, which surfaces to users as a 404 — same response
/// as a genuinely missing table.
pub(crate) fn lookup_model(state: &AppState, table: &str) -> Option<&'static ModelSchema> {
    if !state.is_visible(table) {
        return None;
    }
    let entry = inventory_entries_dedup_by_table()
        .into_iter()
        .find(|e| e.schema.table == table)?;
    // v0.27.7 — apply the same scope filter the sidebar / index do
    // so a curious user typing `/__admin/rustango_orgs` directly
    // gets a 404 instead of leaking cross-tenant data via
    // search_path on schema-mode tenants.
    if !state.scope_visible(entry.schema.scope) {
        return None;
    }
    Some(entry.schema)
}

/// Build one [`Join`] per FK / O2O column on `model` whose target is
/// visible and has a display field. The join's `project` carries only
/// the target's display column — that's all the admin renders.
pub(crate) fn build_fk_joins(state: &AppState, model: &'static ModelSchema) -> Vec<Join> {
    let mut joins = Vec::new();
    for field in model.scalar_fields() {
        let Some(rel) = field.relation else { continue };
        let (to, on) = match rel {
            Relation::Fk { to, on } | Relation::O2O { to, on } => (to, on),
        };
        let Some(target) = lookup_model(state, to) else {
            continue;
        };
        let Some(display_field) = target.display_field() else {
            continue;
        };
        joins.push(Join {
            target,
            on_local: field.column,
            on_remote: on,
            // `field.name` is a valid SQL identifier and unique within the
            // model (it's a Rust struct field), so it makes a clean alias.
            alias: field.name,
            project: vec![display_field.column],
        });
    }
    joins
}

/// Walk a row set produced with `joins` set, and for each row build the
/// `(target_table, source_value_string) → display_html` map entry. Rows
/// where the joined display value is `NULL` (LEFT JOIN miss — target row
/// not present) are skipped, so `render_cell` falls back to the raw value.
pub(crate) fn fk_map_from_joined_rows(
    state: &AppState,
    model: &'static ModelSchema,
    rows: &[sqlx::postgres::PgRow],
) -> FkMap {
    let mut map: FkMap = HashMap::new();
    for field in model.scalar_fields() {
        let Some(rel) = field.relation else { continue };
        let to = match rel {
            Relation::Fk { to, .. } | Relation::O2O { to, .. } => to,
        };
        let Some(target) = lookup_model(state, to) else {
            continue;
        };
        let Some(display_field) = target.display_field() else {
            continue;
        };
        for row in rows {
            let Some(source) = render::read_value_as_string(row, field) else {
                continue;
            };
            let Some(display) = render::read_joined_value_as_html(row, field.name, display_field)
            else {
                continue;
            };
            map.insert((to.to_owned(), source), display);
        }
    }
    map
}

/// Build a `&q=…&<field>=<v>…` tail for prev/next pager URLs so the
/// active search and filters survive page navigation. Each value is
/// percent-encoded via a tiny ASCII-safe escaper good enough for the
/// admin's expected inputs.
pub(crate) fn pager_suffix(q: Option<&str>, filters: &[(&'static str, String)]) -> String {
    let mut out = String::new();
    if let Some(qs) = q {
        out.push_str("&q=");
        out.push_str(&url_encode(qs));
    }
    for (k, v) in filters {
        out.push('&');
        out.push_str(k);
        out.push('=');
        out.push_str(&url_encode(v));
    }
    out
}

/// Minimal URL-encoder for ASCII inputs. Escapes characters that have
/// special meaning in a query string. Multibyte UTF-8 is percent-encoded
/// byte-by-byte — Postgres handles the bytes the same on the way back.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        let safe = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~');
        if safe {
            out.push(byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// Render one cell. For FK columns this resolves to a link into the target
/// table; everything else delegates to [`render::render_value`].
pub(crate) fn render_cell(
    row: &sqlx::postgres::PgRow,
    field: &FieldSchema,
    fk_map: &FkMap,
) -> String {
    if let Some(rel) = field.relation {
        let to = match rel {
            Relation::Fk { to, .. } | Relation::O2O { to, .. } => to,
        };
        let Some(raw_value) = render::read_value_as_string(row, field) else {
            return "<em>NULL</em>".to_owned();
        };
        let raw_esc = render::escape(&raw_value);
        let to_esc = render::escape(to);
        return match fk_map.get(&(to.to_owned(), raw_value)) {
            Some(display) => format!(r#"<a href="/{to_esc}/{raw_esc}">{display}</a>"#),
            // Target hidden by show_only or row genuinely missing — show raw.
            None => raw_esc,
        };
    }
    render::render_value(row, field)
}

/// Render a create or edit form via the `form.html` template. Pre-fill
/// values come from `prefill` (keyed by Rust field name); pass `None` for
/// an empty create form. `pk_locked` makes the PK input read-only (edit
/// mode). `error_msg`, when present, is shown above the form.
///
/// `state` is needed so the sidebar context can be attached.
pub(crate) fn render_form(
    state: &AppState,
    model: &'static ModelSchema,
    prefill: Option<&HashMap<String, String>>,
    pk_locked: bool,
    error_msg: Option<&str>,
) -> String {
    let (action, edit_pk) = if pk_locked {
        let pk_field = model.primary_key().expect("pk_locked requires a PK");
        let pk_value = prefill
            .and_then(|m| m.get(pk_field.name).cloned())
            .unwrap_or_default();
        (
            format!("/__admin/{}/{}", model.table, render::escape(&pk_value)),
            Some(pk_value),
        )
    } else {
        (format!("/__admin/{}", model.table), None)
    };
    let title = if pk_locked {
        format!("Edit {}", model.name)
    } else {
        format!("New {}", model.name)
    };

    let admin_cfg = model
        .admin
        .copied()
        .unwrap_or(crate::core::AdminConfig::DEFAULT);

    let row_for_field = |f: &'static FieldSchema| -> serde_json::Value {
        let value = prefill
            .and_then(|m| m.get(f.name))
            .map_or("", String::as_str);
        let is_readonly_field = admin_cfg.readonly_fields.iter().any(|n| *n == f.name);
        let extra = if f.primary_key {
            " <small>(pk)</small>"
        } else if is_readonly_field {
            " <small>read-only</small>"
        } else if f.auto {
            " <small>auto</small>"
        } else if !f.nullable {
            " <small>required</small>"
        } else {
            ""
        };
        // PK is locked on edit; readonly_fields are locked on edit.
        // Auto fields are always locked — they're DB-assigned.
        let lock_input = f.auto || (pk_locked && (f.primary_key || is_readonly_field));
        serde_json::json!({
            "label": f.name,
            "extra": extra,
            "input": render::render_input(f, value, lock_input),
        })
    };

    let visible = |f: &&'static FieldSchema| -> bool {
        // Auto fields (Auto<T> PK, auto_now_add, auto_uuid, default=…
        // server-assigned columns) are hidden on create — Postgres'
        // DEFAULT fills them. On edit they are shown readonly so the
        // operator can see the value.
        if f.auto && !pk_locked {
            // Hide auto fields entirely on the create form.
            return false;
        }
        true
    };

    // Optionally group fields into fieldsets (slice 10.5). Empty
    // fieldsets means "one unnamed group with every visible field".
    let fieldsets_ctx: Vec<serde_json::Value> = if admin_cfg.fieldsets.is_empty() {
        let rows: Vec<serde_json::Value> = model
            .scalar_fields()
            .filter(visible)
            .map(row_for_field)
            .collect();
        vec![serde_json::json!({ "title": "", "rows": rows })]
    } else {
        admin_cfg
            .fieldsets
            .iter()
            .map(|set| {
                let rows: Vec<serde_json::Value> = set
                    .fields
                    .iter()
                    .filter_map(|name| model.field(name))
                    .filter(visible)
                    .map(row_for_field)
                    .collect();
                serde_json::json!({ "title": set.title, "rows": rows })
            })
            .collect()
    };

    let mut ctx = serde_json::json!({
        "model": { "name": model.name, "table": model.table },
        "title": title,
        "action": action,
        "edit_pk": edit_pk,
        "error": error_msg,
        "fieldsets": fieldsets_ctx,
    });
    super::templates::render_with_chrome(
        "form.html",
        &mut ctx,
        chrome_context(state, Some(model.table)),
    )
}
