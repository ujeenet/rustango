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

/// The hidden CSRF field for this request's token; empty outside a request.
pub(crate) fn csrf_input_for(csrf_token: &str) -> String {
    if csrf_token.is_empty() {
        String::new()
    } else {
        crate::forms::csrf::csrf_input_html(csrf_token)
    }
}

/// The hidden CSRF field for the current request.
#[cfg(feature = "totp")]
pub(crate) fn current_csrf_input() -> String {
    csrf_input_for(&super::session::current_csrf_token().unwrap_or_default())
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
    // #1395 — every admin template's POST form renders `csrf_input`, and
    // this is the one place all of them get their variables from. The
    // token comes from the request-scoped task-local installed by
    // `csrf_context`; outside a request there is none, and the empty
    // string is the honest value. That case is a hand-rendered page or a
    // test, neither of which submits anything — and enforcement never
    // depends on the template, because `CsrfLayer` rejects an unsafe
    // request whatever was rendered.
    let csrf_token = super::session::current_csrf_token().unwrap_or_default();
    let csrf_input = csrf_input_for(&csrf_token);

    serde_json::json!({
        "csrf_token": csrf_token,
        "csrf_input": csrf_input,
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
        // Hides the Activity links from users the feed would refuse.
        "can_view_audit": state.audit_reader().is_some(),
        // v0.28.2 (#77) — sidebar "Change password" link target.
        // Threaded from the tenant admin's RouteConfig.
        "change_password_url": state.config.change_password_url.clone().or_else(|| {
            state.config.session_secret.as_ref()
                .map(|_| format!("{}/account/password", state.config.admin_prefix))
        }),
        // Sidebar Logout POST target. Defaults to `{admin_prefix}/logout`
        // (the bare admin's own route); the tenant admin overrides it to
        // its RouteConfig logout_url (handled at the tenancy layer), so
        // the button doesn't POST to a non-existent `{prefix}/logout`.
        "logout_url": state.config.logout_url.clone().unwrap_or_else(
            || format!("{}/logout", state.config.admin_prefix)
        ),
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
                "impersonated_by": s.impersonated_by,
            }),
            (None, true) => serde_json::json!({ "authenticated": true }),
            (None, false) => serde_json::Value::Null,
        },
    })
}

/// Build the sidebar context — every visible model the admin exposes,
/// grouped by app label. Pass `active_table` so the matching link
/// gets `class="active"`.
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

/// `true` for a column with a `password` widget override. Its value is
/// never echoed: forms render it empty, lists and detail show only whether it is set.
#[must_use]
pub(crate) fn is_secret_field(admin_cfg: &crate::core::AdminConfig, name: &str) -> bool {
    admin_cfg
        .formfield_overrides
        .iter()
        .any(|(f, w)| *f == name && *w == "password")
}

/// Columns `?q=` searches, for the list and autocomplete alike:
/// `search_fields` when set, else the searchable fields. Never a secret (#2228).
/// "Secret" means the `password` widget: a `token` field without it stays searchable.
#[must_use]
pub(crate) fn search_columns(
    model: &'static ModelSchema,
    admin_cfg: &crate::core::AdminConfig,
) -> Vec<&'static str> {
    let fields: Vec<&'static FieldSchema> = if admin_cfg.search_fields.is_empty() {
        model.searchable_fields().collect()
    } else {
        admin_cfg
            .search_fields
            .iter()
            .filter_map(|name| model.field(name))
            .collect()
    };
    fields
        .into_iter()
        .filter(|f| !is_secret_field(admin_cfg, f.name))
        .map(|f| f.column)
        .collect()
}

/// `true` when the list may filter on `field` from the URL: a
/// `list_filter`, displayed or FK column, or an inline's parent pin.
/// Never a secret, so a URL cannot probe its value (#2031).
/// Every FK column is allowed on purpose: inline and facet links filter on it.
#[must_use]
pub(crate) fn url_filterable(
    model: &'static ModelSchema,
    admin_cfg: &crate::core::AdminConfig,
    field: &FieldSchema,
) -> bool {
    if is_secret_field(admin_cfg, field.name) {
        return false;
    }
    let named = |names: &[&str]| names.contains(&field.name);
    named(admin_cfg.list_filter)
        || named(admin_cfg.list_display)
        || admin_cfg.list_display.is_empty()
        || field.relation.is_some()
        || super::inlines::is_parent_pin(model.table, field.column)
}

/// A secret column's list/detail cell: whether it is set, never the value.
pub(crate) fn render_secret_cell(row: &serde_json::Value, field: &FieldSchema) -> String {
    render_secret_value(row.get(field.name))
}

/// [`render_secret_cell`] for an already-read value.
pub(crate) fn render_secret_value(value: Option<&serde_json::Value>) -> String {
    let set = value
        .and_then(serde_json::Value::as_str)
        .is_some_and(|v| !v.is_empty());
    if set {
        "<em>set</em>"
    } else {
        "<em>not set</em>"
    }
    .to_owned()
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
    served_entry(table).map(|e| e.schema)
}

/// The entry the admin renders for `table`: the richest one, as in
/// [`inventory_entries_dedup_by_table`].
pub(crate) fn served_entry(table: &str) -> Option<&'static ModelEntry> {
    inventory::iter::<ModelEntry>
        .into_iter()
        .filter(|e| e.schema.table == table)
        .reduce(|best, e| {
            if e.schema.fields.len() > best.schema.fields.len() {
                e
            } else {
                best
            }
        })
}

/// An FK / O2O column whose cell shows the target's display name.
pub(crate) struct FkDisplay {
    pub(crate) field: &'static FieldSchema,
    /// The relation's `to`, the key [`FkMap`] uses.
    pub(crate) to: &'static str,
    pub(crate) target: &'static ModelSchema,
    pub(crate) on: &'static str,
    pub(crate) display_field: &'static FieldSchema,
}

/// The FK columns of `model` that show a name: the target is visible,
/// has a display field, and `list_select_related` lets it join.
pub(crate) fn fk_display_targets(state: &AppState, model: &'static ModelSchema) -> Vec<FkDisplay> {
    let admin_cfg = admin_config_or_default(model);
    let whitelist: Option<&'static [&'static str]> = match admin_cfg.list_select_related {
        crate::core::ListSelectRelated::None => return Vec::new(),
        crate::core::ListSelectRelated::Only(names) => Some(names),
        _ => None,
    };
    model
        .scalar_fields()
        .filter(|f| whitelist.map_or(true, |allowed| allowed.contains(&f.name)))
        .filter_map(|field| {
            let (to, on) = match field.relation? {
                Relation::Fk { to, on } | Relation::O2O { to, on } => (to, on),
            };
            let target = lookup_model(state, to)?;
            Some(FkDisplay {
                field,
                to,
                target,
                on,
                display_field: target.display_field()?,
            })
        })
        .collect()
}

/// Build one [`Join`] per [`fk_display_targets`] entry. The join's
/// `project` carries only the target's display column.
///
/// `list_select_related` lets operators opt out of specific FK
/// joins (`ListSelectRelated::None` for "no joins";
/// `ListSelectRelated::Only(&[...])` for a whitelist).
///
/// A target row outside its own queryset hooks joins nothing, so its
/// display name stays hidden (#2080). A target with a "view" hook is not
/// joined: its names go through the hook instead (#2267).
pub(crate) fn build_fk_joins(
    state: &AppState,
    model: &'static ModelSchema,
    parts: &axum::http::request::Parts,
) -> Vec<Join> {
    fk_display_targets(state, model)
        .into_iter()
        .filter(|t| !super::object_permissions::has_hook(t.target.table, "view"))
        .map(|t| {
            // `field.name` is a valid SQL identifier and unique within
            // the model (it's a Rust struct field), so it makes a
            // clean alias.
            let alias = t.field.name;
            Join {
                target: t.target,
                alias,
                kind: crate::core::JoinKind::Left,
                // Bare scope columns in the ON resolve to the joined alias.
                on: super::queryset_hooks::RowScope::of(t.target, parts).constrain(
                    crate::core::WhereExpr::ExprCompare {
                        lhs: crate::core::Expr::AliasedColumn {
                            alias: model.table,
                            column: t.field.column,
                        },
                        op: crate::core::Op::Eq,
                        rhs: crate::core::Expr::AliasedColumn {
                            alias,
                            column: t.on,
                        },
                    },
                ),
                project: vec![t.display_field.column],
            }
        })
        .collect()
}

/// A list page's URL: its base path plus the filter state as query
/// pairs. Every link on the page derives from one value, so a pager,
/// facet or date link cannot keep a different subset (#1916).
#[derive(Clone, Debug)]
pub(crate) struct ListQuery {
    base: String,
    params: Vec<(String, String)>,
}

impl ListQuery {
    /// `base` is the full path, admin prefix included.
    pub(crate) fn new(base: String) -> Self {
        Self {
            base,
            params: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.params.push((key.into(), value.into()));
    }

    /// A copy without `keys`.
    #[must_use]
    pub(crate) fn without(&self, keys: &[&str]) -> Self {
        Self {
            base: self.base.clone(),
            params: self
                .params
                .iter()
                .filter(|(k, _)| !keys.contains(&k.as_str()))
                .cloned()
                .collect(),
        }
    }

    /// This query with `key=value` added.
    #[must_use]
    pub(crate) fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.push(key, value);
        self
    }

    pub(crate) fn pairs(&self) -> &[(String, String)] {
        &self.params
    }

    pub(crate) fn url(&self) -> String {
        if self.params.is_empty() {
            return self.base.clone();
        }
        format!("{}?{}", self.base, &self.suffix()[1..])
    }

    /// `&k=v…` for templates that write `?page=N` themselves.
    pub(crate) fn suffix(&self) -> String {
        let mut out = String::new();
        for (k, v) in &self.params {
            out.push('&');
            out.push_str(&url_encode(k));
            out.push('=');
            out.push_str(&url_encode(v));
        }
        out
    }
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
    admin_prefix: &str,
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
            Some(display) => format!(
                r#"<a href="{prefix}/{to_esc}/{raw_esc}">{display}</a>"#,
                prefix = render::escape(admin_prefix),
            ),
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

/// A new form preselects the model default; only a nullable bool can
/// show "false" apart from blank (NULL).
fn new_form_bool_value(f: &FieldSchema) -> &'static str {
    match f.default.map(|d| d.trim_matches('\'').to_ascii_lowercase()) {
        Some(d) if d == "true" || d == "1" => "true",
        Some(d) if f.nullable && (d == "false" || d == "0") => "false",
        _ => "",
    }
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
    let locked = state.locked_fields(model, if pk_locked { "change" } else { "add" });

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
        let is_secret = is_secret_field(&admin_cfg, f.name);
        let value = match prefill.and_then(|m| m.get(f.name)) {
            _ if is_secret => "",
            Some(v) => v.as_str(),
            None if prefill.is_none() && f.ty == crate::core::FieldType::Bool => {
                new_form_bool_value(f)
            }
            None => "",
        };
        let is_readonly_field = locked.contains(&f.name);
        let extra = if f.primary_key {
            " <small>(pk)</small>"
        } else if is_readonly_field {
            " <small>read-only</small>"
        } else if is_secret && pk_locked {
            " <small>leave empty to keep</small>"
        } else if f.auto {
            " <small>auto</small>"
        } else if gfk_ct_columns.contains(f.column) {
            " <small>generic FK</small>"
        } else if !f.nullable {
            " <small>required</small>"
        } else {
            ""
        };
        // PK is locked on edit. Auto and readonly fields are always
        // locked: the server never reads them from the form.
        let lock_input = f.auto || is_readonly_field || (pk_locked && f.primary_key);
        // `formfield_overrides`: look up a per-field widget override
        // from the AdminConfig before dispatching to the FieldType
        // default. Unknown names fall back automatically —
        // `render_input_with_widget` logs the warning.
        let widget_override = admin_cfg
            .formfield_overrides
            .iter()
            .find(|(name, _)| *name == f.name)
            .map(|(_, widget)| *widget);
        // #244 — swap raw integer input for a ContentType `<select>`
        // on fields named as a `generic_fk` ct_column.
        let mut input_html = if gfk_ct_columns.contains(f.column) {
            render::render_gfk_select(f, value, lock_input, gfk_picker_cts)
        } else if is_secret {
            // Required only on create: an empty edit keeps the stored value.
            render::render_secret_input(f, lock_input, !pk_locked)
        } else {
            render::render_input_with_widget(f, value, lock_input, widget_override)
        };
        // `raw_id_fields`: when the field is an FK / O2O and is named
        // in `admin.raw_id_fields`, append a magnifying-glass lookup
        // link that points at the target model's admin list view. It
        // lets the operator find the right PK to type without
        // scrolling through every option.
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
        // `autocomplete_fields`: append a `<datalist>` with
        // `id="<field>_options"`, set the input's
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
                        r#"<script{nonce}>(function(){{"#,
                        r#"  var inp=document.querySelector('input[name="{name}"]');"#,
                        r#"  if(!inp)return;"#,
                        r#"  var dl=document.getElementById('{datalist}');"#,
                        r#"  var url='{prefix}/{target}/__autocomplete';"#,
                        r#"  function refresh(){{"#,
                        r#"    fetch(url+'?q='+encodeURIComponent(inp.value)).then(function(r){{return r.json();}}).then(function(j){{"#,
                        // Text nodes, never innerHTML: `text` is row data (#2144).
                        r#"      dl.replaceChildren.apply(dl,(j.results||[]).map(function(o){{var op=document.createElement('option');op.value=o.id;op.textContent=o.text||o.id;return op;}}));"#,
                        r#"    }}).catch(function(){{}});"#,
                        r#"  }}"#,
                        r#"  inp.addEventListener('input',refresh);"#,
                        r#"  inp.addEventListener('focus',refresh);"#,
                        r#"}})();</script>"#,
                    ),
                    datalist = datalist_id,
                    nonce = crate::csp_nonce::nonce_attr(),
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
            // `help_text` — short caption rendered under the input.
            // `None` means no caption; the template treats it as
            // falsy and renders nothing.
            "help_text": f.help_text,
        })
    };

    let fieldsets_ctx: Vec<serde_json::Value> = FormLayout::of(model, &admin_cfg, pk_locked)
        .groups
        .into_iter()
        .map(|(title, fields)| {
            let rows: Vec<serde_json::Value> = fields.into_iter().map(row_for_field).collect();
            serde_json::json!({ "title": title, "rows": rows })
        })
        .collect();

    let inline_form_panels_ctx: Vec<serde_json::Value> = inline_panels
        .into_iter()
        .map(|p| serde_json::to_value(p).unwrap_or(serde_json::Value::Null))
        .collect();

    // `prepopulated_fields`: build the
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
        // Only emit the slug script when not editing: stop
        // populating once the value is set, because a stored slug
        // usually wants to stay stable. The form has
        // a `prepopulated_active` flag the template can branch on.
        "prepopulated_active": !pk_locked && !prepopulated_ctx.is_empty(),
    });
    super::templates::render_with_chrome(
        "form.html",
        &mut ctx,
        chrome_context(state, Some(model.table)),
    )
}

/// The fields the admin form renders, grouped by fieldset. The renderer
/// and both submit handlers read it, so a POST can only write what the
/// form shows.
pub(crate) struct FormLayout {
    /// `(fieldset title, fields)`; one untitled group without `fieldsets`.
    pub(crate) groups: Vec<(&'static str, Vec<&'static FieldSchema>)>,
}

impl FormLayout {
    /// `pk_locked` is the edit form, which shows auto fields read-only.
    pub(crate) fn of(
        model: &'static ModelSchema,
        admin_cfg: &crate::core::AdminConfig,
        pk_locked: bool,
    ) -> Self {
        // `editable = false` fields never render; auto fields only on edit.
        let visible = |f: &&'static FieldSchema| f.editable && (pk_locked || !f.auto);
        let groups = if admin_cfg.fieldsets.is_empty() {
            vec![("", model.scalar_fields().filter(visible).collect())]
        } else {
            admin_cfg
                .fieldsets
                .iter()
                .map(|set| {
                    let fields = set
                        .fields
                        .iter()
                        .filter_map(|name| model.field(name))
                        .filter(visible)
                        .collect();
                    (set.title, fields)
                })
                .collect()
        };
        Self { groups }
    }

    /// Scalar fields the form does not render; submit handlers skip them.
    /// Auto fields stay with the writers, which assign them server-side.
    pub(crate) fn unrendered(&self, model: &'static ModelSchema) -> Vec<&'static str> {
        model
            .scalar_fields()
            .filter(|f| {
                !f.auto
                    && !self
                        .groups
                        .iter()
                        .any(|(_, g)| g.iter().any(|r| r.name == f.name))
            })
            .map(|f| f.name)
            .collect()
    }
}

/// As [`render_form`] but threads a list of `InlineFormPanel` and a
/// pre-loaded `ContentType` list into the form context. The first
/// drives inline panel rendering (#50, slice 2); the second drives
/// the `generic_fk` `<select>` picker (#244). Used by `edit_form` and
/// `create_form` — pass an empty `inline_panels` from create-form,
/// which does not render inlines.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::FieldType;

    /// List and detail FK cells link under the mounted prefix (#1916).
    #[test]
    fn fk_cell_links_under_the_admin_prefix() {
        let mut f = FieldSchema::new("author", "author_id", FieldType::I64);
        f.relation = Some(Relation::Fk {
            to: "author",
            on: "id",
        });
        let row = serde_json::json!({ "author": 7 });
        let mut fk_map = FkMap::new();
        fk_map.insert(("author".into(), "7".into()), "Ann".into());
        assert_eq!(
            render_cell_json(&row, &f, &fk_map, "/adm"),
            r#"<a href="/adm/author/7">Ann</a>"#
        );
    }

    #[test]
    fn new_form_bool_preselects_the_default() {
        let mut f = FieldSchema::new("flag", "flag", FieldType::Bool);
        f.nullable = true;
        f.default = Some("false");
        assert_eq!(new_form_bool_value(&f), "false");
        f.default = Some("0");
        assert_eq!(new_form_bool_value(&f), "false");
        f.default = None;
        assert_eq!(new_form_bool_value(&f), "");
        f.default = Some("TRUE");
        assert_eq!(new_form_bool_value(&f), "true");
        f.nullable = false;
        f.default = Some("false");
        assert_eq!(new_form_bool_value(&f), "");
    }

    #[test]
    fn list_query_drops_and_adds_keys() {
        let mut q = ListQuery::new("/adm/t".into());
        q.push("q", "a b");
        q.push("year", "2024");
        assert_eq!(q.url(), "/adm/t?q=a%20b&year=2024");
        assert_eq!(q.without(&["q", "year"]).url(), "/adm/t");
        assert_eq!(q.without(&["year"]).with("k", "v").suffix(), "&q=a%20b&k=v");
    }
}
