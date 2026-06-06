//! View-side helpers shared across handlers — model lookup, FK join
//! composition, FK display-value mapping, list-cell rendering, form
//! rendering, and pager URL composition.

use std::collections::HashMap;

use crate::core::{inventory, FieldSchema, Join, ModelEntry, ModelSchema, Relation};
#[allow(unused_imports)]
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
    // #253 slice B — pick up the request-scoped AdminSession via
    // the task-local installed by `require_session`. Falling back
    // to `None` when called outside an admin request keeps
    // non-admin chrome callers (tests, hand-rendered pages) working.
    let session = super::session::current();
    chrome_context_with_session(state, active_table, session.as_ref())
}

/// As [`chrome_context`] but takes an explicit `Option<&AdminSession>`.
/// Used by tests and any path that has the session in hand directly
/// (without going through the task-local). #253 slice B.
pub(crate) fn chrome_context_with_session(
    state: &AppState,
    active_table: Option<&str>,
    session: Option<&super::session::AdminSession>,
) -> serde_json::Value {
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
        // #253 — Logout button visibility. The bare admin's
        // session middleware redirects unauthenticated requests to
        // `/login`, so by the time chrome renders we know any
        // visitor is logged in. Templates show the button when
        // `session_user` is non-null. Tenancy admins thread their
        // own session info through a parallel path; this signal is
        // only set when the bare admin's `with_session_auth` is on.
        // #253 slice B — per-user chrome info. When a request-bound
        // `AdminSession` is available the sidebar renders "Signed in
        // as <username>" + the (superuser) badge. When session auth
        // is configured but no session was threaded (e.g. older
        // callers using the bare `chrome_context`), fall back to
        // just `authenticated: true` so the Logout button still
        // renders.
        "session_user": match (session, state.config.session_secret.is_some()) {
            (Some(s), _) => serde_json::json!({
                "authenticated": true,
                "username": s.username,
                "is_superuser": s.is_superuser,
            }),
            (None, true) => serde_json::json!({ "authenticated": true }),
            (None, false) => serde_json::Value::Null,
        },
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

/// Resolve `table` to a `ModelSchema` or emit `AdminError::TableNotFound`.
/// Folds the `lookup_model(...).ok_or(AdminError::TableNotFound { table })`
/// pattern repeated across every CRUD handler (issue #562). Takes
/// `table` by reference so the caller can keep ownership for further
/// use; the error variant clones internally on the not-found path.
///
/// Use this from any admin handler that needs the model + the standard
/// 404 fallthrough. Handlers that also need the PK use
/// [`resolve_model_and_pk`] instead.
pub(crate) fn resolve_model(
    state: &AppState,
    table: &str,
) -> Result<&'static ModelSchema, crate::admin::errors::AdminError> {
    lookup_model(state, table).ok_or_else(|| crate::admin::errors::AdminError::TableNotFound {
        table: table.to_owned(),
    })
}

/// Resolve `table` to a `ModelSchema` and parse `pk_raw` against the
/// model's primary-key field. Folds the second prologue pattern that
/// recurs across every detail/edit/delete handler:
///
/// ```ignore
/// let model = lookup_model(&state, &table).ok_or(AdminError::TableNotFound { ... })?;
/// let pk_field = model.primary_key().ok_or_else(|| AdminError::Internal(...))?;
/// let pk_value = forms::parse_pk_string(pk_field, &pk_raw).map_err(AdminError::Form)?;
/// ```
///
/// Issue #562. Returns the model, the PK `FieldSchema`, and the parsed
/// `SqlValue` ready to bind in a `WHERE pk = ?` clause.
pub(crate) fn resolve_model_and_pk(
    state: &AppState,
    table: &str,
    pk_raw: &str,
) -> Result<
    (
        &'static ModelSchema,
        &'static crate::core::FieldSchema,
        crate::core::SqlValue,
    ),
    crate::admin::errors::AdminError,
> {
    let model = resolve_model(state, table)?;
    let pk_field = primary_key_or_internal(model)?;
    let pk_value = crate::forms::parse_pk_string(pk_field, pk_raw)
        .map_err(crate::admin::errors::AdminError::Form)?;
    Ok((model, pk_field, pk_value))
}

/// Return the model's `#[rustango(admin(...))]` block, falling back to
/// [`crate::core::AdminConfig::DEFAULT`] when none is declared. Folds
/// the third prologue pattern that recurs across every list / detail /
/// create / update / delete handler:
///
/// ```ignore
/// let admin_cfg = model
///     .admin
///     .copied()
///     .unwrap_or(crate::core::AdminConfig::DEFAULT);
/// ```
///
/// Issue #562 (admin CRUD-handler prologue dedup).
#[must_use]
pub(crate) fn admin_config_or_default(model: &'static ModelSchema) -> crate::core::AdminConfig {
    model
        .admin
        .copied()
        .unwrap_or(crate::core::AdminConfig::DEFAULT)
}

/// Resolve the model's primary-key `FieldSchema`, mapping the
/// `Option::None` no-PK case to [`AdminError::Internal`]. Folds the
/// fourth prologue pattern that recurs across every detail / create /
/// update / delete handler that doesn't go through
/// [`resolve_model_and_pk`] (because they don't have a `pk_raw` to
/// parse — e.g. list endpoints that just need to know the PK column
/// name for ordering).
///
/// ```ignore
/// let pk_field = model.primary_key().ok_or_else(|| {
///     AdminError::Internal(format!("model `{}` has no primary key", model.name))
/// })?;
/// ```
///
/// Issue #562 (admin CRUD-handler prologue dedup).
pub(crate) fn primary_key_or_internal(
    model: &'static ModelSchema,
) -> Result<&'static crate::core::FieldSchema, crate::admin::errors::AdminError> {
    model.primary_key().ok_or_else(|| {
        crate::admin::errors::AdminError::Internal(format!(
            "model `{}` has no primary key",
            model.name
        ))
    })
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
///
/// #352 — Django-shape `list_select_related` lets operators opt out
/// of specific FK joins (`ListSelectRelated::None` for "no joins";
/// `ListSelectRelated::Only(&[...])` for a whitelist). The default
/// `ListSelectRelated::All` preserves rustango's join-everything
/// behavior.
pub(crate) fn build_fk_joins(state: &AppState, model: &'static ModelSchema) -> Vec<Join> {
    let admin_cfg = model
        .admin
        .copied()
        .unwrap_or(crate::core::AdminConfig::DEFAULT);
    if matches!(
        admin_cfg.list_select_related,
        crate::core::ListSelectRelated::None
    ) {
        return Vec::new();
    }
    let whitelist: Option<&'static [&'static str]> = match admin_cfg.list_select_related {
        crate::core::ListSelectRelated::Only(names) => Some(names),
        _ => None,
    };
    let mut joins = Vec::new();
    for field in model.scalar_fields() {
        if let Some(allowed) = whitelist {
            if !allowed.contains(&field.name) {
                continue;
            }
        }
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
        let alias = field.name;
        joins.push(Join {
            target,
            // `field.name` is a valid SQL identifier and unique within
            // the model (it's a Rust struct field), so it makes a
            // clean alias.
            alias,
            kind: crate::core::JoinKind::Left,
            // `<main>.<fk_col> = <alias>.<target_pk>` expressed as a
            // WhereExpr now that Join's `on_local`/`on_remote` shape
            // was generalized in issue #80.
            on: crate::core::WhereExpr::ExprCompare {
                lhs: crate::core::Expr::AliasedColumn {
                    alias: model.table,
                    column: field.column,
                },
                op: crate::core::Op::Eq,
                rhs: crate::core::Expr::AliasedColumn { alias, column: on },
            },
            project: vec![display_field.column],
        });
    }
    joins
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

// #806 — was a byte-identical copy of `crate::url_codec::url_encode`;
// route through the canonical codec.
use crate::url_codec::url_encode;

/// Walk a row set produced with `joins` set, and for each row build the
/// `(target_table, source_value_string) → display_html` map entry. Tri-
/// dialect: takes a `Vec<serde_json::Value>` (one row per Value), uses
/// the dialect-agnostic `*_json` reader companions. Rows where the
/// joined display value is `NULL` (LEFT JOIN miss) are skipped so the
/// cell renderer falls back to the raw value.
pub(crate) fn fk_map_from_joined_rows_json(
    state: &AppState,
    model: &'static ModelSchema,
    rows: &[serde_json::Value],
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
            let Some(source) = render::read_value_as_string_json(row, field) else {
                continue;
            };
            let Some(display) =
                render::read_joined_value_as_html_json(row, field.name, display_field)
            else {
                continue;
            };
            map.insert((to.to_owned(), source), display);
        }
    }
    map
}

/// v0.37 — JSON-bridge counterpart of [`render_cell`]. Same FK-link
/// resolution logic, but reads the row through `serde_json::Value`
/// so it compiles + runs against any backend.
pub(crate) fn render_cell_json(
    row: &serde_json::Value,
    field: &FieldSchema,
    fk_map: &FkMap,
) -> String {
    if let Some(rel) = field.relation {
        let to = match rel {
            Relation::Fk { to, .. } | Relation::O2O { to, .. } => to,
        };
        let Some(raw_value) = render::read_value_as_string_json(row, field) else {
            return "<em>NULL</em>".to_owned();
        };
        let raw_esc = render::escape(&raw_value);
        let to_esc = render::escape(to);
        return match fk_map.get(&(to.to_owned(), raw_value)) {
            Some(display) => format!(r#"<a href="/{to_esc}/{raw_esc}">{display}</a>"#),
            None => raw_esc,
        };
    }
    render::render_value_json(row, field)
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
    render_form_with_inlines_and_pickers(
        state,
        model,
        prefill,
        pk_locked,
        error_msg,
        Vec::new(),
        &[],
    )
}

#[allow(clippy::too_many_arguments)]
fn render_form_with_inlines_and_pickers(
    state: &AppState,
    model: &'static ModelSchema,
    prefill: Option<&HashMap<String, String>>,
    pk_locked: bool,
    error_msg: Option<&str>,
    inline_panels: Vec<super::inlines::InlineFormPanel>,
    gfk_picker_cts: &[crate::contenttypes::ContentType],
) -> String {
    // v0.31.1 (#5): respect `state.config.admin_prefix` instead of
    // hardcoding `/__admin`. Apps on the v0.29+ friendly default
    // (`/admin`) were getting form-action URLs that 404'd.
    let admin_prefix = state.config.admin_prefix.as_str();
    let (action, edit_pk) = if pk_locked {
        let pk_field = model.primary_key().expect("pk_locked requires a PK");
        let pk_value = prefill
            .and_then(|m| m.get(pk_field.name).cloned())
            .unwrap_or_default();
        (
            format!(
                "{admin_prefix}/{}/{}",
                model.table,
                render::escape(&pk_value)
            ),
            Some(pk_value),
        )
    } else {
        (format!("{admin_prefix}/{}", model.table), None)
    };
    let title = if pk_locked {
        format!("Edit {}", model.display_label())
    } else {
        format!("New {}", model.display_label())
    };

    let admin_cfg = model
        .admin
        .copied()
        .unwrap_or(crate::core::AdminConfig::DEFAULT);

    // #244 — collect every `generic_fk(...)` `ct_column` so the row
    // closure can swap a raw integer input for a ContentType `<select>`
    // when that column is being rendered. Empty when the model has no
    // `generic_fk` declarations OR the caller didn't pre-load the CT
    // list — both cases fall through to `render_input`'s default.
    let gfk_ct_columns: std::collections::HashSet<&'static str> = if gfk_picker_cts.is_empty() {
        std::collections::HashSet::new()
    } else {
        model
            .generic_relations
            .iter()
            .map(|gr| gr.ct_column)
            .collect()
    };

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
        } else if gfk_ct_columns.contains(f.column) {
            " <small>generic FK</small>"
        } else if !f.nullable {
            " <small>required</small>"
        } else {
            ""
        };
        // PK is locked on edit; readonly_fields are locked on edit.
        // Auto fields are always locked — they're DB-assigned.
        let lock_input = f.auto || (pk_locked && (f.primary_key || is_readonly_field));
        // #359 — Django-shape `formfield_overrides`. Look up a
        // per-field widget override from the AdminConfig before
        // dispatching to the FieldType default. Unknown names fall
        // back automatically — `render_input_with_widget` logs the
        // warning.
        let widget_override = admin_cfg
            .formfield_overrides
            .iter()
            .find(|(name, _)| *name == f.name)
            .map(|(_, widget)| *widget);
        // #244 — swap raw integer input for a ContentType `<select>`
        // on fields named as a `generic_fk` ct_column.
        let mut input_html = if gfk_ct_columns.contains(f.column) {
            render::render_gfk_select(f, value, lock_input, gfk_picker_cts)
        } else {
            render::render_input_with_widget(f, value, lock_input, widget_override)
        };
        // #357 — Django-shape `raw_id_fields`. When the field is an
        // FK / O2O AND is named in `admin.raw_id_fields`, append a
        // magnifying-glass lookup link that points at the target
        // model's admin list view. Lets the operator find the right
        // PK to type without scrolling through every option.
        if admin_cfg.raw_id_fields.iter().any(|n| *n == f.name) {
            if let Some(rel) = f.relation {
                let target_table = match rel {
                    crate::core::Relation::Fk { to, .. }
                    | crate::core::Relation::O2O { to, .. } => to,
                };
                let lookup_url = format!("{}/{}", admin_prefix, render::escape(target_table));
                use std::fmt::Write as _;
                let _ = write!(
                    input_html,
                    r#" <a class="raw-id-lookup" href="{lookup_url}" target="_blank" rel="noopener" title="Look up {label}">🔍</a>"#,
                    label = render::escape(f.display_label()),
                );
            }
        }
        // #358 — Django-shape `autocomplete_fields`. Append a
        // `<datalist>` with `id="<field>_options"`, set the input's
        // `list=` attribute, and emit a tiny inline JS block that
        // populates the datalist via fetch to the target's
        // `__autocomplete` endpoint on every input event.
        if admin_cfg.autocomplete_fields.iter().any(|n| *n == f.name) {
            if let Some(rel) = f.relation {
                let target_table = match rel {
                    crate::core::Relation::Fk { to, .. }
                    | crate::core::Relation::O2O { to, .. } => to,
                };
                let escaped_target = render::escape(target_table);
                let escaped_name = render::escape(f.name);
                let datalist_id = format!("{escaped_name}_options");
                // Inject `list="<id>"` onto the existing input HTML.
                // The `name="…"` attribute is unique within the form
                // so a single substitution is unambiguous.
                let needle = format!(r#"name="{escaped_name}""#);
                let replacement =
                    format!(r#"name="{escaped_name}" list="{datalist_id}" autocomplete="off""#);
                input_html = input_html.replacen(&needle, &replacement, 1);
                use std::fmt::Write as _;
                let _ = write!(
                    input_html,
                    concat!(
                        r#" <datalist id="{datalist}"></datalist>"#,
                        r#"<script>(function(){{"#,
                        r#"  var inp=document.querySelector('input[name="{name}"]');"#,
                        r#"  if(!inp)return;"#,
                        r#"  var dl=document.getElementById('{datalist}');"#,
                        r#"  var url='{prefix}/{target}/__autocomplete';"#,
                        r#"  function refresh(){{"#,
                        r#"    fetch(url+'?q='+encodeURIComponent(inp.value)).then(function(r){{return r.json();}}).then(function(j){{"#,
                        r#"      dl.innerHTML=(j.results||[]).map(function(o){{return '<option value=\"'+o.id+'\">'+(o.text||o.id)+'</option>';}}).join('');"#,
                        r#"    }}).catch(function(){{}});"#,
                        r#"  }}"#,
                        r#"  inp.addEventListener('input',refresh);"#,
                        r#"  inp.addEventListener('focus',refresh);"#,
                        r#"}})();</script>"#,
                    ),
                    datalist = datalist_id,
                    name = escaped_name,
                    prefix = render::escape(admin_prefix),
                    target = escaped_target,
                );
            }
        }
        serde_json::json!({
            "label": f.display_label(),
            "extra": extra,
            "input": input_html,
            // Django-shape `help_text` (#admin-helptext) — short
            // caption rendered under the input. `None` means no
            // caption; template treats it as falsy and renders nothing.
            "help_text": f.help_text,
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
        // #449 — Django-shape `editable = false` removes the field
        // from the auto-generated change-form entirely. The value
        // is still visible on list / detail views (those don't
        // route through this filter).
        if !f.editable {
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

    let inline_form_panels_ctx: Vec<serde_json::Value> = inline_panels
        .into_iter()
        .map(|p| serde_json::to_value(p).unwrap_or(serde_json::Value::Null))
        .collect();

    // #356 — Django-shape `prepopulated_fields`. Build the
    // `{ target_input_name: [source_input_name, …] }` map the
    // form.html JS reads to wire change events. We translate Rust
    // field names → HTML input `name=` (which == Rust field name in
    // rustango admin today — no `form_id-` prefix is applied to the
    // top-level form). Entries pointing at unknown fields are
    // silently dropped so a stale model attr can't break the form.
    let prepopulated_ctx: Vec<serde_json::Value> = admin_cfg
        .prepopulated_fields
        .iter()
        .filter_map(|p| {
            let target = model.field(p.target)?;
            // Skip target if it isn't editable on this form (auto, PK
            // locked on edit, or `editable = false`).
            if !target.editable || target.auto {
                return None;
            }
            let sources: Vec<&str> = p
                .sources
                .iter()
                .filter_map(|src| model.field(src).map(|f| f.name))
                .collect();
            if sources.is_empty() {
                return None;
            }
            Some(serde_json::json!({
                "target": target.name,
                "sources": sources,
            }))
        })
        .collect();

    let mut ctx = serde_json::json!({
        "model": {
            "name": model.name,
            "table": model.table,
            "label": model.display_label(),
            "label_plural": model.display_label_plural(),
        },
        "title": title,
        "action": action,
        "edit_pk": edit_pk,
        "error": error_msg,
        "fieldsets": fieldsets_ctx,
        "inline_form_panels": inline_form_panels_ctx,
        "prepopulated_fields": prepopulated_ctx,
        // Only emit the slug script when not editing — Django's
        // semantic is "stop populating once the value is set" and a
        // stored slug usually wants to remain stable. The form has
        // a `prepopulated_active` flag the template can branch on.
        "prepopulated_active": !pk_locked && !prepopulated_ctx.is_empty(),
    });
    super::templates::render_with_chrome(
        "form.html",
        &mut ctx,
        chrome_context(state, Some(model.table)),
    )
}

/// As [`render_form`] but threads a list of `InlineFormPanel` and a
/// pre-loaded `ContentType` list into the form context. The first
/// drives inline panel rendering (#50, slice 2); the second drives
/// the `generic_fk` `<select>` picker (#244). Used by `edit_form` and
/// `create_form` — pass an empty `inline_panels` from create-form
/// (Django's create-form-doesn't-render-inlines behavior).
pub(crate) fn render_form_with_inlines_and_picker(
    state: &AppState,
    model: &'static ModelSchema,
    prefill: Option<&HashMap<String, String>>,
    pk_locked: bool,
    error_msg: Option<&str>,
    inline_panels: Vec<super::inlines::InlineFormPanel>,
    gfk_picker_cts: &[crate::contenttypes::ContentType],
) -> String {
    render_form_with_inlines_and_pickers(
        state,
        model,
        prefill,
        pk_locked,
        error_msg,
        inline_panels,
        gfk_picker_cts,
    )
}
