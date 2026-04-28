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
use std::sync::Arc;

use axum::extract::{Form, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Json;
use axum::Router;
use rustango_core::{
    inventory, CountQuery, DeleteQuery, FieldSchema, Filter, InsertQuery, ModelEntry, ModelSchema,
    Op, Relation, SearchClause, SelectQuery, SqlValue, UpdateQuery,
};
use rustango_sql::sqlx::{self, PgPool};

use forms::FormError;

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
        self.config.read_only_tables.contains(table)
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

    let mut html = String::from(PAGE_HEAD);
    html.push_str("<h1>rustango admin</h1>");
    if models.is_empty() {
        html.push_str("<p><em>No models registered.</em></p>");
    } else {
        html.push_str(
            "<table><thead><tr><th>Model</th><th>Table</th><th>Fields</th></tr></thead><tbody>",
        );
        for m in models {
            let name = render::escape(m.name);
            let table = render::escape(m.table);
            let count = m.scalar_fields().count();
            let _ = write!(
                html,
                "<tr><td><a href=\"/{table}\">{name}</a></td><td>{table}</td><td>{count}</td></tr>",
            );
        }
        html.push_str("</tbody></table>");
    }
    html.push_str(PAGE_FOOT);
    Html(html)
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

    let total = rustango_sql::count_rows(
        &state.pool,
        &CountQuery {
            model,
            filters: filters.clone(),
        },
    )
    .await?;
    // NOTE: count_rows ignores the search clause; counts are approximate
    // when ?q is set. Acceptable for a v0.2 admin pager.
    let rows = rustango_sql::select_rows(
        &state.pool,
        &SelectQuery {
            model,
            filters: filters.clone(),
            search: search.clone(),
            limit: Some(PAGE_SIZE),
            offset: Some(offset),
        },
    )
    .await?;

    let fk_map = resolve_fk_displays(&state, model, &rows).await?;

    let last_page = if total == 0 {
        1
    } else {
        ((total - 1) / PAGE_SIZE) + 1
    };

    let mut html = String::from(PAGE_HEAD);
    let name = render::escape(model.name);
    let table_q = render::escape(model.table);
    let plural = if total == 1 { "" } else { "s" };
    let read_only = state.is_read_only(model.table);
    let new_link = if read_only {
        String::from(" &nbsp;&middot;&nbsp; <em>read-only</em>")
    } else {
        format!(r#" &nbsp;&middot;&nbsp; <a href="/{table_q}/new">+ new {name}</a>"#)
    };
    let _ = write!(
        html,
        r#"<p><a href="/">&larr; admin home</a></p><h1>{name}</h1>
<p>Table: <code>{table_q}</code> &mdash; {total} row{plural}{new_link}</p>"#,
    );

    // Search box (when at least one searchable field exists).
    if model.searchable_fields().next().is_some() {
        let q_val = render::escape(q.as_deref().unwrap_or(""));
        let _ = write!(
            html,
            r#"<form method="get" action="/{table_q}" class="search">
<input type="search" name="q" value="{q_val}" placeholder="search&hellip;">
<button type="submit">go</button>"#,
        );
        // Carry active field filters through the form so submitting search
        // doesn't drop them.
        for (k, v) in &active_field_filters {
            let _ = write!(
                html,
                r#"<input type="hidden" name="{}" value="{}">"#,
                render::escape(k),
                render::escape(v),
            );
        }
        html.push_str("</form>");
    }

    // Active-filter badges + clear-all link.
    if !active_field_filters.is_empty() || q.is_some() {
        html.push_str(r#"<p class="active-filters">filtered by: "#);
        if let Some(qs) = &q {
            let _ = write!(html, "<code>q={}</code> ", render::escape(qs));
        }
        for (k, v) in &active_field_filters {
            let _ = write!(
                html,
                "<code>{}={}</code> ",
                render::escape(k),
                render::escape(v),
            );
        }
        let _ = write!(html, r#"&middot; <a href="/{table_q}">clear</a></p>"#);
    }

    if rows.is_empty() {
        html.push_str("<p><em>No rows on this page.</em></p>");
    } else {
        html.push_str("<table><thead><tr>");
        for f in model.scalar_fields() {
            let label = if f.primary_key {
                format!("{} <small>(pk)</small>", render::escape(f.name))
            } else {
                render::escape(f.name)
            };
            let _ = write!(html, "<th>{label}</th>");
        }
        html.push_str("<th></th></tr></thead><tbody>");
        for row in &rows {
            html.push_str("<tr>");
            for f in model.scalar_fields() {
                let value = render_cell(row, f, &fk_map);
                let _ = write!(html, "<td>{value}</td>");
            }
            html.push_str("<td>");
            if let Some(pk) = pk_field {
                let pk_str = render::render_value_for_input(row, pk);
                let pk_esc = render::escape(&pk_str);
                let _ = write!(html, r#"<a href="/{table_q}/{pk_esc}">view</a>"#);
            }
            html.push_str("</td></tr>");
        }
        html.push_str("</tbody></table>");
    }

    // Pager. Pager URLs preserve q + field filters via a query-string suffix.
    if last_page > 1 {
        let suffix = pager_suffix(q.as_deref(), &active_field_filters);
        html.push_str(r#"<p class="pager">"#);
        if page > 1 {
            let _ = write!(
                html,
                r#"<a href="/{table_q}?page={prev}{suffix}">&larr; prev</a> &middot; "#,
                prev = page - 1,
            );
        } else {
            html.push_str(r#"<span class="muted">&larr; prev</span> &middot; "#);
        }
        let _ = write!(html, "page {page} of {last_page}");
        if page < last_page {
            let _ = write!(
                html,
                r#" &middot; <a href="/{table_q}?page={next}{suffix}">next &rarr;</a>"#,
                next = page + 1,
            );
        } else {
            html.push_str(r#" &middot; <span class="muted">next &rarr;</span>"#);
        }
        html.push_str("</p>");
    }
    html.push_str(PAGE_FOOT);
    Ok(Html(html))
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
            filters: vec![Filter {
                column: pk_field.column,
                op: Op::Eq,
                value: pk_value,
            }],
            search: None,
            limit: None,
            offset: None,
        },
    )
    .await?
    .ok_or(AdminError::RowNotFound {
        table: table.clone(),
        pk: pk_raw.clone(),
    })?;

    // Resolve any FK display values for this single row before rendering.
    let fk_map = resolve_fk_displays(&state, model, std::slice::from_ref(&row)).await?;

    let mut html = String::from(PAGE_HEAD);
    let name = render::escape(model.name);
    let table_q = render::escape(model.table);
    let pk_esc = render::escape(&pk_raw);
    let _ = write!(
        html,
        r#"<p><a href="/">admin</a> &rsaquo; <a href="/{table_q}">{name}</a> &rsaquo; <strong>{pk_esc}</strong></p>
<h1>{name} #{pk_esc}</h1>
<dl>"#,
    );
    for f in model.scalar_fields() {
        let label = render::escape(f.name);
        let value = render_cell(&row, f, &fk_map);
        let _ = write!(html, "<dt>{label}</dt><dd>{value}</dd>");
    }
    html.push_str("</dl>");
    if state.is_read_only(model.table) {
        html.push_str("<p><em>This table is read-only.</em></p>");
    } else {
        let _ = write!(
            html,
            r#"<p><a href="/{table_q}/{pk_esc}/edit">edit</a> &middot;
<form method="post" action="/{table_q}/{pk_esc}/delete" style="display:inline" onsubmit="return confirm('Delete this row?')">
<button type="submit">delete</button></form></p>"#,
        );
    }
    html.push_str(PAGE_FOOT);
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
            filters: vec![Filter {
                column: pk_field.column,
                op: Op::Eq,
                value: pk_value,
            }],
            search: None,
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
        filters: vec![Filter {
            column: pk_field.column,
            op: Op::Eq,
            value: pk_value,
        }],
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
            filters: vec![Filter {
                column: pk_field.column,
                op: Op::Eq,
                value: pk_value,
            }],
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

/// Map of `(target_table, source_value_string) → display_value_html`. Built
/// once per page load; rendering then looks up FK display values from it
/// instead of issuing one query per row.
type FkMap = HashMap<(String, String), String>;

/// For every FK / O2O column on `model`, batch-fetch the target rows and
/// build a map keyed by `(target_table, source_value_as_string)`. Targets
/// that aren't visible (filtered via `show_only`) or whose row is missing
/// just don't appear in the map — `render_cell` then falls back to the
/// raw value.
async fn resolve_fk_displays(
    state: &AppState,
    model: &'static ModelSchema,
    rows: &[sqlx::postgres::PgRow],
) -> Result<FkMap, AdminError> {
    let mut map: FkMap = HashMap::new();
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
        let Some(on_field) = target.field_by_column(on) else {
            continue;
        };

        // Distinct FK values from the visible rows.
        let mut seen = HashSet::new();
        let mut fk_values: Vec<SqlValue> = Vec::new();
        for row in rows {
            let Some(s) = render::read_value_as_string(row, field) else {
                continue;
            };
            if seen.insert(s.clone()) {
                if let Some(v) = render::read_value_as_sqlvalue(row, field) {
                    fk_values.push(v);
                }
            }
        }
        if fk_values.is_empty() {
            continue;
        }

        let target_rows = rustango_sql::select_rows(
            &state.pool,
            &SelectQuery {
                model: target,
                filters: vec![Filter {
                    column: on,
                    op: Op::In,
                    value: SqlValue::List(fk_values),
                }],
                search: None,
                limit: None,
                offset: None,
            },
        )
        .await?;

        for trow in &target_rows {
            let Some(key) = render::read_value_as_string(trow, on_field) else {
                continue;
            };
            let display = render::render_value(trow, display_field);
            map.insert((to.to_owned(), key), display);
        }
    }
    Ok(map)
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

/// Render a create or edit form. Pre-fill values come from `prefill` (keyed
/// by Rust field name); pass `None` for an empty create form. `pk_locked`
/// makes the PK input read-only (edit mode). `error_msg`, when present, is
/// shown above the form.
fn render_form(
    model: &'static ModelSchema,
    prefill: Option<&HashMap<String, String>>,
    pk_locked: bool,
    error_msg: Option<&str>,
) -> String {
    let mut html = String::from(PAGE_HEAD);
    let name = render::escape(model.name);
    let table = render::escape(model.table);
    let action = if pk_locked {
        // Update form: POST back to /{table}/{pk}
        let pk_field = model.primary_key().expect("pk_locked requires a PK");
        let pk_value = prefill
            .and_then(|m| m.get(pk_field.name).cloned())
            .unwrap_or_default();
        format!("/{}/{}", model.table, render::escape(&pk_value))
    } else {
        // Create form: POST to /{table}
        format!("/{}", model.table)
    };
    let title = if pk_locked {
        format!("Edit {name}")
    } else {
        format!("New {name}")
    };
    let _ = write!(
        html,
        r#"<p><a href="/">admin</a> &rsaquo; <a href="/{table}">{name}</a></p>
<h1>{title}</h1>"#,
    );
    if let Some(err) = error_msg {
        let _ = write!(html, r#"<p class="error">{}</p>"#, render::escape(err));
    }
    let _ = write!(
        html,
        r#"<form method="post" action="{action}"><table>"#,
        action = render::escape(&action),
    );
    for f in model.scalar_fields() {
        let value = prefill
            .and_then(|m| m.get(f.name))
            .map_or("", String::as_str);
        render_form_row(&mut html, f, value, pk_locked);
    }
    html.push_str("</table>");
    let _ = write!(html, r#"<p><button type="submit">save</button></p></form>"#,);
    html.push_str(PAGE_FOOT);
    html
}

fn render_form_row(html: &mut String, field: &FieldSchema, value: &str, pk_locked: bool) {
    let label = render::escape(field.name);
    let extra = if field.primary_key {
        " <small>(pk)</small>"
    } else if !field.nullable {
        " <small>required</small>"
    } else {
        ""
    };
    let input = render::render_input(field, value, pk_locked);
    let _ = write!(
        html,
        r#"<tr><th><label for="{label}">{label}{extra}</label></th><td>{input}</td></tr>"#,
    );
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

const PAGE_HEAD: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>rustango admin</title>
<style>
body { font-family: -apple-system, system-ui, sans-serif; max-width: 960px; margin: 2rem auto; padding: 0 1rem; }
table { border-collapse: collapse; width: 100%; }
th, td { border: 1px solid #ccc; padding: .35rem .6rem; text-align: left; vertical-align: top; }
th { background: #f4f4f4; }
a { color: #0a4; text-decoration: none; }
a:hover { text-decoration: underline; }
small { color: #888; font-weight: normal; }
em { color: #888; }
input[type=text], input[type=number], input[type=date], input[type=datetime-local], textarea { width: 100%; box-sizing: border-box; padding: .25rem .4rem; font: inherit; }
textarea { min-height: 4rem; }
.error { background: #fee; border: 1px solid #f88; padding: .5rem .75rem; border-radius: 4px; }
button { padding: .4rem 1rem; font: inherit; cursor: pointer; }
dl { display: grid; grid-template-columns: max-content 1fr; gap: .25rem 1rem; }
dt { font-weight: bold; }
</style>
</head>
<body>
"#;

const PAGE_FOOT: &str = "\n</body>\n</html>";
