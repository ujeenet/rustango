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
//! | [`CreateView`] | GET empty form / POST insert / 303 to `success_url` | `<table>_form.html` |
//! | [`UpdateView`] | GET prefilled form / POST update / 303 | `<table>_form.html` |
//! | [`DeleteView`] | GET confirm / POST delete / 303 | `<table>_confirm_delete.html` |
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
//! ## Pool capture vs per-tenant resolution
//!
//! Each view ships two router constructors:
//!
//! - `.router(prefix, tera, pool)` — single-tenant, captures a
//!   `PgPool` at mount time. Mirrors the original
//!   `ViewSet::router`.
//! - `.tenant_router(prefix, tera)` — multi-tenant, resolves a
//!   per-request connection via the
//!   [`crate::extractors::Tenant`] extractor. Mirrors
//!   `viewset::ViewSet::tenant_router`. Available behind the
//!   combined `template_views` + `tenancy` features.
//!
//! Same builder API across both flavors; pick whichever matches
//! the project's connection-management strategy. Templates port
//! between them without edits.

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
    filter_fields: Vec<String>,
    search_fields: Vec<String>,
}

impl ListView {
    /// Start a `ListView` for the given schema. Defaults: template
    /// name `<table>_list.html`, page size 20, no `ORDER BY`, all
    /// fields included, no filters, no search.
    #[must_use]
    pub fn for_model(schema: &'static ModelSchema) -> Self {
        Self {
            schema,
            template: format!("{}_list.html", schema.table),
            page_size: 20,
            fields: None,
            order_by: Vec::new(),
            filter_fields: Vec::new(),
            search_fields: Vec::new(),
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

    /// Allow exact-match filtering on these fields via URL query
    /// parameters: `GET /posts?author_id=42&status=published` runs
    /// `WHERE author_id = '42' AND status = 'published'` (when both
    /// are in the allowlist; unknown query params are silently
    /// ignored, matching the Django convention).
    ///
    /// Mirrors `viewset::ViewSet::filter_fields` but without the
    /// Django-style `__lookup` syntax (just exact match) — keeps
    /// the ListView surface minimal. Projects that want
    /// `__gt` / `__in` / `__icontains` build their own filters in
    /// a hand-rolled handler.
    ///
    /// Each name resolves against the schema by Rust field name OR
    /// SQL column name; unmatched names are silently dropped at
    /// request time so a typo here doesn't crash startup.
    #[must_use]
    pub fn filter_fields(mut self, names: &[&str]) -> Self {
        self.filter_fields = names.iter().map(|s| (*s).to_owned()).collect();
        self
    }

    /// Enable text search across these fields via the `?search=...`
    /// query parameter: `GET /posts?search=rustango` runs
    /// `WHERE title ILIKE '%rustango%' OR body ILIKE '%rustango%'`
    /// against the listed fields. Mirrors
    /// `viewset::ViewSet::search_fields`.
    ///
    /// Each name should be a `FieldType::String` field; non-string
    /// fields would need a `::text` cast on the SQL side and aren't
    /// handled today (they're silently dropped, matching the viewset).
    #[must_use]
    pub fn search_fields(mut self, names: &[&str]) -> Self {
        self.search_fields = names.iter().map(|s| (*s).to_owned()).collect();
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
    /// Single-tenant pool capture — every request runs against the
    /// same pool. For tenancy projects use [`Self::tenant_router`].
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

    /// Tenant-aware variant — each request resolves its own
    /// connection via the [`crate::extractors::Tenant`] extractor
    /// instead of capturing a single pool at mount time.
    /// Required for multi-tenant projects (subdomain / schema /
    /// per-tenant database). Mirrors `viewset::ViewSet::tenant_router`.
    #[cfg(feature = "tenancy")]
    #[must_use]
    pub fn tenant_router(self, prefix: &str, tera: Arc<Tera>) -> Router<()> {
        let state = Arc::new(TenantListViewState { vs: self, tera });
        Router::new()
            .route(prefix, get(handle_list_tenant))
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
    let where_clause = build_list_where(
        state.vs.schema,
        &state.vs.filter_fields,
        &state.vs.search_fields,
        &params,
    );
    let select_q = SelectQuery {
        model: state.vs.schema,
        where_clause: where_clause.clone(),
        search: None,
        joins: vec![],
        order_by,
        limit: Some(state.vs.page_size),
        offset: Some(offset),
    };
    let count_q = crate::core::CountQuery {
        model: state.vs.schema,
        where_clause,
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
    insert_filter_context(&mut ctx, &state.vs.filter_fields, &params);

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

    /// Tenant-aware variant — see [`ListView::tenant_router`].
    #[cfg(feature = "tenancy")]
    #[must_use]
    pub fn tenant_router(self, prefix: &str, tera: Arc<Tera>) -> Router<()> {
        let state = Arc::new(TenantDetailViewState { vs: self, tera });
        let path = format!("{}/{{pk}}", prefix.trim_end_matches('/'));
        Router::new()
            .route(&path, get(handle_detail_tenant))
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

    /// Tenant-aware variant — see [`ListView::tenant_router`].
    #[cfg(feature = "tenancy")]
    #[must_use]
    pub fn tenant_router(self, prefix: &str, tera: Arc<Tera>) -> Router<()> {
        let state = Arc::new(TenantDeleteViewState { vs: self, tera });
        let path = format!("{}/{{pk}}/delete", prefix.trim_end_matches('/'));
        Router::new()
            .route(
                &path,
                axum::routing::get(handle_delete_confirm_tenant).post(handle_delete_submit_tenant),
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
    headers: axum::http::HeaderMap,
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
    let set_cookie = stamp_csrf(&headers, &mut ctx);
    let mut resp = render(&state.tera, &state.vs.template, &ctx);
    apply_csrf_cookie(&mut resp, set_cookie);
    resp
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

// ============================================================== CreateView

/// `GET <prefix>/new` renders an empty form, `POST <prefix>/new`
/// inserts the row and 303s to `success_url`.
///
/// Form rendering is data-driven: the GET handler stamps a `form`
/// object into the Tera context with typed field metadata
/// (`name`, `column`, `ty`, `required`, `max_length`, `value`) so
/// templates can iterate `{% for field in form.fields %}` and
/// produce whatever HTML they want. The framework doesn't pre-bake
/// field HTML — that's a layout decision projects own.
///
/// POST handling parses `application/x-www-form-urlencoded` bodies,
/// coerces each value to `SqlValue` from the field's declared type,
/// and builds an `InsertQuery` skipping `Auto<T>` PKs (so Postgres'
/// DEFAULT fires). Validation errors render the form back with
/// `errors: { field_name: "message" }` in the context.
///
/// CSRF protection is the project's responsibility — mount under
/// a CSRF-protected scope when the POST is reachable from a browser.
#[derive(Clone)]
pub struct CreateView {
    schema: &'static ModelSchema,
    template: String,
    success_url: String,
    fields: Option<Vec<String>>,
}

impl CreateView {
    /// Start a `CreateView` for the given schema. Defaults: template
    /// `<table>_form.html`, redirect to `/` on success.
    #[must_use]
    pub fn for_model(schema: &'static ModelSchema) -> Self {
        Self {
            schema,
            template: format!("{}_form.html", schema.table),
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
    /// Default `/`. Typical: the list view's URL.
    #[must_use]
    pub fn success_url(mut self, url: impl Into<String>) -> Self {
        self.success_url = url.into();
        self
    }

    /// Restrict which fields appear in the form. Default — every
    /// non-PK, non-`Auto<T>`, non-generated scalar.
    #[must_use]
    pub fn fields(mut self, names: &[&str]) -> Self {
        self.fields = Some(names.iter().map(|s| (*s).to_owned()).collect());
        self
    }

    /// Mount as `GET`/`POST <prefix>/new`.
    #[must_use]
    pub fn router(self, prefix: &str, tera: Arc<Tera>, pool: PgPool) -> Router<()> {
        let state = Arc::new(FormViewState {
            schema: self.schema,
            template: self.template.clone(),
            success_url: self.success_url.clone(),
            fields: self.fields.clone(),
            tera,
            pool,
        });
        let path = format!("{}/new", prefix.trim_end_matches('/'));
        Router::new()
            .route(
                &path,
                axum::routing::get(handle_create_get).post(handle_create_post),
            )
            .with_state(state)
    }

    /// Tenant-aware variant — see [`ListView::tenant_router`].
    #[cfg(feature = "tenancy")]
    #[must_use]
    pub fn tenant_router(self, prefix: &str, tera: Arc<Tera>) -> Router<()> {
        let state = Arc::new(TenantFormViewState {
            schema: self.schema,
            template: self.template,
            success_url: self.success_url,
            fields: self.fields,
            tera,
        });
        let path = format!("{}/new", prefix.trim_end_matches('/'));
        Router::new()
            .route(
                &path,
                axum::routing::get(handle_create_get_tenant).post(handle_create_post_tenant),
            )
            .with_state(state)
    }
}

// ============================================================== UpdateView

/// `GET <prefix>/{pk}/edit` renders a form prefilled from the row;
/// `POST <prefix>/{pk}/edit` updates the row and 303s to `success_url`.
///
/// Same field-rendering and form-parsing rules as [`CreateView`] —
/// templates should be interchangeable. The PK column is read-only
/// (skipped from the form fields) since the URL already pins it.
#[derive(Clone)]
pub struct UpdateView {
    schema: &'static ModelSchema,
    template: String,
    success_url: String,
    fields: Option<Vec<String>>,
}

impl UpdateView {
    #[must_use]
    pub fn for_model(schema: &'static ModelSchema) -> Self {
        Self {
            schema,
            template: format!("{}_form.html", schema.table),
            success_url: "/".to_owned(),
            fields: None,
        }
    }

    #[must_use]
    pub fn template(mut self, name: impl Into<String>) -> Self {
        self.template = name.into();
        self
    }

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

    /// Mount as `GET`/`POST <prefix>/{pk}/edit`.
    #[must_use]
    pub fn router(self, prefix: &str, tera: Arc<Tera>, pool: PgPool) -> Router<()> {
        let state = Arc::new(FormViewState {
            schema: self.schema,
            template: self.template.clone(),
            success_url: self.success_url.clone(),
            fields: self.fields.clone(),
            tera,
            pool,
        });
        let path = format!("{}/{{pk}}/edit", prefix.trim_end_matches('/'));
        Router::new()
            .route(
                &path,
                axum::routing::get(handle_update_get).post(handle_update_post),
            )
            .with_state(state)
    }

    /// Tenant-aware variant — see [`ListView::tenant_router`].
    #[cfg(feature = "tenancy")]
    #[must_use]
    pub fn tenant_router(self, prefix: &str, tera: Arc<Tera>) -> Router<()> {
        let state = Arc::new(TenantFormViewState {
            schema: self.schema,
            template: self.template,
            success_url: self.success_url,
            fields: self.fields,
            tera,
        });
        let path = format!("{}/{{pk}}/edit", prefix.trim_end_matches('/'));
        Router::new()
            .route(
                &path,
                axum::routing::get(handle_update_get_tenant).post(handle_update_post_tenant),
            )
            .with_state(state)
    }
}

// ============================================================== form-view shared

#[derive(Clone)]
struct FormViewState {
    schema: &'static ModelSchema,
    template: String,
    success_url: String,
    fields: Option<Vec<String>>,
    tera: Arc<Tera>,
    pool: PgPool,
}

/// Field metadata stamped into the Tera context for `{% for field
/// in form.fields %}` iteration. Mirrors the relevant subset of
/// [`crate::core::FieldSchema`] in a serde-friendly shape so
/// templates don't have to know the schema types.
#[derive(serde::Serialize)]
struct FormField {
    name: &'static str,
    column: &'static str,
    /// Lowercase string of the SQL type — `"string" | "i32" | "i64"
    /// | "f32" | "f64" | "bool" | "datetime" | "date" | "uuid" |
    /// "json"`. Templates branch on this to pick `<input type=…>`.
    ty: &'static str,
    required: bool,
    max_length: Option<u32>,
    /// Current value as a string ("" on Create, current row value
    /// on Update, or the user's submitted value on a re-rendered
    /// form after validation failure).
    value: String,
}

/// Walk the schema and produce the form-fields slice. Skips:
/// - the primary key (CreateView lets the DB assign; UpdateView
///   pins it from the URL)
/// - `Auto<T>` fields generally (server-assigned)
/// - `generated_as` columns (DB-computed)
/// - relations whose target is a foreign table (FK/M2M handling
///   needs picker UI; templates can render IDs as plain inputs in
///   the meantime — relation fields render with `ty = "i64"` etc.)
fn form_fields(
    schema: &'static ModelSchema,
    explicit: Option<&[String]>,
    values: &HashMap<String, String>,
) -> Vec<FormField> {
    schema
        .fields
        .iter()
        .filter(|f| {
            if f.primary_key || f.auto || f.generated_as.is_some() {
                return false;
            }
            match explicit {
                Some(names) => names.iter().any(|n| n == f.name || n == f.column),
                None => true,
            }
        })
        .map(|f| FormField {
            name: f.name,
            column: f.column,
            ty: field_type_label(f.ty),
            required: !f.nullable,
            max_length: f.max_length,
            value: values
                .get(f.name)
                .or_else(|| values.get(f.column))
                .cloned()
                .unwrap_or_default(),
        })
        .collect()
}

fn field_type_label(ty: crate::core::FieldType) -> &'static str {
    use crate::core::FieldType as T;
    match ty {
        T::String => "string",
        T::I16 => "i16",
        T::I32 => "i32",
        T::I64 => "i64",
        T::F32 => "f32",
        T::F64 => "f64",
        T::Bool => "bool",
        T::DateTime => "datetime",
        T::Date => "date",
        T::Uuid => "uuid",
        T::Json => "json",
    }
}

/// Coerce a form-encoded string into a `SqlValue` based on the
/// field's declared type. Empty strings on nullable fields produce
/// `SqlValue::Null`. Coercion failures surface as a per-field error
/// so the form can re-render with the user's input intact.
fn coerce_value(field: &crate::core::FieldSchema, raw: &str) -> Result<SqlValue, String> {
    use crate::core::FieldType as T;
    if raw.is_empty() && field.nullable {
        return Ok(SqlValue::Null);
    }
    match field.ty {
        T::String => Ok(SqlValue::String(raw.to_owned())),
        T::I16 => raw
            .parse::<i16>()
            .map(|n| SqlValue::I64(i64::from(n)))
            .map_err(|e| format!("expected an integer, got `{raw}` ({e})")),
        T::I32 => raw
            .parse::<i32>()
            .map(|n| SqlValue::I64(i64::from(n)))
            .map_err(|e| format!("expected an integer, got `{raw}` ({e})")),
        T::I64 => raw
            .parse::<i64>()
            .map(SqlValue::I64)
            .map_err(|e| format!("expected an integer, got `{raw}` ({e})")),
        T::F32 => raw
            .parse::<f32>()
            .map(|n| SqlValue::F64(f64::from(n)))
            .map_err(|e| format!("expected a number, got `{raw}` ({e})")),
        T::F64 => raw
            .parse::<f64>()
            .map(SqlValue::F64)
            .map_err(|e| format!("expected a number, got `{raw}` ({e})")),
        T::Bool => match raw {
            "1" | "true" | "on" | "yes" => Ok(SqlValue::Bool(true)),
            "0" | "false" | "off" | "no" | "" => Ok(SqlValue::Bool(false)),
            _ => Err(format!("expected boolean, got `{raw}`")),
        },
        // The rest fall through to String — DB-level casts handle
        // most projects' shapes (datetime / date / uuid all parse
        // cleanly from ISO 8601 / canonical text). Projects that
        // need stricter parsing override via ModelForm.
        _ => Ok(SqlValue::String(raw.to_owned())),
    }
}

async fn handle_create_get(
    State(state): State<Arc<FormViewState>>,
    headers: axum::http::HeaderMap,
) -> Response {
    let mut ctx = Context::new();
    let fields = form_fields(state.schema, state.fields.as_deref(), &HashMap::new());
    ctx.insert(
        "form",
        &serde_json::json!({"fields": fields, "errors": serde_json::Map::new()}),
    );
    ctx.insert("is_create", &true);
    ctx.insert("is_update", &false);
    let set_cookie = stamp_csrf(&headers, &mut ctx);
    let mut resp = render(&state.tera, &state.template, &ctx);
    apply_csrf_cookie(&mut resp, set_cookie);
    resp
}

async fn handle_create_post(
    State(state): State<Arc<FormViewState>>,
    headers: axum::http::HeaderMap,
    axum::Form(form): axum::Form<HashMap<String, String>>,
) -> Response {
    let (columns, values, errors) = parse_form(state.schema, state.fields.as_deref(), &form);
    if !errors.is_empty() {
        return rerender_form(&state, &form, &errors, /*is_update=*/ false, &headers);
    }
    let insert_q = crate::core::InsertQuery {
        model: state.schema,
        columns,
        values,
        returning: vec![],
        on_conflict: None,
    };
    match crate::sql::insert(&state.pool, &insert_q).await {
        Ok(()) => axum::response::Redirect::to(&state.success_url).into_response(),
        Err(e) => template_error(&format!("insert row: {e}")),
    }
}

async fn handle_update_get(
    State(state): State<Arc<FormViewState>>,
    Path(pk): Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(pk_field) = state.schema.primary_key() else {
        return template_error(&format!(
            "model `{}` has no primary key — UpdateView can't probe by PK",
            state.schema.table
        ));
    };
    let select_q = SelectQuery {
        model: state.schema,
        where_clause: WhereExpr::Predicate(Filter {
            column: pk_field.column,
            op: Op::Eq,
            value: SqlValue::String(pk.clone()),
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
    let scalars: Vec<&'static crate::core::FieldSchema> = state.schema.scalar_fields().collect();
    let row_json = row_to_json(&row, &scalars);
    // Convert the row's JSON object into a string-keyed string-valued
    // HashMap so `form_fields` can pick up the existing values.
    let row_obj = row_json.as_object().cloned().unwrap_or_default();
    let mut values: HashMap<String, String> = HashMap::with_capacity(row_obj.len());
    for (k, v) in row_obj {
        let s = match v {
            serde_json::Value::Null => String::new(),
            serde_json::Value::String(s) => s,
            other => other.to_string(),
        };
        values.insert(k, s);
    }
    let fields = form_fields(state.schema, state.fields.as_deref(), &values);
    let mut ctx = Context::new();
    ctx.insert(
        "form",
        &serde_json::json!({"fields": fields, "errors": serde_json::Map::new()}),
    );
    ctx.insert("object", &row_json);
    ctx.insert("pk", &pk);
    ctx.insert("is_create", &false);
    ctx.insert("is_update", &true);
    let set_cookie = stamp_csrf(&headers, &mut ctx);
    let mut resp = render(&state.tera, &state.template, &ctx);
    apply_csrf_cookie(&mut resp, set_cookie);
    resp
}

async fn handle_update_post(
    State(state): State<Arc<FormViewState>>,
    Path(pk): Path<String>,
    headers: axum::http::HeaderMap,
    axum::Form(form): axum::Form<HashMap<String, String>>,
) -> Response {
    let Some(pk_field) = state.schema.primary_key() else {
        return template_error(&format!(
            "model `{}` has no primary key — UpdateView can't update by PK",
            state.schema.table
        ));
    };
    let (columns, values, errors) = parse_form(state.schema, state.fields.as_deref(), &form);
    if !errors.is_empty() {
        return rerender_form(&state, &form, &errors, /*is_update=*/ true, &headers);
    }
    let assignments: Vec<crate::core::Assignment> = columns
        .into_iter()
        .zip(values)
        .map(|(column, value)| crate::core::Assignment { column, value })
        .collect();
    let update_q = crate::core::UpdateQuery {
        model: state.schema,
        set: assignments,
        where_clause: WhereExpr::Predicate(Filter {
            column: pk_field.column,
            op: Op::Eq,
            value: SqlValue::String(pk),
        }),
    };
    match crate::sql::update(&state.pool, &update_q).await {
        Ok(0) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Ok(_) => axum::response::Redirect::to(&state.success_url).into_response(),
        Err(e) => template_error(&format!("update row: {e}")),
    }
}

/// Walk the form submission and produce `(columns, values, errors)`.
/// `errors` is non-empty only when at least one field failed to
/// coerce. Form-level validation (required-field checks) happens
/// here too — empty non-nullable fields surface as
/// `"this field is required"`.
fn parse_form(
    schema: &'static ModelSchema,
    explicit: Option<&[String]>,
    submitted: &HashMap<String, String>,
) -> (Vec<&'static str>, Vec<SqlValue>, HashMap<String, String>) {
    let mut columns: Vec<&'static str> = Vec::new();
    let mut values: Vec<SqlValue> = Vec::new();
    let mut errors: HashMap<String, String> = HashMap::new();
    for f in schema.fields {
        if f.primary_key || f.auto || f.generated_as.is_some() {
            continue;
        }
        if let Some(names) = explicit {
            if !names.iter().any(|n| n == f.name || n == f.column) {
                continue;
            }
        }
        let raw = submitted
            .get(f.name)
            .or_else(|| submitted.get(f.column))
            .cloned()
            .unwrap_or_default();
        if raw.is_empty() && !f.nullable && !matches!(f.ty, crate::core::FieldType::Bool) {
            // Bool checkboxes legitimately submit nothing when
            // unchecked — treat that as `false` rather than
            // "required missing".
            errors.insert(f.name.to_owned(), "this field is required".to_owned());
            continue;
        }
        match coerce_value(f, &raw) {
            Ok(v) => {
                // Bounds validation — `max_length` / `min` / `max`
                // declared on the schema. Surface as a per-field
                // form error so the user can fix it without an
                // insert/update round-trip.
                if let Err(e) = crate::core::validate_value(schema.name, f, &v) {
                    errors.insert(f.name.to_owned(), bounds_error_message(&e));
                    continue;
                }
                columns.push(f.column);
                values.push(v);
            }
            Err(e) => {
                errors.insert(f.name.to_owned(), e);
            }
        }
    }
    (columns, values, errors)
}

/// Render a [`crate::core::QueryError`] from `validate_value` as a
/// user-friendly form error string. The Display impl is fine for
/// logs but leaks `model.field` framing that's noise in the UI;
/// this strips that down to the bounds-side message.
fn bounds_error_message(e: &crate::core::QueryError) -> String {
    use crate::core::QueryError;
    match e {
        QueryError::MaxLengthExceeded { max, actual, .. } => {
            format!("must be {max} characters or fewer (got {actual})")
        }
        QueryError::OutOfRange {
            min, max, value, ..
        } => match (min, max) {
            (Some(lo), Some(hi)) => format!("must be between {lo} and {hi} (got {value})"),
            (Some(lo), None) => format!("must be ≥ {lo} (got {value})"),
            (None, Some(hi)) => format!("must be ≤ {hi} (got {value})"),
            (None, None) => format!("invalid value: {value}"),
        },
        // Other variants aren't produced by validate_value; surface
        // the framework's Display string as a fallback so the user
        // sees something actionable rather than an empty message.
        other => other.to_string(),
    }
}

/// Re-render the form template after a validation failure with the
/// user's submitted values + per-field errors. Mirrors Django's
/// "render with errors" pattern so the user doesn't lose what they
/// typed.
fn rerender_form(
    state: &FormViewState,
    submitted: &HashMap<String, String>,
    errors: &HashMap<String, String>,
    is_update: bool,
    headers: &axum::http::HeaderMap,
) -> Response {
    let fields = form_fields(state.schema, state.fields.as_deref(), submitted);
    let mut ctx = Context::new();
    ctx.insert(
        "form",
        &serde_json::json!({"fields": fields, "errors": errors}),
    );
    ctx.insert("is_create", &!is_update);
    ctx.insert("is_update", &is_update);
    // The user POST'd here from an earlier GET, so the CSRF cookie
    // is almost always already present. Stamp the same token back
    // into the context so the re-rendered form's hidden input
    // matches what the browser will send on the next attempt.
    let set_cookie = stamp_csrf(headers, &mut ctx);
    let mut resp = render(&state.tera, &state.template, &ctx);
    *resp.status_mut() = StatusCode::UNPROCESSABLE_ENTITY;
    apply_csrf_cookie(&mut resp, set_cookie);
    resp
}

// ============================================================== shared helpers

/// Resolve a `Vec<(name, desc)>` into the static-string `OrderClause`
/// shape the SQL writer expects. Returns the original column name in
/// the error string when it doesn't match any field.
///
/// **Stable-pagination guarantee**: when `spec` is empty, falls back
/// to `ORDER BY <pk> ASC` so paginated [`ListView`] doesn't return
/// rows in arbitrary Postgres-internal order (which would make
/// page 2 overlap page 1 between requests). Models without a PK
/// still get an empty `ORDER BY` — there's no canonical column to
/// pick — but the paginated views warn-log when that happens.
fn resolve_order_by(
    schema: &'static ModelSchema,
    spec: &[(String, bool)],
) -> Result<Vec<OrderClause>, String> {
    if spec.is_empty() {
        return Ok(default_order_by(schema));
    }
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

/// PK-based fallback ordering for paginated views without an
/// explicit `.order_by(...)`. Postgres doesn't guarantee any
/// particular row order without `ORDER BY` — between two requests,
/// the same query can return rows in different order, so page 2
/// might have rows that already appeared on page 1. Defaulting to
/// `<pk> ASC` is cheap (the PK is indexed) and deterministic.
///
/// Models without a primary key fall through to an empty clause —
/// the application is on its own (and pagination on a PK-less model
/// is unusual anyway).
fn default_order_by(schema: &'static ModelSchema) -> Vec<OrderClause> {
    match schema.primary_key() {
        Some(pk) => vec![OrderClause {
            column: pk.column,
            desc: false,
        }],
        None => Vec::new(),
    }
}

/// Build the `WHERE` clause for a [`ListView`] handler from URL
/// query parameters + the configured `filter_fields` /
/// `search_fields` allowlists.
///
/// Filter shape: `?<field>=<value>` — exact match (`Op::Eq`) only.
/// Unknown query params (anything not in `filter_fields`, plus the
/// reserved `page` / `page_size` / `search`) are silently ignored,
/// so a typo in the URL doesn't 400 the request.
///
/// Search shape: `?search=<query>` — `ILIKE '%<query>%'` against
/// each `search_field`, OR-combined. The search predicates land
/// directly in the WHERE clause (rather than the IR's separate
/// `SearchClause`) so `SelectQuery` and `CountQuery` see them
/// equally — pagination's `total_pages` reflects the searched
/// subset, not the unsearched total.
///
/// The `%` and `_` characters in the user's search input are
/// escaped via the framework's `escape_like_pattern` so they
/// match literally rather than acting as wildcards. This matches
/// the viewset's behavior (defense against pattern injection).
fn build_list_where(
    schema: &'static ModelSchema,
    filter_fields: &[String],
    search_fields: &[String],
    params: &HashMap<String, String>,
) -> WhereExpr {
    use crate::core::Filter;

    let mut predicates: Vec<WhereExpr> = Vec::new();

    // Exact-match filters from `?field=value` query params.
    for (key, val) in params {
        if matches!(key.as_str(), "page" | "page_size" | "search") {
            continue;
        }
        if !filter_fields.iter().any(|f| f == key) {
            continue;
        }
        let Some(field) = schema.field(key) else {
            continue;
        };
        predicates.push(WhereExpr::Predicate(Filter {
            column: field.column,
            op: Op::Eq,
            value: SqlValue::String(val.clone()),
        }));
    }

    // ILIKE search across `search_fields`, OR-combined.
    if let Some(q) = params.get("search").filter(|s| !s.is_empty()) {
        let escaped = escape_like_pattern(q);
        let pattern = format!("%{escaped}%");
        let mut or_branches: Vec<WhereExpr> = Vec::new();
        for name in search_fields {
            if let Some(field) = schema.field(name) {
                or_branches.push(WhereExpr::Predicate(Filter {
                    column: field.column,
                    op: Op::ILike,
                    value: SqlValue::String(pattern.clone()),
                }));
            }
        }
        match or_branches.len() {
            0 => {}
            1 => predicates.push(or_branches.remove(0)),
            _ => predicates.push(WhereExpr::Or(or_branches)),
        }
    }

    if predicates.is_empty() {
        WhereExpr::And(vec![])
    } else if predicates.len() == 1 {
        predicates.remove(0)
    } else {
        WhereExpr::And(predicates)
    }
}

/// Escape `%` and `_` so the user's search input matches literally
/// in `LIKE` / `ILIKE` rather than acting as wildcards. Mirrors
/// what the viewset does with user input.
fn escape_like_pattern(input: &str) -> String {
    input
        .replace('\\', r"\\")
        .replace('%', r"\%")
        .replace('_', r"\_")
}

/// Read or mint a CSRF token and stamp it into the Tera context as
/// `csrf_token`. Returns the optional `Set-Cookie` header value the
/// caller should attach to the response when the cookie was missing
/// (so the first-ever GET to the form doesn't render an empty
/// token, which would make the subsequent POST fail CSRF
/// validation).
///
/// Without the `csrf` feature compiled in, this is a no-op:
/// `csrf_token` is stamped as an empty string and `None` is
/// returned. The `<input type="hidden" name="_csrf" value="">`
/// in the rendered HTML is harmless — CSRF validation isn't
/// enforced when the feature is off.
fn stamp_csrf(_headers: &axum::http::HeaderMap, ctx: &mut Context) -> Option<String> {
    #[cfg(feature = "csrf")]
    {
        let (token, set_cookie) =
            crate::forms::csrf::ensure_token(_headers, crate::forms::csrf::CSRF_COOKIE);
        ctx.insert("csrf_token", &token);
        set_cookie
    }
    #[cfg(not(feature = "csrf"))]
    {
        ctx.insert("csrf_token", "");
        None
    }
}

/// Append a `Set-Cookie` header to a ready response when
/// [`stamp_csrf`] minted a fresh token. No-op for the common case
/// where the cookie was already present.
fn apply_csrf_cookie(resp: &mut Response, set_cookie: Option<String>) {
    let Some(c) = set_cookie else { return };
    if let Ok(hv) = axum::http::HeaderValue::from_str(&c) {
        resp.headers_mut()
            .append(axum::http::header::SET_COOKIE, hv);
    }
}

/// Stamp the active filter values + search query back into the
/// Tera context so templates can repopulate filter form inputs.
///
/// Stamps two top-level vars:
/// - `filters: Map<String, String>` — only fields in the
///   `filter_fields` allowlist that the user actually supplied
///   a value for. Empty map when no filter was active.
/// - `search: String` — the active `?search=` value, or `""`
///   when unset / empty.
///
/// Templates can then mark dropdowns / re-fill inputs:
///
/// ```html
/// <input name="search" value="{{ search }}">
/// <select name="status">
///   <option value="published"
///     {% if filters.status == "published" %}selected{% endif %}>
///     Published
///   </option>
/// </select>
/// ```
pub(super) fn insert_filter_context(
    ctx: &mut Context,
    filter_fields: &[String],
    params: &HashMap<String, String>,
) {
    let filters: HashMap<&str, &str> = params
        .iter()
        .filter(|(k, _)| filter_fields.iter().any(|f| f == *k))
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    ctx.insert("filters", &filters);

    let search = params.get("search").map(String::as_str).unwrap_or_default();
    ctx.insert("search", search);
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

// ============================================================== tenant variants

/// Tenant-aware state structs + handlers (#A5 follow-up). Each
/// mirrors its single-tenant sibling but drops the captured pool —
/// the [`crate::extractors::Tenant`] extractor resolves a per-
/// request connection that handlers query via `t.conn()` against
/// the `_on` SQL helpers.
///
/// Mirrors `viewset::tenant_router` so projects can mix
/// `template_views::ListView::tenant_router(...)` and
/// `viewset::ViewSet::for_model(...).tenant_router(...)` in the
/// same `Router` without thinking about pool plumbing.
#[cfg(feature = "tenancy")]
mod tenant {
    use super::*;
    use crate::extractors::Tenant;

    // ---------- ListView ----------

    #[derive(Clone)]
    pub(super) struct TenantListViewState {
        pub(super) vs: ListView,
        pub(super) tera: Arc<Tera>,
    }

    pub(super) async fn handle_list_tenant(
        State(state): State<Arc<TenantListViewState>>,
        Query(params): Query<HashMap<String, String>>,
        mut t: Tenant,
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
        let where_clause = build_list_where(
            state.vs.schema,
            &state.vs.filter_fields,
            &state.vs.search_fields,
            &params,
        );
        let select_q = SelectQuery {
            model: state.vs.schema,
            where_clause: where_clause.clone(),
            search: None,
            joins: vec![],
            order_by,
            limit: Some(state.vs.page_size),
            offset: Some(offset),
        };
        let count_q = crate::core::CountQuery {
            model: state.vs.schema,
            where_clause,
        };

        let conn = t.conn();
        // Run sequentially — `Tenant::conn()` hands out a `&mut`
        // exclusive borrow, so we can't fan out the two queries
        // in parallel like the pool path does. The latency hit is
        // bounded by tokio's task switch.
        let rows = match crate::sql::select_rows_on(&mut *conn, &select_q).await {
            Ok(r) => r,
            Err(e) => return template_error(&format!("query rows: {e}")),
        };
        let total = match crate::sql::count_rows_on(&mut *conn, &count_q).await {
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
        super::insert_filter_context(&mut ctx, &state.vs.filter_fields, &params);

        render(&state.tera, &state.vs.template, &ctx)
    }

    // ---------- DetailView ----------

    #[derive(Clone)]
    pub(super) struct TenantDetailViewState {
        pub(super) vs: DetailView,
        pub(super) tera: Arc<Tera>,
    }

    pub(super) async fn handle_detail_tenant(
        State(state): State<Arc<TenantDetailViewState>>,
        Path(pk): Path<String>,
        mut t: Tenant,
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
        let row = match crate::sql::select_one_row_on(&mut *t.conn(), &select_q).await {
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

    // ---------- DeleteView ----------

    #[derive(Clone)]
    pub(super) struct TenantDeleteViewState {
        pub(super) vs: DeleteView,
        pub(super) tera: Arc<Tera>,
    }

    pub(super) async fn handle_delete_confirm_tenant(
        State(state): State<Arc<TenantDeleteViewState>>,
        Path(pk): Path<String>,
        headers: axum::http::HeaderMap,
        mut t: Tenant,
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
        let row = match crate::sql::select_one_row_on(&mut *t.conn(), &select_q).await {
            Ok(Some(r)) => r,
            Ok(None) => return (StatusCode::NOT_FOUND, "not found").into_response(),
            Err(e) => return template_error(&format!("query row: {e}")),
        };
        let fields = resolved_fields(state.vs.schema, state.vs.fields.as_deref());
        let object = row_to_json(&row, &fields);
        let mut ctx = Context::new();
        ctx.insert("object", &object);
        let set_cookie = super::stamp_csrf(&headers, &mut ctx);
        let mut resp = render(&state.tera, &state.vs.template, &ctx);
        super::apply_csrf_cookie(&mut resp, set_cookie);
        resp
    }

    pub(super) async fn handle_delete_submit_tenant(
        State(state): State<Arc<TenantDeleteViewState>>,
        Path(pk): Path<String>,
        mut t: Tenant,
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
        match crate::sql::delete_on(&mut *t.conn(), &delete_q).await {
            Ok(0) => (StatusCode::NOT_FOUND, "not found").into_response(),
            Ok(_) => axum::response::Redirect::to(&state.vs.success_url).into_response(),
            Err(e) => template_error(&format!("delete row: {e}")),
        }
    }

    // ---------- CreateView / UpdateView ----------

    #[derive(Clone)]
    pub(super) struct TenantFormViewState {
        pub(super) schema: &'static ModelSchema,
        pub(super) template: String,
        pub(super) success_url: String,
        pub(super) fields: Option<Vec<String>>,
        pub(super) tera: Arc<Tera>,
    }

    pub(super) async fn handle_create_get_tenant(
        State(state): State<Arc<TenantFormViewState>>,
        headers: axum::http::HeaderMap,
    ) -> Response {
        let mut ctx = Context::new();
        let fields = form_fields(state.schema, state.fields.as_deref(), &HashMap::new());
        ctx.insert(
            "form",
            &serde_json::json!({"fields": fields, "errors": serde_json::Map::new()}),
        );
        ctx.insert("is_create", &true);
        ctx.insert("is_update", &false);
        let set_cookie = super::stamp_csrf(&headers, &mut ctx);
        let mut resp = render(&state.tera, &state.template, &ctx);
        super::apply_csrf_cookie(&mut resp, set_cookie);
        resp
    }

    pub(super) async fn handle_create_post_tenant(
        State(state): State<Arc<TenantFormViewState>>,
        headers: axum::http::HeaderMap,
        mut t: Tenant,
        axum::Form(form): axum::Form<HashMap<String, String>>,
    ) -> Response {
        let (columns, values, errors) = parse_form(state.schema, state.fields.as_deref(), &form);
        if !errors.is_empty() {
            return rerender_form_tenant(
                &state, &form, &errors, /*is_update=*/ false, &headers,
            );
        }
        let insert_q = crate::core::InsertQuery {
            model: state.schema,
            columns,
            values,
            returning: vec![],
            on_conflict: None,
        };
        match crate::sql::insert_on(&mut *t.conn(), &insert_q).await {
            Ok(()) => axum::response::Redirect::to(&state.success_url).into_response(),
            Err(e) => template_error(&format!("insert row: {e}")),
        }
    }

    pub(super) async fn handle_update_get_tenant(
        State(state): State<Arc<TenantFormViewState>>,
        Path(pk): Path<String>,
        headers: axum::http::HeaderMap,
        mut t: Tenant,
    ) -> Response {
        let Some(pk_field) = state.schema.primary_key() else {
            return template_error(&format!(
                "model `{}` has no primary key — UpdateView can't probe by PK",
                state.schema.table
            ));
        };
        let select_q = SelectQuery {
            model: state.schema,
            where_clause: WhereExpr::Predicate(Filter {
                column: pk_field.column,
                op: Op::Eq,
                value: SqlValue::String(pk.clone()),
            }),
            search: None,
            joins: vec![],
            order_by: vec![],
            limit: Some(1),
            offset: None,
        };
        let row = match crate::sql::select_one_row_on(&mut *t.conn(), &select_q).await {
            Ok(Some(r)) => r,
            Ok(None) => return (StatusCode::NOT_FOUND, "not found").into_response(),
            Err(e) => return template_error(&format!("query row: {e}")),
        };
        let scalars: Vec<&'static crate::core::FieldSchema> =
            state.schema.scalar_fields().collect();
        let row_json = row_to_json(&row, &scalars);
        let row_obj = row_json.as_object().cloned().unwrap_or_default();
        let mut values: HashMap<String, String> = HashMap::with_capacity(row_obj.len());
        for (k, v) in row_obj {
            let s = match v {
                serde_json::Value::Null => String::new(),
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            };
            values.insert(k, s);
        }
        let fields = form_fields(state.schema, state.fields.as_deref(), &values);
        let mut ctx = Context::new();
        ctx.insert(
            "form",
            &serde_json::json!({"fields": fields, "errors": serde_json::Map::new()}),
        );
        ctx.insert("object", &row_json);
        ctx.insert("pk", &pk);
        ctx.insert("is_create", &false);
        ctx.insert("is_update", &true);
        let set_cookie = super::stamp_csrf(&headers, &mut ctx);
        let mut resp = render(&state.tera, &state.template, &ctx);
        super::apply_csrf_cookie(&mut resp, set_cookie);
        resp
    }

    pub(super) async fn handle_update_post_tenant(
        State(state): State<Arc<TenantFormViewState>>,
        Path(pk): Path<String>,
        headers: axum::http::HeaderMap,
        mut t: Tenant,
        axum::Form(form): axum::Form<HashMap<String, String>>,
    ) -> Response {
        let Some(pk_field) = state.schema.primary_key() else {
            return template_error(&format!(
                "model `{}` has no primary key — UpdateView can't update by PK",
                state.schema.table
            ));
        };
        let (columns, values, errors) = parse_form(state.schema, state.fields.as_deref(), &form);
        if !errors.is_empty() {
            return rerender_form_tenant(
                &state, &form, &errors, /*is_update=*/ true, &headers,
            );
        }
        let assignments: Vec<crate::core::Assignment> = columns
            .into_iter()
            .zip(values)
            .map(|(column, value)| crate::core::Assignment { column, value })
            .collect();
        let update_q = crate::core::UpdateQuery {
            model: state.schema,
            set: assignments,
            where_clause: WhereExpr::Predicate(Filter {
                column: pk_field.column,
                op: Op::Eq,
                value: SqlValue::String(pk),
            }),
        };
        match crate::sql::update_on(&mut *t.conn(), &update_q).await {
            Ok(0) => (StatusCode::NOT_FOUND, "not found").into_response(),
            Ok(_) => axum::response::Redirect::to(&state.success_url).into_response(),
            Err(e) => template_error(&format!("update row: {e}")),
        }
    }

    fn rerender_form_tenant(
        state: &TenantFormViewState,
        submitted: &HashMap<String, String>,
        errors: &HashMap<String, String>,
        is_update: bool,
        headers: &axum::http::HeaderMap,
    ) -> Response {
        let fields = form_fields(state.schema, state.fields.as_deref(), submitted);
        let mut ctx = Context::new();
        ctx.insert(
            "form",
            &serde_json::json!({"fields": fields, "errors": errors}),
        );
        ctx.insert("is_create", &!is_update);
        ctx.insert("is_update", &is_update);
        let set_cookie = super::stamp_csrf(headers, &mut ctx);
        let mut resp = render(&state.tera, &state.template, &ctx);
        *resp.status_mut() = StatusCode::UNPROCESSABLE_ENTITY;
        super::apply_csrf_cookie(&mut resp, set_cookie);
        resp
    }
}

#[cfg(feature = "tenancy")]
use tenant::{
    handle_create_get_tenant, handle_create_post_tenant, handle_delete_confirm_tenant,
    handle_delete_submit_tenant, handle_detail_tenant, handle_list_tenant,
    handle_update_get_tenant, handle_update_post_tenant, TenantDeleteViewState,
    TenantDetailViewState, TenantFormViewState, TenantListViewState,
};

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

    /// Schema with declared bounds — `max_length = 5` on the
    /// title, `min = 0 / max = 100` on a score field. Used by the
    /// bounds-validation tests.
    fn schema_with_bounds() -> &'static ModelSchema {
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
                    max_length: Some(5),
                    min: None,
                    max: None,
                    default: None,
                    auto: false,
                    unique: false,
                    generated_as: None,
                },
                crate::core::FieldSchema {
                    name: "score",
                    column: "score",
                    ty: FieldType::I32,
                    nullable: false,
                    primary_key: false,
                    relation: None,
                    max_length: None,
                    min: Some(0),
                    max: Some(100),
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

    /// Empty `order_by` falls back to PK-ASC so paginated views
    /// return rows in deterministic order (no page overlap between
    /// requests).
    #[test]
    fn resolve_order_by_empty_falls_back_to_pk() {
        let s = schema_two_fields();
        let out = resolve_order_by(s, &[]).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].column, "id");
        assert!(!out[0].desc, "PK fallback is ASC");
    }

    /// `ListView` builder accepts filter_fields + search_fields.
    #[test]
    fn list_view_filter_and_search_chain() {
        let lv = ListView::for_model(schema_two_fields())
            .filter_fields(&["title"])
            .search_fields(&["title"]);
        assert_eq!(lv.filter_fields, vec!["title".to_owned()]);
        assert_eq!(lv.search_fields, vec!["title".to_owned()]);
    }

    /// No filter params, no search → empty `WhereExpr::And(vec![])`.
    #[test]
    fn build_list_where_empty_params_returns_empty_and() {
        let s = schema_two_fields();
        let where_clause = build_list_where(s, &["title".into()], &[], &HashMap::new());
        match where_clause {
            WhereExpr::And(v) => assert!(v.is_empty()),
            other => panic!("expected empty And, got {other:?}"),
        }
    }

    /// Filter param matching the allowlist produces a single
    /// `Predicate(Eq)` predicate.
    #[test]
    fn build_list_where_filter_field_in_allowlist() {
        let s = schema_two_fields();
        let mut params = HashMap::new();
        params.insert("title".to_owned(), "Hello".to_owned());
        let where_clause = build_list_where(s, &["title".into()], &[], &params);
        match where_clause {
            WhereExpr::Predicate(f) => {
                assert_eq!(f.column, "title");
                assert_eq!(f.op, Op::Eq);
            }
            other => panic!("expected single Predicate, got {other:?}"),
        }
    }

    /// Filter params NOT in the allowlist are silently dropped —
    /// matches Django's behavior (typos shouldn't 400).
    #[test]
    fn build_list_where_unknown_field_ignored() {
        let s = schema_two_fields();
        let mut params = HashMap::new();
        params.insert("category".to_owned(), "tech".to_owned()); // not in allowlist
        let where_clause = build_list_where(s, &["title".into()], &[], &params);
        match where_clause {
            WhereExpr::And(v) => assert!(v.is_empty()),
            other => panic!("expected empty And, got {other:?}"),
        }
    }

    /// Reserved keys (`page`, `page_size`, `search`) are skipped
    /// even if a `filter_fields` allowlist names them.
    #[test]
    fn build_list_where_reserved_keys_skipped() {
        let s = schema_two_fields();
        let mut params = HashMap::new();
        params.insert("page".to_owned(), "2".to_owned());
        params.insert("page_size".to_owned(), "50".to_owned());
        let where_clause = build_list_where(s, &["page".into(), "page_size".into()], &[], &params);
        match where_clause {
            WhereExpr::And(v) => assert!(v.is_empty()),
            other => panic!("expected empty And, got {other:?}"),
        }
    }

    /// `?search=foo` on a single search_field produces a single
    /// `Predicate(ILike)` (no OR wrapper for one branch).
    #[test]
    fn build_list_where_search_single_field() {
        let s = schema_two_fields();
        let mut params = HashMap::new();
        params.insert("search".to_owned(), "Hello".to_owned());
        let where_clause = build_list_where(s, &[], &["title".into()], &params);
        match where_clause {
            WhereExpr::Predicate(f) => {
                assert_eq!(f.column, "title");
                assert_eq!(f.op, Op::ILike);
                if let SqlValue::String(p) = f.value {
                    assert!(p.contains("Hello"), "got: {p}");
                    assert!(p.starts_with('%') && p.ends_with('%'), "got: {p}");
                } else {
                    panic!("expected SqlValue::String");
                }
            }
            other => panic!("expected single Predicate, got {other:?}"),
        }
    }

    /// Search with multiple fields produces an OR-combined branch.
    /// Filter + search together AND-combine at the top level.
    #[test]
    fn build_list_where_filter_plus_search_and_combined() {
        let s = schema_with_bounds(); // has id, title, score
        let mut params = HashMap::new();
        params.insert("title".to_owned(), "Hello".to_owned()); // filter
        params.insert("search".to_owned(), "world".to_owned()); // search
        let where_clause = build_list_where(
            s,
            &["title".into()],
            &["title".into(), "score".into()],
            &params,
        );
        match where_clause {
            WhereExpr::And(branches) => {
                assert_eq!(branches.len(), 2, "expected filter AND search");
            }
            other => panic!("expected And, got {other:?}"),
        }
    }

    /// `%` and `_` in user input get escaped so they match
    /// literally rather than acting as `LIKE` wildcards.
    #[test]
    fn escape_like_pattern_neutralizes_wildcards() {
        assert_eq!(escape_like_pattern("100%"), r"100\%");
        assert_eq!(escape_like_pattern("foo_bar"), r"foo\_bar");
        assert_eq!(escape_like_pattern(r"a\b"), r"a\\b");
        assert_eq!(escape_like_pattern("plain"), "plain");
    }

    /// Empty `?search=` is treated as "no search" — different from
    /// "search for empty string". Otherwise navigating from a
    /// search-active page back to the unfiltered list (via a link
    /// with `?search=`) would still apply ILIKE '%%' and miss any
    /// rows where the field is NULL.
    #[test]
    fn build_list_where_empty_search_param_skipped() {
        let s = schema_two_fields();
        let mut params = HashMap::new();
        params.insert("search".to_owned(), String::new());
        let where_clause = build_list_where(s, &[], &["title".into()], &params);
        match where_clause {
            WhereExpr::And(v) => assert!(v.is_empty()),
            other => panic!("expected empty And, got {other:?}"),
        }
    }

    /// `insert_filter_context` stamps `filters` (allowlisted-only)
    /// and `search` into the Tera context so templates can
    /// repopulate filter form inputs.
    #[test]
    fn insert_filter_context_stamps_active_values() {
        let mut ctx = Context::new();
        let mut params = HashMap::new();
        params.insert("status".to_owned(), "published".to_owned());
        params.insert("category".to_owned(), "tech".to_owned()); // not in allowlist
        params.insert("search".to_owned(), "rustango".to_owned());
        params.insert("page".to_owned(), "2".to_owned()); // never a filter
        insert_filter_context(&mut ctx, &["status".into()], &params);

        // Tera's Context doesn't expose direct read; serialize round-trip
        // via the rendered template. `category` was outside the
        // allowlist so it shouldn't appear in the filters map.
        let mut tera = Tera::default();
        tera.add_raw_template(
            "t",
            "{{ filters.status }}|{{ search }}|{{ filters | length }}",
        )
        .unwrap();
        let rendered = tera.render("t", &ctx).unwrap();
        assert_eq!(rendered, "published|rustango|1");
    }

    /// `stamp_csrf` reads the existing CSRF cookie when present
    /// and stamps the same value into the Tera context (so the
    /// rendered hidden input matches what the browser will send
    /// back on POST). Returns `None` for the Set-Cookie since the
    /// cookie was already there.
    #[cfg(feature = "csrf")]
    #[test]
    fn stamp_csrf_reuses_existing_cookie() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_static("session=abc; rustango_csrf=existing-token"),
        );
        let mut ctx = Context::new();
        let set_cookie = stamp_csrf(&headers, &mut ctx);
        assert!(
            set_cookie.is_none(),
            "no Set-Cookie when cookie was present"
        );
        let mut tera = Tera::default();
        tera.add_raw_template("t", "{{ csrf_token }}").unwrap();
        let rendered = tera.render("t", &ctx).unwrap();
        assert_eq!(rendered, "existing-token");
    }

    /// `stamp_csrf` mints a fresh token when the cookie is absent
    /// and returns the Set-Cookie header for the caller to attach.
    #[cfg(feature = "csrf")]
    #[test]
    fn stamp_csrf_mints_fresh_when_absent() {
        let headers = axum::http::HeaderMap::new();
        let mut ctx = Context::new();
        let set_cookie = stamp_csrf(&headers, &mut ctx);
        let cookie = set_cookie.expect("Set-Cookie returned when cookie absent");
        assert!(cookie.starts_with("rustango_csrf="), "got: {cookie}");
        // The token in the context matches what's in the Set-Cookie.
        let token_in_cookie = cookie
            .split_once('=')
            .and_then(|(_, rest)| rest.split(';').next())
            .unwrap();
        let mut tera = Tera::default();
        tera.add_raw_template("t", "{{ csrf_token }}").unwrap();
        let rendered = tera.render("t", &ctx).unwrap();
        assert_eq!(rendered, token_in_cookie);
        // Token shape: 32 random bytes → base64url no-pad → 43 chars.
        assert_eq!(rendered.len(), 43);
    }

    /// Without the `csrf` feature, `stamp_csrf` is a no-op that
    /// stamps an empty `csrf_token`. The hidden input renders as
    /// `<input value="">` — harmless when CSRF isn't enforced.
    #[cfg(not(feature = "csrf"))]
    #[test]
    fn stamp_csrf_noop_when_feature_off() {
        let headers = axum::http::HeaderMap::new();
        let mut ctx = Context::new();
        let set_cookie = stamp_csrf(&headers, &mut ctx);
        assert!(set_cookie.is_none());
        let mut tera = Tera::default();
        tera.add_raw_template("t", "{{ csrf_token }}").unwrap();
        assert_eq!(tera.render("t", &ctx).unwrap(), "");
    }

    /// `apply_csrf_cookie` appends a Set-Cookie header when given
    /// `Some(value)`, no-op for `None`.
    #[test]
    fn apply_csrf_cookie_appends_when_some() {
        let mut resp = (StatusCode::OK, "ok").into_response();
        apply_csrf_cookie(&mut resp, Some("rustango_csrf=tok; Path=/".into()));
        let cookies: Vec<_> = resp
            .headers()
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .collect();
        assert_eq!(cookies.len(), 1);
        assert!(cookies[0].to_str().unwrap().contains("rustango_csrf=tok"));

        // None branch — no header added.
        let mut resp = (StatusCode::OK, "ok").into_response();
        apply_csrf_cookie(&mut resp, None);
        assert!(resp.headers().get(axum::http::header::SET_COOKIE).is_none());
    }

    /// No filters/search → `filters` empty, `search` empty string.
    #[test]
    fn insert_filter_context_empty_params_yields_empty_values() {
        let mut ctx = Context::new();
        insert_filter_context(&mut ctx, &["status".into()], &HashMap::new());
        let mut tera = Tera::default();
        tera.add_raw_template("t", "[{{ search }}][{{ filters | length }}]")
            .unwrap();
        let rendered = tera.render("t", &ctx).unwrap();
        assert_eq!(rendered, "[][0]");
    }

    /// Models without a PK fall through to empty `ORDER BY` —
    /// pagination on PK-less models is unusual, and there's no
    /// canonical column to pick.
    #[test]
    fn default_order_by_empty_when_no_pk() {
        // Build a schema with no primary key.
        let no_pk: &'static ModelSchema = Box::leak(Box::new(ModelSchema {
            name: "Audit",
            table: "audits",
            fields: Box::leak(Box::new([crate::core::FieldSchema {
                name: "msg",
                column: "msg",
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
            }])),
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
        }));
        assert!(default_order_by(no_pk).is_empty());
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

    /// CreateView default template + success_url.
    #[test]
    fn create_view_defaults() {
        let cv = CreateView::for_model(schema_two_fields());
        assert_eq!(cv.template, "posts_form.html");
        assert_eq!(cv.success_url, "/");
    }

    /// UpdateView shares the same default template name (forms are
    /// interchangeable; templates branch on `is_create`/`is_update`).
    #[test]
    fn update_view_default_template_matches_create() {
        let uv = UpdateView::for_model(schema_two_fields());
        assert_eq!(uv.template, "posts_form.html");
    }

    /// `form_fields` skips the primary key (Auto<i64> id).
    #[test]
    fn form_fields_skips_pk_and_auto() {
        let s = schema_two_fields();
        let values = HashMap::new();
        let ff = form_fields(s, None, &values);
        assert_eq!(ff.len(), 1);
        assert_eq!(ff[0].name, "title");
    }

    /// `form_fields` populates `value` from the supplied row.
    #[test]
    fn form_fields_populates_value_from_row() {
        let s = schema_two_fields();
        let mut values = HashMap::new();
        values.insert("title".to_owned(), "Hello".to_owned());
        let ff = form_fields(s, None, &values);
        assert_eq!(ff[0].value, "Hello");
    }

    /// `coerce_value` rejects garbage integers with a clear error.
    #[test]
    fn coerce_value_int_error_surfaces() {
        let s = schema_two_fields();
        // The `id` field is i64.
        let id = &s.fields[0];
        let err = coerce_value(id, "not-a-number").unwrap_err();
        assert!(err.contains("integer"), "got: {err}");
    }

    /// Empty raw value on a NOT NULL non-bool field is reported as
    /// required-missing by `parse_form` (not by `coerce_value`).
    /// `coerce_value` itself returns Ok(SqlValue::String("")) for
    /// strings with the empty value — which is fine; required-ness
    /// is checked in the parse layer.
    #[test]
    fn coerce_value_empty_string_passes_through() {
        let s = schema_two_fields();
        let title = &s.fields[1];
        let v = coerce_value(title, "").unwrap();
        assert!(matches!(v, SqlValue::String(ref s) if s.is_empty()));
    }

    /// `parse_form` collects required-missing errors for non-nullable
    /// fields without raw values.
    #[test]
    fn parse_form_flags_required_missing() {
        let s = schema_two_fields();
        let submitted = HashMap::new();
        let (cols, vals, errors) = parse_form(s, None, &submitted);
        assert!(cols.is_empty());
        assert!(vals.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors.contains_key("title"));
    }

    /// `parse_form` accepts a valid submission and produces the
    /// columns/values pair the InsertQuery / UpdateQuery want.
    #[test]
    fn parse_form_accepts_valid_submission() {
        let s = schema_two_fields();
        let mut submitted = HashMap::new();
        submitted.insert("title".to_owned(), "Hello".to_owned());
        let (cols, vals, errors) = parse_form(s, None, &submitted);
        assert!(errors.is_empty());
        assert_eq!(cols, vec!["title"]);
        assert_eq!(vals.len(), 1);
        assert!(matches!(&vals[0], SqlValue::String(ref s) if s == "Hello"));
    }

    /// `field_type_label` covers every FieldType variant — keeps
    /// the match exhaustive when new types land.
    #[test]
    fn field_type_label_covers_known_variants() {
        use crate::core::FieldType as T;
        assert_eq!(field_type_label(T::String), "string");
        assert_eq!(field_type_label(T::I16), "i16");
        assert_eq!(field_type_label(T::I32), "i32");
        assert_eq!(field_type_label(T::I64), "i64");
        assert_eq!(field_type_label(T::F32), "f32");
        assert_eq!(field_type_label(T::F64), "f64");
        assert_eq!(field_type_label(T::Bool), "bool");
        assert_eq!(field_type_label(T::DateTime), "datetime");
        assert_eq!(field_type_label(T::Date), "date");
        assert_eq!(field_type_label(T::Uuid), "uuid");
        assert_eq!(field_type_label(T::Json), "json");
    }

    /// `parse_form` enforces `max_length` declared on the schema —
    /// the user gets a form-side error rather than a 500-on-insert
    /// when the SQL layer rejects the over-long value.
    #[test]
    fn parse_form_enforces_max_length() {
        let s = schema_with_bounds();
        let mut submitted = HashMap::new();
        submitted.insert("title".to_owned(), "way too long".to_owned()); // 12 > 5
        submitted.insert("score".to_owned(), "50".to_owned());
        let (cols, vals, errors) = parse_form(s, None, &submitted);
        assert!(cols.is_empty() || !cols.contains(&"title"));
        assert!(
            vals.is_empty() || vals.len() == 1,
            "title rejected, score still in"
        );
        let title_err = errors.get("title").expect("title error present");
        assert!(
            title_err.contains("5") && title_err.contains("12"),
            "expected length detail, got: {title_err}"
        );
        // The other field validates fine.
        assert!(!errors.contains_key("score"));
    }

    /// `parse_form` enforces `min`/`max` declared on integer fields.
    #[test]
    fn parse_form_enforces_int_range() {
        let s = schema_with_bounds();
        let mut submitted = HashMap::new();
        submitted.insert("title".to_owned(), "ok".to_owned());
        submitted.insert("score".to_owned(), "150".to_owned()); // > 100
        let (_, _, errors) = parse_form(s, None, &submitted);
        let score_err = errors.get("score").expect("score error present");
        assert!(
            score_err.contains("100") && score_err.contains("150"),
            "expected range detail, got: {score_err}"
        );
    }

    /// `bounds_error_message` produces user-friendly text without
    /// the framework's `model.field` framing — the field name is
    /// already the error key, so the message just needs the rule.
    #[test]
    fn bounds_error_message_strips_framing() {
        use crate::core::QueryError;
        let max = QueryError::MaxLengthExceeded {
            model: "Post",
            field: "title".into(),
            max: 5,
            actual: 12,
        };
        let msg = bounds_error_message(&max);
        assert!(msg.contains("5") && msg.contains("12"), "got: {msg}");
        assert!(!msg.contains("Post"), "should drop model framing: {msg}");
        assert!(!msg.contains("title"), "should drop field framing: {msg}");

        let range = QueryError::OutOfRange {
            model: "Post",
            field: "score".into(),
            value: 150,
            min: Some(0),
            max: Some(100),
        };
        let msg = bounds_error_message(&range);
        assert!(
            msg.contains("0") && msg.contains("100") && msg.contains("150"),
            "got: {msg}"
        );
    }

    /// One-sided bounds (only `min` set, or only `max`) get a
    /// readable "must be ≥ N" / "must be ≤ N" message.
    #[test]
    fn bounds_error_message_one_sided_range() {
        use crate::core::QueryError;
        let only_min = QueryError::OutOfRange {
            model: "X",
            field: "n".into(),
            value: -5,
            min: Some(0),
            max: None,
        };
        assert!(bounds_error_message(&only_min).contains("≥ 0"));

        let only_max = QueryError::OutOfRange {
            model: "X",
            field: "n".into(),
            value: 200,
            min: None,
            max: Some(100),
        };
        assert!(bounds_error_message(&only_max).contains("≤ 100"));
    }

    /// Smoke: every CBV's `tenant_router` builds without panicking
    /// on axum routing constraints. End-to-end live coverage lives
    /// in the cookbook integration tests; these guard the trait
    /// bound + state-cloning shape.
    #[cfg(feature = "tenancy")]
    #[test]
    fn tenant_routers_build_for_basic_model() {
        let s = schema_two_fields();
        let tera = Arc::new(Tera::default());
        let _ = ListView::for_model(s)
            .page_size(10)
            .tenant_router("/posts", tera.clone());
        let _ = DetailView::for_model(s).tenant_router("/posts", tera.clone());
        let _ = DeleteView::for_model(s)
            .success_url("/posts")
            .tenant_router("/posts", tera.clone());
        let _ = CreateView::for_model(s)
            .success_url("/posts")
            .tenant_router("/posts", tera.clone());
        let _ = UpdateView::for_model(s)
            .success_url("/posts")
            .tenant_router("/posts", tera);
    }
}
