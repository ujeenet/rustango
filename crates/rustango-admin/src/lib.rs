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
    Op, SelectQuery, SqlValue, UpdateQuery,
};
use rustango_sql::sqlx::{self, PgPool};
use serde::Deserialize;

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

#[derive(Deserialize, Default)]
struct PageParams {
    /// 1-based page number; clamped to `>= 1` on read.
    page: Option<i64>,
}

async fn table_view(
    Path(table): Path<String>,
    Query(params): Query<PageParams>,
    State(state): State<AppState>,
) -> Result<Html<String>, AdminError> {
    let model = lookup_model(&state, &table).ok_or(AdminError::TableNotFound { table })?;
    let pk_field = model.primary_key();
    let page = params.page.unwrap_or(1).max(1);
    let offset = (page - 1) * PAGE_SIZE;

    // One COUNT(*) for the pager, one SELECT for the visible page.
    let total = rustango_sql::count_rows(
        &state.pool,
        &CountQuery {
            model,
            filters: vec![],
        },
    )
    .await?;
    let rows = rustango_sql::select_rows(
        &state.pool,
        &SelectQuery {
            model,
            filters: vec![],
            limit: Some(PAGE_SIZE),
            offset: Some(offset),
        },
    )
    .await?;

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
                let value = render::render_value(row, f);
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

    // Pager.
    if last_page > 1 {
        html.push_str(r#"<p class="pager">"#);
        if page > 1 {
            let _ = write!(
                html,
                r#"<a href="/{table_q}?page={prev}">&larr; prev</a> &middot; "#,
                prev = page - 1,
            );
        } else {
            html.push_str(r#"<span class="muted">&larr; prev</span> &middot; "#);
        }
        let _ = write!(html, "page {page} of {last_page}");
        if page < last_page {
            let _ = write!(
                html,
                r#" &middot; <a href="/{table_q}?page={next}">next &rarr;</a>"#,
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
            limit: None,
            offset: None,
        },
    )
    .await?
    .ok_or(AdminError::RowNotFound {
        table: table.clone(),
        pk: pk_raw.clone(),
    })?;

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
        let value = render::render_value(&row, f);
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
