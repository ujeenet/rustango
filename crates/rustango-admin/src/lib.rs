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
//! v0.1 scope: list of models on `/`, list of rows on `/<table>`. Detail
//! view, create/edit forms, and delete confirmation land in the next
//! slice.

mod render;

use std::fmt::Write as _;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rustango_core::{ModelEntry, ModelSchema, SelectQuery, inventory};
use rustango_sql::sqlx::{self, PgPool};
use rustango_sql::{Dialect, Postgres};

/// Mount the admin under any prefix using axum's nesting:
/// `Router::new().nest("/admin", rustango_admin::router(pool))`.
pub fn router(pool: PgPool) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/{table}", get(table_view))
        .with_state(AppState { pool })
}

#[derive(Clone)]
struct AppState {
    pool: PgPool,
}

/// `GET /` — list every registered model with a link to its row list.
async fn index() -> Html<String> {
    let mut models: Vec<&'static ModelSchema> = inventory::iter::<ModelEntry>
        .into_iter()
        .map(|e| e.schema)
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

/// `GET /<table>` — render every row in the table as an HTML grid.
async fn table_view(
    Path(table): Path<String>,
    State(state): State<AppState>,
) -> Result<Html<String>, AdminError> {
    let model = lookup_model(&table).ok_or(AdminError::TableNotFound { table })?;

    let select = SelectQuery {
        model,
        filters: vec![],
    };
    let stmt = Postgres
        .compile_select(&select)
        .map_err(|e| AdminError::Internal(e.to_string()))?;
    let rows = sqlx::query(&stmt.sql).fetch_all(&state.pool).await?;

    let mut html = String::from(PAGE_HEAD);
    let name = render::escape(model.name);
    let table = render::escape(model.table);
    let count = rows.len();
    let plural = if count == 1 { "" } else { "s" };
    let _ = write!(
        html,
        "<p><a href=\"/\">&larr; admin home</a></p><h1>{name}</h1><p>Table: <code>{table}</code> &mdash; {count} row{plural}</p>",
    );

    if rows.is_empty() {
        html.push_str("<p><em>No rows.</em></p>");
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
        html.push_str("</tr></thead><tbody>");
        for row in &rows {
            html.push_str("<tr>");
            for f in model.scalar_fields() {
                let value = render::render_value(row, f);
                let _ = write!(html, "<td>{value}</td>");
            }
            html.push_str("</tr>");
        }
        html.push_str("</tbody></table>");
    }
    html.push_str(PAGE_FOOT);
    Ok(Html(html))
}

fn lookup_model(table: &str) -> Option<&'static ModelSchema> {
    inventory::iter::<ModelEntry>
        .into_iter()
        .find(|e| e.schema.table == table)
        .map(|e| e.schema)
}

/// Errors surfaced by admin handlers; rendered as plain JSON 4xx/5xx.
#[derive(Debug)]
enum AdminError {
    TableNotFound { table: String },
    Internal(String),
}

impl From<sqlx::Error> for AdminError {
    fn from(e: sqlx::Error) -> Self {
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
</style>
</head>
<body>
"#;

const PAGE_FOOT: &str = "\n</body>\n</html>";
