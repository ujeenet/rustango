//! Generic class-based views for HTML templates (Django-shape).
//!
//! Sibling of [`crate::viewset`] for the JSON/API side. Each view is a
//! data structure that builds a Tera-rendered axum `Router` over a
//! `#[derive(Model)]` schema:
//!
//! | View | What it does | Template default |
//! |------|--------------|------------------|
//! | [`ListView`] | Paginated list — `?page=N` query param | `<table>_list.html` |
//! | [`DetailView`] | Single row by primary key — `/{pk}` | `<table>_detail.html` |
//! | [`CreateView`] | (slice 3) GET form / POST insert | `<table>_form.html` |
//! | [`UpdateView`] | (slice 3) GET form / POST update | `<table>_form.html` |
//! | [`DeleteView`] | (slice 4) GET confirm / POST delete | `<table>_confirm_delete.html` |
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::template_views::ListView;
//! use std::sync::Arc;
//! use tera::Tera;
//!
//! let mut tera = Tera::default();
//! tera.add_raw_template("post_list.html", r#"
//!     {% for post in object_list %}
//!         <h2>{{ post.title }}</h2>
//!         <p>{{ post.body }}</p>
//!     {% endfor %}
//!     {% if has_prev %}<a href="?page={{ page - 1 }}">prev</a>{% endif %}
//!     {% if has_next %}<a href="?page={{ page + 1 }}">next</a>{% endif %}
//! "#).unwrap();
//!
//! let app = ListView::for_model(Post::SCHEMA)
//!     .page_size(20)
//!     .order_by("created_at", true)   // DESC
//!     .router("/posts", Arc::new(tera), pool);
//! ```
//!
//! ## Tera context
//!
//! Every view stamps a consistent context shape so templates port
//! cleanly between views:
//!
//! - `object_list: Vec<Map<String, Value>>` — the page's rows as JSON
//!   (`null` for SQL nulls; columns named after the field's `column`)
//! - `page: i64` — 1-indexed current page
//! - `page_size: i64`
//! - `total: i64` — total matching rows across every page
//! - `total_pages: i64`
//! - `has_next: bool`
//! - `has_prev: bool`
//!
//! ## Single-tenant only (today)
//!
//! These views take a `PgPool` at mount time, like the original
//! `ViewSet::router`. Tenancy projects use the per-request
//! `Tenant` extractor and hand-roll the equivalent for now — a
//! `tenant_router` variant lands in a follow-up once the auto-
//! admin's per-tenant pattern stabilises.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde_json::Value;
use tera::{Context, Tera};

use crate::core::{Filter, ModelSchema, Op, OrderClause, SelectQuery, SqlValue, WhereExpr};
use crate::sql::sqlx::PgPool;
use crate::sql::{count_rows, row_to_json, select_one_row, select_rows};

// ============================================================== ListView

/// Render a paginated list of rows for `M`.
///
/// See the [module docs] for the Tera context shape and template
/// defaults.
///
/// [module docs]: crate::template_views
#[derive(Clone)]
pub struct ListView {
    schema: &'static ModelSchema,
    template: String,
    page_size: i64,
    fields: Option<Vec<String>>,
    order_by: Vec<(String, bool)>,
}

impl ListView {
    /// Start a `ListView` for the given schema. Defaults: template
    /// name `<table>_list.html`, page size 20, no `ORDER BY`, all
    /// fields included.
    #[must_use]
    pub fn for_model(schema: &'static ModelSchema) -> Self {
        Self {
            schema,
            template: format!("{}_list.html", schema.table),
            page_size: 20,
            fields: None,
            order_by: Vec::new(),
        }
    }

    /// Override the Tera template name.
    #[must_use]
    pub fn template(mut self, name: impl Into<String>) -> Self {
        self.template = name.into();
        self
    }

    /// Page size — clamped to `≥ 1`. Default 20.
    #[must_use]
    pub fn page_size(mut self, n: usize) -> Self {
        self.page_size = i64::try_from(n).unwrap_or(20).max(1);
        self
    }

    /// Add an `ORDER BY` clause. Call multiple times for tie-breakers.
    #[must_use]
    pub fn order_by(mut self, column: impl Into<String>, desc: bool) -> Self {
        self.order_by.push((column.into(), desc));
        self
    }

    /// Restrict the columns rendered into the Tera context. Default
    /// (`None`) renders every scalar field.
    #[must_use]
    pub fn fields(mut self, names: &[&str]) -> Self {
        self.fields = Some(names.iter().map(|s| (*s).to_owned()).collect());
        self
    }

    /// Mount as `GET <prefix>` rendering through `tera` from `pool`.
    /// Single-tenant pool capture — hand-roll a `Tenant`-extractor
    /// equivalent for tenancy projects until `tenant_router` ships.
    #[must_use]
    pub fn router(self, prefix: &str, tera: Arc<Tera>, pool: PgPool) -> Router<()> {
        let state = Arc::new(ListViewState {
            vs: self,
            tera,
            pool,
        });
        Router::new()
            .route(prefix, get(handle_list))
            .with_state(state)
    }
}

#[derive(Clone)]
struct ListViewState {
    vs: ListView,
    tera: Arc<Tera>,
    pool: PgPool,
}

async fn handle_list(
    State(state): State<Arc<ListViewState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let page: i64 = params
        .get("page")
        .and_then(|p| p.parse().ok())
        .unwrap_or(1)
        .max(1);
    let offset = (page - 1) * state.vs.page_size;

    let order_by = match resolve_order_by(state.vs.schema, &state.vs.order_by) {
        Ok(v) => v,
        Err(msg) => return template_error(&msg),
    };
    let select_q = SelectQuery {
        model: state.vs.schema,
        where_clause: WhereExpr::And(vec![]),
        search: None,
        joins: vec![],
        order_by,
        limit: Some(state.vs.page_size),
        offset: Some(offset),
    };
    let count_q = crate::core::CountQuery {
        model: state.vs.schema,
        where_clause: WhereExpr::And(vec![]),
    };

    let (rows_result, count_result) = tokio::join!(
        select_rows(&state.pool, &select_q),
        count_rows(&state.pool, &count_q),
    );
    let rows = match rows_result {
        Ok(r) => r,
        Err(e) => return template_error(&format!("query rows: {e}")),
    };
    let total = match count_result {
        Ok(c) => c,
        Err(e) => return template_error(&format!("count rows: {e}")),
    };

    let fields = resolved_fields(state.vs.schema, state.vs.fields.as_deref());
    let object_list: Vec<Value> = rows.iter().map(|r| row_to_json(r, &fields)).collect();

    let total_pages = ((total - 1).max(0) / state.vs.page_size) + 1;
    let mut ctx = Context::new();
    ctx.insert("object_list", &object_list);
    ctx.insert("page", &page);
    ctx.insert("page_size", &state.vs.page_size);
    ctx.insert("total", &total);
    ctx.insert("total_pages", &total_pages);
    ctx.insert("has_next", &(page < total_pages));
    ctx.insert("has_prev", &(page > 1));

    render(&state.tera, &state.vs.template, &ctx)
}

// ============================================================== DetailView

/// Render a single row of `M` looked up by primary key.
///
/// Mounts as `GET <prefix>/{pk}`. The `{pk}` segment is parsed as a
/// string (no type coercion at the URL layer); the SQL probe quotes
/// it as a `SqlValue::Text` so any PK type that compares against
/// the column's actual SQL type via Postgres' implicit casts works
/// (`i64`, `Uuid`, `String`, …).
#[derive(Clone)]
pub struct DetailView {
    schema: &'static ModelSchema,
    template: String,
    fields: Option<Vec<String>>,
}

impl DetailView {
    #[must_use]
    pub fn for_model(schema: &'static ModelSchema) -> Self {
        Self {
            schema,
            template: format!("{}_detail.html", schema.table),
            fields: None,
        }
    }

    #[must_use]
    pub fn template(mut self, name: impl Into<String>) -> Self {
        self.template = name.into();
        self
    }

    #[must_use]
    pub fn fields(mut self, names: &[&str]) -> Self {
        self.fields = Some(names.iter().map(|s| (*s).to_owned()).collect());
        self
    }

    #[must_use]
    pub fn router(self, prefix: &str, tera: Arc<Tera>, pool: PgPool) -> Router<()> {
        let state = Arc::new(DetailViewState {
            vs: self,
            tera,
            pool,
        });
        let path = format!("{}/{{pk}}", prefix.trim_end_matches('/'));
        Router::new()
            .route(&path, get(handle_detail))
            .with_state(state)
    }
}

#[derive(Clone)]
struct DetailViewState {
    vs: DetailView,
    tera: Arc<Tera>,
    pool: PgPool,
}

async fn handle_detail(
    State(state): State<Arc<DetailViewState>>,
    Path(pk): Path<String>,
) -> Response {
    let Some(pk_field) = state.vs.schema.primary_key() else {
        return template_error(&format!(
            "model `{}` has no primary key — DetailView can't probe by PK",
            state.vs.schema.table
        ));
    };
    let select_q = SelectQuery {
        model: state.vs.schema,
        where_clause: WhereExpr::Predicate(Filter {
            column: pk_field.column,
            op: Op::Eq,
            value: SqlValue::String(pk),
        }),
        search: None,
        joins: vec![],
        order_by: vec![],
        limit: Some(1),
        offset: None,
    };
    let row = match select_one_row(&state.pool, &select_q).await {
        Ok(Some(r)) => r,
        Ok(None) => return (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => return template_error(&format!("query row: {e}")),
    };

    let fields = resolved_fields(state.vs.schema, state.vs.fields.as_deref());
    let object = row_to_json(&row, &fields);
    let mut ctx = Context::new();
    ctx.insert("object", &object);

    render(&state.tera, &state.vs.template, &ctx)
}

// ============================================================== DeleteView

/// Two-step delete: `GET <prefix>/{pk}/delete` renders a confirmation
/// page, `POST <prefix>/{pk}/delete` executes the delete and 303s to
/// `success_url`. Mirrors Django's `DeleteView`.
///
/// CSRF protection is the project's responsibility — mount this view
/// under a CSRF-protected scope (`rustango::forms::csrf`) when the
/// POST is reachable from a browser.
#[derive(Clone)]
pub struct DeleteView {
    schema: &'static ModelSchema,
    template: String,
    success_url: String,
    fields: Option<Vec<String>>,
}

impl DeleteView {
    /// Start a `DeleteView` for the given schema. Defaults: template
    /// `<table>_confirm_delete.html`, redirect on success to `/`.
    #[must_use]
    pub fn for_model(schema: &'static ModelSchema) -> Self {
        Self {
            schema,
            template: format!("{}_confirm_delete.html", schema.table),
            success_url: "/".to_owned(),
            fields: None,
        }
    }

    #[must_use]
    pub fn template(mut self, name: impl Into<String>) -> Self {
        self.template = name.into();
        self
    }

    /// Where the browser is redirected after a successful POST.
    /// Default `/`. Typical: the list view's URL (`/posts`).
    #[must_use]
    pub fn success_url(mut self, url: impl Into<String>) -> Self {
        self.success_url = url.into();
        self
    }

    #[must_use]
    pub fn fields(mut self, names: &[&str]) -> Self {
        self.fields = Some(names.iter().map(|s| (*s).to_owned()).collect());
        self
    }

    /// Mount as `GET`/`POST <prefix>/{pk}/delete`.
    #[must_use]
    pub fn router(self, prefix: &str, tera: Arc<Tera>, pool: PgPool) -> Router<()> {
        let state = Arc::new(DeleteViewState {
            vs: self,
            tera,
            pool,
        });
        let path = format!("{}/{{pk}}/delete", prefix.trim_end_matches('/'));
        Router::new()
            .route(
                &path,
                axum::routing::get(handle_delete_confirm).post(handle_delete_submit),
            )
            .with_state(state)
    }
}

#[derive(Clone)]
struct DeleteViewState {
    vs: DeleteView,
    tera: Arc<Tera>,
    pool: PgPool,
}

async fn handle_delete_confirm(
    State(state): State<Arc<DeleteViewState>>,
    Path(pk): Path<String>,
) -> Response {
    let Some(pk_field) = state.vs.schema.primary_key() else {
        return template_error(&format!(
            "model `{}` has no primary key — DeleteView can't probe by PK",
            state.vs.schema.table
        ));
    };
    let select_q = SelectQuery {
        model: state.vs.schema,
        where_clause: WhereExpr::Predicate(Filter {
            column: pk_field.column,
            op: Op::Eq,
            value: SqlValue::String(pk),
        }),
        search: None,
        joins: vec![],
        order_by: vec![],
        limit: Some(1),
        offset: None,
    };
    let row = match select_one_row(&state.pool, &select_q).await {
        Ok(Some(r)) => r,
        Ok(None) => return (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => return template_error(&format!("query row: {e}")),
    };
    let fields = resolved_fields(state.vs.schema, state.vs.fields.as_deref());
    let object = row_to_json(&row, &fields);
    let mut ctx = Context::new();
    ctx.insert("object", &object);
    render(&state.tera, &state.vs.template, &ctx)
}

async fn handle_delete_submit(
    State(state): State<Arc<DeleteViewState>>,
    Path(pk): Path<String>,
) -> Response {
    let Some(pk_field) = state.vs.schema.primary_key() else {
        return template_error(&format!(
            "model `{}` has no primary key — DeleteView can't delete by PK",
            state.vs.schema.table
        ));
    };
    let delete_q = crate::core::DeleteQuery {
        model: state.vs.schema,
        where_clause: WhereExpr::Predicate(Filter {
            column: pk_field.column,
            op: Op::Eq,
            value: SqlValue::String(pk),
        }),
    };
    match crate::sql::delete(&state.pool, &delete_q).await {
        Ok(0) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Ok(_) => axum::response::Redirect::to(&state.vs.success_url).into_response(),
        Err(e) => template_error(&format!("delete row: {e}")),
    }
}

// ============================================================== shared helpers

/// Resolve a `Vec<(name, desc)>` into the static-string `OrderClause`
/// shape the SQL writer expects. Returns the original column name in
/// the error string when it doesn't match any field.
fn resolve_order_by(
    schema: &'static ModelSchema,
    spec: &[(String, bool)],
) -> Result<Vec<OrderClause>, String> {
    let mut out = Vec::with_capacity(spec.len());
    for (name, desc) in spec {
        let field = schema
            .fields
            .iter()
            .find(|f| f.name == name || f.column == name)
            .ok_or_else(|| {
                format!(
                    "order_by(`{}`) does not match any field on `{}`",
                    name, schema.table
                )
            })?;
        out.push(OrderClause {
            column: field.column,
            desc: *desc,
        });
    }
    Ok(out)
}

/// Resolve the projection set — either every scalar field or the
/// caller's explicit `fields` allowlist.
fn resolved_fields(
    schema: &'static ModelSchema,
    explicit: Option<&[String]>,
) -> Vec<&'static crate::core::FieldSchema> {
    match explicit {
        Some(names) => schema
            .scalar_fields()
            .filter(|f| names.iter().any(|n| n == f.name || n == f.column))
            .collect(),
        None => schema.scalar_fields().collect(),
    }
}

/// Render a Tera template, returning a 200 + HTML body on success
/// or 500 + a plain-text error on failure. The error body is
/// deliberately small — operators get the full Tera error in the
/// tracing log; the response body is for the developer's eyeballs
/// during local dev.
fn render(tera: &Tera, name: &str, ctx: &Context) -> Response {
    match tera.render(name, ctx) {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::warn!(target: "rustango::template_views", template = %name, error = %e, "template render failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("template render error: {e}"),
            )
                .into_response()
        }
    }
}

fn template_error(msg: &str) -> Response {
    tracing::warn!(target: "rustango::template_views", error = %msg, "template view error");
    (StatusCode::INTERNAL_SERVER_ERROR, msg.to_owned()).into_response()
}

// ============================================================== tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::FieldType;

    fn schema_two_fields() -> &'static ModelSchema {
        // Build a tiny schema by leaking — fine in unit tests.
        Box::leak(Box::new(ModelSchema {
            name: "Post",
            table: "posts",
            fields: Box::leak(Box::new([
                crate::core::FieldSchema {
                    name: "id",
                    column: "id",
                    ty: FieldType::I64,
                    nullable: false,
                    primary_key: true,
                    relation: None,
                    max_length: None,
                    min: None,
                    max: None,
                    default: None,
                    auto: true,
                    unique: false,
                    generated_as: None,
                },
                crate::core::FieldSchema {
                    name: "title",
                    column: "title",
                    ty: FieldType::String,
                    nullable: false,
                    primary_key: false,
                    relation: None,
                    max_length: None,
                    min: None,
                    max: None,
                    default: None,
                    auto: false,
                    unique: false,
                    generated_as: None,
                },
            ])),
            display: None,
            app_label: None,
            admin: None,
            soft_delete_column: None,
            permissions: false,
            audit_track: None,
            m2m: &[],
            indexes: &[],
            check_constraints: &[],
            composite_relations: &[],
            generic_relations: &[],
            scope: crate::core::ModelScope::Tenant,
        }))
    }

    /// Default template name follows the Django convention.
    #[test]
    fn list_view_default_template_matches_table() {
        let lv = ListView::for_model(schema_two_fields());
        assert_eq!(lv.template, "posts_list.html");
        assert_eq!(lv.page_size, 20);
    }

    #[test]
    fn detail_view_default_template_matches_table() {
        let dv = DetailView::for_model(schema_two_fields());
        assert_eq!(dv.template, "posts_detail.html");
    }

    /// Builders chain — every setter returns `Self`.
    #[test]
    fn list_view_builder_chains() {
        let lv = ListView::for_model(schema_two_fields())
            .template("custom.html")
            .page_size(50)
            .order_by("title", false)
            .order_by("id", true)
            .fields(&["id", "title"]);
        assert_eq!(lv.template, "custom.html");
        assert_eq!(lv.page_size, 50);
        assert_eq!(lv.order_by.len(), 2);
        assert_eq!(lv.fields.as_deref().map(<[String]>::len), Some(2));
    }

    /// `page_size(0)` clamps to 1 — empty pages are nonsensical.
    #[test]
    fn list_view_page_size_clamps_to_one() {
        let lv = ListView::for_model(schema_two_fields()).page_size(0);
        assert_eq!(lv.page_size, 1);
    }

    /// `resolve_order_by` accepts a field's Rust name OR its SQL
    /// column name. The viewset uses field names; admin uses
    /// column names; both should work without surprise.
    #[test]
    fn resolve_order_by_accepts_field_or_column_name() {
        let s = schema_two_fields();
        let r = resolve_order_by(s, &[("title".into(), false)]).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].column, "title");
        assert!(!r[0].desc);
    }

    /// Unknown field name surfaces a clear error string instead of
    /// silently dropping the order clause.
    #[test]
    fn resolve_order_by_rejects_unknown_field() {
        let s = schema_two_fields();
        let err = resolve_order_by(s, &[("nope".into(), false)]).unwrap_err();
        assert!(err.contains("`nope`"), "got: {err}");
        assert!(err.contains("posts"), "got: {err}");
    }

    /// `resolved_fields(None)` returns every scalar field.
    #[test]
    fn resolved_fields_default_is_every_scalar() {
        let s = schema_two_fields();
        let fields = resolved_fields(s, None);
        // Schema has 2 fields, both scalar.
        assert_eq!(fields.len(), 2);
    }

    /// Explicit fields list filters down.
    #[test]
    fn resolved_fields_explicit_filters() {
        let s = schema_two_fields();
        let names = ["title".to_owned()];
        let fields = resolved_fields(s, Some(&names));
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].name, "title");
    }

    /// `DeleteView::for_model` produces the Django-convention
    /// confirm-delete template name + a `/` success_url default
    /// (caller almost always overrides to the list URL).
    #[test]
    fn delete_view_default_template_and_success_url() {
        let dv = DeleteView::for_model(schema_two_fields());
        assert_eq!(dv.template, "posts_confirm_delete.html");
        assert_eq!(dv.success_url, "/");
    }

    /// `DeleteView` builder chains override the defaults.
    #[test]
    fn delete_view_builder_chains() {
        let dv = DeleteView::for_model(schema_two_fields())
            .template("custom_delete.html")
            .success_url("/posts")
            .fields(&["id", "title"]);
        assert_eq!(dv.template, "custom_delete.html");
        assert_eq!(dv.success_url, "/posts");
        assert_eq!(dv.fields.as_deref().map(<[String]>::len), Some(2));
    }
}
