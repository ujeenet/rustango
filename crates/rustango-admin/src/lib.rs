//! Auto-generated CRUD admin for rustango models.
//!
//! Walks the `inventory` registry every `#[derive(Model)]` populates and
//! serves an axum [`Router`] over it — no per-model code required.
//!
//! ```ignore
//! use rustango::{migrate, admin};
//! use rustango::sql::sqlx::PgPool;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let pool = PgPool::connect(&std::env::var("DATABASE_URL")?).await?;
//!     migrate::apply_all(&pool).await?;
//!
//!     let app = admin::router(pool);
//!     let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
//!     axum::serve(listener, app).await?;
//!     Ok(())
//! }
//! ```
//!
//! Routes:
//! * `GET  /`                          — list every registered model
//! * `GET  /<table>`                   — list rows
//! * `GET  /<table>/new`               — create form
//! * `POST /<table>`                   — submit create
//! * `GET  /<table>/<pk>`              — detail view
//! * `GET  /<table>/<pk>/edit`         — edit form (PK readonly)
//! * `POST /<table>/<pk>`              — submit edit
//! * `POST /<table>/<pk>/delete`       — submit delete

mod auth;
mod forms;
mod render;

pub use auth::protect_with_basic_auth;

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::{Arc, OnceLock};

use axum::extract::{Form, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Json;
use axum::Router;
use rustango_core::{
    inventory, CountQuery, DeleteQuery, FieldSchema, Filter, InsertQuery, Join, ModelEntry,
    ModelSchema, Op, Relation, SearchClause, SelectQuery, SqlValue, UpdateQuery, WhereExpr,
};
use rustango_sql::sqlx::{self, PgPool};

use forms::FormError;

/// Lazily-initialized Tera registry holding the four bundled templates.
/// `include_str!` baked into the binary at compile time → no runtime
/// filesystem dependency.
fn templates() -> &'static tera::Tera {
    static T: OnceLock<tera::Tera> = OnceLock::new();
    T.get_or_init(|| {
        let mut tera = tera::Tera::default();
        tera.add_raw_templates([
            ("base.html", include_str!("../templates/base.html")),
            ("index.html", include_str!("../templates/index.html")),
            ("list.html", include_str!("../templates/list.html")),
            ("detail.html", include_str!("../templates/detail.html")),
            ("form.html", include_str!("../templates/form.html")),
        ])
        .expect("admin templates compile");
        tera
    })
}

/// Render a bundled template with a serde-serializable context. Panics
/// if the template is missing or the context fails to serialize — both
/// are programmer bugs caught in tests.
fn render(template: &str, ctx: &serde_json::Value) -> String {
    let tera_ctx = tera::Context::from_serialize(ctx).expect("admin context serializes");
    templates()
        .render(template, &tera_ctx)
        .expect("admin template renders")
}

/// Mount the admin under any prefix using axum's nesting:
/// `Router::new().nest("/admin", rustango_admin::router(pool))`.
///
/// Equivalent to `Builder::new(pool).build()`. For finer control (model
/// allowlist, read-only tables) use [`Builder`].
pub fn router(pool: PgPool) -> Router {
    Builder::new(pool).build()
}

/// Configurable admin builder.
///
/// ```ignore
/// let app = admin::Builder::new(pool)
///     .show_only(["user", "post", "audit_log"])
///     .read_only(["audit_log"])
///     .build();
/// ```
#[must_use]
pub struct Builder {
    pool: PgPool,
    config: Config,
}

#[derive(Clone, Default)]
pub(crate) struct Config {
    /// Tables visible in the admin. `None` = every registered model.
    pub(crate) allowed_tables: Option<HashSet<String>>,
    /// Tables whose mutating routes are blocked and whose write-buttons
    /// are hidden in HTML.
    pub(crate) read_only_tables: HashSet<String>,
    /// Global read-only mode — when true, **every** visible table is
    /// treated as read-only regardless of `read_only_tables`. Used by
    /// `rustango-tenancy` to gate non-superuser tenant users without
    /// having to enumerate every table at request time.
    pub(crate) read_only_all: bool,
}

impl Builder {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            config: Config::default(),
        }
    }

    /// Restrict the admin to these tables. Models not in the list are
    /// hidden from the index and return 404 on direct hits.
    pub fn show_only<I, S>(mut self, tables: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.config.allowed_tables = Some(tables.into_iter().map(Into::into).collect());
        self
    }

    /// Mark these tables read-only. List/detail still render; create,
    /// edit, and delete routes return 403, and the corresponding buttons
    /// are hidden in the HTML.
    pub fn read_only<I, S>(mut self, tables: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.config
            .read_only_tables
            .extend(tables.into_iter().map(Into::into));
        self
    }

    /// Mark **every** table read-only — the admin renders list/detail
    /// views but every mutating route returns 403 and write-buttons
    /// are hidden. Used by callers (e.g. `rustango-tenancy` for
    /// non-superuser tenant users) that gate by a runtime flag and
    /// don't want to enumerate every table per request.
    pub fn read_only_all(mut self) -> Self {
        self.config.read_only_all = true;
        self
    }

    pub fn build(self) -> Router {
        Router::new()
            .route("/", get(index))
            .route("/{table}", get(table_view).post(create_submit))
            .route("/{table}/new", get(create_form))
            .route("/{table}/{pk}", get(detail_view).post(update_submit))
            .route("/{table}/{pk}/edit", get(edit_form))
            .route("/{table}/{pk}/delete", post(delete_submit))
            .with_state(AppState {
                pool: self.pool,
                config: Arc::new(self.config),
            })
    }
}

#[derive(Clone)]
struct AppState {
    pool: PgPool,
    config: Arc<Config>,
}

impl AppState {
    fn is_visible(&self, table: &str) -> bool {
        self.config
            .allowed_tables
            .as_ref()
            .is_none_or(|allowed| allowed.contains(table))
    }

    fn is_read_only(&self, table: &str) -> bool {
        self.config.read_only_all || self.config.read_only_tables.contains(table)
    }
}

// ============================================================== INDEX

async fn index(State(state): State<AppState>) -> Html<String> {
    let mut models: Vec<&'static ModelSchema> = inventory::iter::<ModelEntry>
        .into_iter()
        .map(|e| e.schema)
        .filter(|m| state.is_visible(m.table))
        .collect();
    models.sort_by_key(|m| m.name);

    let models_ctx: Vec<serde_json::Value> = models
        .into_iter()
        .map(|m| {
            serde_json::json!({
                "name": m.name,
                "table": m.table,
                "field_count": m.scalar_fields().count(),
            })
        })
        .collect();
    Html(render(
        "index.html",
        &serde_json::json!({ "models": models_ctx }),
    ))
}

// ============================================================== LIST

const PAGE_SIZE: i64 = 50;

/// Reserved query parameters; everything else is treated as a per-field filter.
const RESERVED_PARAMS: &[&str] = &["page", "q"];

#[allow(clippy::too_many_lines)] // mostly linear HTML emission; splitting hurts readability
async fn table_view(
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
    let total = rustango_sql::count_rows(
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
    let rows = rustango_sql::select_rows(
        &state.pool,
        &SelectQuery {
            model,
            where_clause,
            search: search.clone(),
            joins,
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

    Ok(Html(render(
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

async fn detail_view(
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

    let row = rustango_sql::select_one_row(
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
    let html = render(
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

async fn create_form(
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

async fn create_submit(
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
    if let Err(e) = rustango_sql::insert(&state.pool, &query).await {
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

async fn edit_form(
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

    let row = rustango_sql::select_one_row(
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

async fn update_submit(
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
    let assignments: Vec<rustango_core::Assignment> = collected
        .into_iter()
        .map(|(column, value)| rustango_core::Assignment { column, value })
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
    if let Err(e) = rustango_sql::update(&state.pool, &query).await {
        let html = render_form(model, Some(&form), true, Some(&e.to_string()));
        return Ok(Html(html).into_response());
    }
    Ok(Redirect::to(&format!("/{}/{}", model.table, pk_raw)).into_response())
}

// ============================================================== DELETE

async fn delete_submit(
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

    rustango_sql::delete(
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

// ============================================================== HELPERS

/// Resolve `table` to a `ModelSchema`, but only if the admin is configured
/// to expose it. A model that exists but is filtered out via `show_only`
/// returns `None` here, which surfaces to users as a 404 — same response
/// as a genuinely missing table.
fn lookup_model(state: &AppState, table: &str) -> Option<&'static ModelSchema> {
    if !state.is_visible(table) {
        return None;
    }
    inventory::iter::<ModelEntry>
        .into_iter()
        .find(|e| e.schema.table == table)
        .map(|e| e.schema)
}

/// Map of `(target_table, source_value_string) → display_value_html`.
/// Populated from joined rows so list/detail rendering needs no extra
/// per-FK queries.
type FkMap = HashMap<(String, String), String>;

/// Build one [`Join`] per FK / O2O column on `model` whose target is
/// visible and has a display field. The join's `project` carries only
/// the target's display column — that's all the admin renders.
fn build_fk_joins(state: &AppState, model: &'static ModelSchema) -> Vec<Join> {
    let mut joins = Vec::new();
    for field in model.scalar_fields() {
        let Some(rel) = field.relation else { continue };
        let (to, on) = match rel {
            Relation::Fk { to, on } | Relation::O2O { to, on } => (to, on),
            Relation::M2M { .. } => continue,
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
fn fk_map_from_joined_rows(
    state: &AppState,
    model: &'static ModelSchema,
    rows: &[sqlx::postgres::PgRow],
) -> FkMap {
    let mut map: FkMap = HashMap::new();
    for field in model.scalar_fields() {
        let Some(rel) = field.relation else { continue };
        let to = match rel {
            Relation::Fk { to, .. } | Relation::O2O { to, .. } => to,
            Relation::M2M { .. } => continue,
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

/// Render one cell. For FK columns this resolves to a link into the target
/// table; everything else delegates to [`render::render_value`].
/// Build a `&q=…&<field>=<v>…` tail for prev/next pager URLs so the
/// active search and filters survive page navigation. Each value is
/// percent-encoded via a tiny ASCII-safe escaper good enough for the
/// admin's expected inputs.
fn pager_suffix(q: Option<&str>, filters: &[(&'static str, String)]) -> String {
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

fn render_cell(row: &sqlx::postgres::PgRow, field: &FieldSchema, fk_map: &FkMap) -> String {
    if let Some(rel) = field.relation {
        let to = match rel {
            Relation::Fk { to, .. } | Relation::O2O { to, .. } => Some(to),
            Relation::M2M { .. } => None,
        };
        if let Some(to) = to {
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
    }
    render::render_value(row, field)
}

/// Render a create or edit form via the `form.html` template. Pre-fill
/// values come from `prefill` (keyed by Rust field name); pass `None` for
/// an empty create form. `pk_locked` makes the PK input read-only (edit
/// mode). `error_msg`, when present, is shown above the form.
fn render_form(
    model: &'static ModelSchema,
    prefill: Option<&HashMap<String, String>>,
    pk_locked: bool,
    error_msg: Option<&str>,
) -> String {
    let action = if pk_locked {
        let pk_field = model.primary_key().expect("pk_locked requires a PK");
        let pk_value = prefill
            .and_then(|m| m.get(pk_field.name).cloned())
            .unwrap_or_default();
        format!("/{}/{}", model.table, render::escape(&pk_value))
    } else {
        format!("/{}", model.table)
    };
    let title = if pk_locked {
        format!("Edit {}", model.name)
    } else {
        format!("New {}", model.name)
    };

    let rows_ctx: Vec<serde_json::Value> = model
        .scalar_fields()
        .map(|f| {
            let value = prefill
                .and_then(|m| m.get(f.name))
                .map_or("", String::as_str);
            let extra = if f.primary_key {
                " <small>(pk)</small>"
            } else if !f.nullable {
                " <small>required</small>"
            } else {
                ""
            };
            serde_json::json!({
                "label": f.name,
                "extra": extra,
                "input": render::render_input(f, value, pk_locked),
            })
        })
        .collect();

    render(
        "form.html",
        &serde_json::json!({
            "model": { "name": model.name, "table": model.table },
            "title": title,
            "action": action,
            "error": error_msg,
            "rows": rows_ctx,
        }),
    )
}

// ============================================================== ERRORS

#[derive(Debug)]
enum AdminError {
    TableNotFound { table: String },
    RowNotFound { table: String, pk: String },
    ReadOnly { table: String },
    Form(FormError),
    Internal(String),
}

impl From<sqlx::Error> for AdminError {
    fn from(e: sqlx::Error) -> Self {
        Self::Internal(e.to_string())
    }
}

impl From<rustango_sql::ExecError> for AdminError {
    fn from(e: rustango_sql::ExecError) -> Self {
        Self::Internal(e.to_string())
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        match self {
            Self::TableNotFound { table } => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "table not found", "table": table })),
            )
                .into_response(),
            Self::RowNotFound { table, pk } => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "row not found", "table": table, "pk": pk })),
            )
                .into_response(),
            Self::ReadOnly { table } => (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({ "error": "table is read-only", "table": table })),
            )
                .into_response(),
            Self::Form(e) => (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "form", "detail": e.to_string() })),
            )
                .into_response(),
            Self::Internal(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "internal", "detail": msg })),
            )
                .into_response(),
        }
    }
}
