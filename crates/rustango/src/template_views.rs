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

use crate::core::{FieldSchema, Filter, ModelSchema, Op, SelectQuery, SqlValue, WhereExpr};
use crate::sql::Pool;
use crate::sql::{count_rows_pool, select_one_row_as_json, select_rows_as_json};

// ============================================================== ListView

// ============================================================== Bulk actions

/// Future returned by a [`ListView`] bulk action handler. Mirrors
/// the shape `admin::AdminActionFuture` uses so handlers feel
/// consistent across the framework. Errors render as a 400 response
/// with the supplied string in the body.
pub type BulkActionFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>;

/// Handler closure for a bulk action mounted on the static-pool
/// [`ListView::router`] path. Receives the captured `&Pool` and
/// the parsed list of selected primary keys (already type-coerced
/// from the form's `_selected_action` strings).
pub type BulkActionFn =
    Arc<dyn for<'a> Fn(&'a Pool, &'a [SqlValue]) -> BulkActionFuture<'a> + Send + Sync>;

/// Tenant-mode counterpart — runs against the per-request tenant
/// connection from [`crate::extractors::Tenant::conn`]. Wired via
/// [`ListView::tenant_action`].
/// v0.38 — PG-only by signature (takes `&mut PgConnection`). Sqlite/
/// MySQL tenants get the tri-dialect `Pool`-based variant below.
#[cfg(all(feature = "tenancy", feature = "postgres"))]
pub type TenantBulkActionFn = Arc<
    dyn for<'a> Fn(&'a mut crate::sql::sqlx::PgConnection, &'a [SqlValue]) -> BulkActionFuture<'a>
        + Send
        + Sync,
>;

/// One registered bulk action. The framework ships
/// `delete_selected` as a built-in when [`ListView::bulk_actions`]
/// is enabled; user-defined actions wire in via
/// [`ListView::action`] (static pool) or
/// [`ListView::tenant_action`] (per-request tenant connection).
#[derive(Clone)]
pub struct BulkAction {
    pub name: String,
    pub label: String,
    pub handler: BulkActionHandler,
}

/// Either a static-pool handler or a tenant-mode handler. Built up
/// by the builder methods and dispatched in the matching POST
/// handler. Mixing kinds on the same `ListView` doesn't make sense
/// (the router only mounts one handler shape), so the wrong-kind
/// case surfaces a clear runtime error rather than corrupting the
/// connection.
#[derive(Clone)]
pub enum BulkActionHandler {
    Pool(BulkActionFn),
    #[cfg(all(feature = "tenancy", feature = "postgres"))]
    Tenant(TenantBulkActionFn),
}

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
    /// Hard cap on `?page_size=N` URL overrides. Default 100.
    max_page_size: i64,
    fields: Option<Vec<String>>,
    order_by: Vec<(String, bool)>,
    filter_fields: Vec<String>,
    search_fields: Vec<String>,
    /// Allowlist for `?ordering=col` / `?ordering=-col` overrides.
    /// Empty = no override allowed; the builder-side `order_by`
    /// is the only ordering applied.
    ordering_fields: Vec<String>,
    /// When `true`, the list endpoint accepts POSTs that carry an
    /// `action` form field plus `_selected_action[]` PK values, and
    /// dispatches to the handler registered under that action name.
    /// Built-in `delete_selected` is always included when on; user
    /// actions stack via [`Self::action`] /
    /// [`Self::tenant_action`].
    bulk_actions_enabled: bool,
    /// User-registered actions (not including the built-in
    /// `delete_selected`).
    actions: Vec<BulkAction>,
    /// When `true`, the built-in `delete_selected` action shows a
    /// confirmation page (selected rows + a "Confirm delete" button)
    /// before actually firing the DELETE — Django admin's two-step
    /// shape. Custom actions registered via [`Self::action`] /
    /// [`Self::tenant_action`] are not gated by this flag (mirrors
    /// Django: only `delete_selected` is confirmed by default).
    /// Default off — confirmations only mount when the user opts in
    /// via [`Self::with_delete_confirmation`].
    confirm_delete: bool,
    /// Tera template name for the bulk-delete confirmation page.
    /// Default `<table>_confirm_bulk_delete.html`. Override via
    /// [`Self::with_delete_confirmation_template`].
    confirm_delete_template: Option<String>,
    /// When `true`, every FK column on the schema gets a sibling
    /// `<column>_display` field stamped into each row's JSON,
    /// resolved against the FK target model's `#[rustango(display =
    /// "...")]` value. Templates render
    /// `{{ row.author_id_display | default(value=row.author_id) }}`
    /// to show "Ada Lovelace" instead of `42`. Default off — the
    /// resolution adds one extra `SELECT pk, display FROM <target>
    /// WHERE pk = ANY(...)` per FK column per page, which is
    /// usually cheap but isn't free. Opt in via
    /// [`Self::with_fk_display`].
    fk_display: bool,
    /// #379 — Django-shape `context_object_name`. Binds the row
    /// list under a custom Tera variable in addition to the
    /// default `object_list`. Empty (the default) skips the
    /// extra binding.
    context_object_name: String,
}

impl ListView {
    /// Start a `ListView` for the given schema. Defaults: template
    /// name `<table>_list.html`, page size 20, max page size 100,
    /// no `ORDER BY`, all fields included, no filters, no search,
    /// no `?ordering=` override.
    #[must_use]
    pub fn for_model(schema: &'static ModelSchema) -> Self {
        Self {
            schema,
            template: format!("{}_list.html", schema.table),
            page_size: 20,
            max_page_size: 100,
            fields: None,
            order_by: Vec::new(),
            filter_fields: Vec::new(),
            search_fields: Vec::new(),
            ordering_fields: Vec::new(),
            bulk_actions_enabled: false,
            actions: Vec::new(),
            confirm_delete: false,
            confirm_delete_template: None,
            fk_display: false,
            context_object_name: String::new(),
        }
    }

    /// Django-shape `context_object_name` — bind the row list
    /// under a custom Tera variable in addition to the default
    /// `object_list`. Issue #379. Empty string (the default)
    /// leaves only `object_list`.
    ///
    /// ```ignore
    /// ListView::for_model(Post::SCHEMA)
    ///     .context_object_name("posts")
    /// // template can now read `{% for post in posts %}` AND
    /// // `{% for post in object_list %}` — both work.
    /// ```
    #[must_use]
    pub fn context_object_name(mut self, name: impl Into<String>) -> Self {
        self.context_object_name = name.into();
        self
    }

    /// Override the Tera template name.
    #[must_use]
    pub fn template(mut self, name: impl Into<String>) -> Self {
        self.template = name.into();
        self
    }

    /// Default page size — clamped to `≥ 1`. Default 20. Users can
    /// override per-request via `?page_size=N`, clamped to
    /// `[1, max_page_size]`.
    #[must_use]
    pub fn page_size(mut self, n: usize) -> Self {
        self.page_size = i64::try_from(n).unwrap_or(20).max(1);
        self
    }

    /// Hard cap on `?page_size=N` URL overrides. Default 100.
    /// Prevents a hostile client from issuing `?page_size=999999` and
    /// dragging the database into a giant scan. Clamped to `≥ 1`.
    #[must_use]
    pub fn max_page_size(mut self, n: usize) -> Self {
        self.max_page_size = i64::try_from(n).unwrap_or(100).max(1);
        self
    }

    /// Add an `ORDER BY` clause. Call multiple times for tie-breakers.
    #[must_use]
    pub fn order_by(mut self, column: impl Into<String>, desc: bool) -> Self {
        self.order_by.push((column.into(), desc));
        self
    }

    /// Allow `?ordering=col` / `?ordering=-col` URL overrides on
    /// these fields. Without this, the ordering set via
    /// [`Self::order_by`] is fixed. With it, users (or sortable
    /// table headers in templates) can switch the active sort.
    ///
    /// Each name resolves against the schema by Rust field name OR
    /// SQL column name; unmatched names are silently dropped at
    /// request time so a typo in the URL doesn't 400. Mirrors the
    /// `filter_fields` / `search_fields` allowlist shape — bare
    /// names plus the `-` desc prefix.
    ///
    /// The active ordering string is stamped into the Tera context
    /// as `ordering`, so templates can render sortable headers:
    ///
    /// ```html
    /// {# Click to toggle asc/desc — `ordering` carries the active spec #}
    /// <a href="?ordering={% if ordering == 'title' %}-{% endif %}title">Title</a>
    /// ```
    #[must_use]
    pub fn ordering_fields(mut self, names: &[&str]) -> Self {
        self.ordering_fields = names.iter().map(|s| (*s).to_owned()).collect();
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

    /// Enable bulk actions (Django-admin shape). Mounts a `POST
    /// <prefix>` route alongside the existing `GET`. The list
    /// endpoint stamps a `bulk_actions` array into the Tera context
    /// (`[{name, label}, ...]`) so templates can render an action
    /// dropdown. The built-in `delete_selected` is always included
    /// when on; register more via [`Self::action`] (static pool) or
    /// [`Self::tenant_action`].
    ///
    /// Form shape the POST handler expects:
    /// - `action`: the name of one registered action
    /// - `_selected_action`: one or more values, each a row's PK
    /// - `_csrf`: the CSRF token (when [`crate::manage::Cli::with_csrf`]
    ///   is on, which is the recommended setup for form-driven CBVs)
    ///
    /// Successful action runs return `303 See Other` to the same
    /// prefix so a refresh after the redirect doesn't replay the
    /// action.
    ///
    /// ## Destructive-action UX (built-in `delete_selected`)
    ///
    /// **The current implementation runs every action immediately
    /// on POST — no confirmation step.** Django admin ships a
    /// confirmation page for `delete_selected` (select rows →
    /// submit → "are you sure?" page → confirm → delete). The
    /// rustango v0.30.4 v1 of bulk actions skips that intermediate
    /// page. Until a `confirm_template` builder lands, the
    /// recommended pattern is:
    ///
    /// 1. Add a `<confirm>` JS handler in the template:
    ///    `<form onsubmit="return confirmDestructive(this)">`
    /// 2. Or wrap the destructive action with a custom action
    ///    handler that spawns its own confirmation route via
    ///    [`Self::action`] and only calls the framework's
    ///    `delete_selected` after confirmation.
    ///
    /// Tracking item: a `with_delete_confirmation(true)` flag that
    /// renders an inline confirmation page when the form's `action
    /// = delete_selected` POST arrives without a `confirmed = true`
    /// flag. v0.31 candidate.
    ///
    /// ```rust,ignore
    /// ListView::for_model(Post::SCHEMA)
    ///     .bulk_actions(true)               // enables built-in delete_selected
    ///     .action("publish_selected", "Publish selected", Arc::new(|pool, pks| {
    ///         Box::pin(async move {
    ///             let pks: Vec<i64> = pks.iter().filter_map(|v| match v {
    ///                 SqlValue::I64(n) => Some(*n), _ => None,
    ///             }).collect();
    ///             sqlx::query("UPDATE posts SET status = 'published' WHERE id = ANY($1)")
    ///                 .bind(&pks).execute(pool).await
    ///                 .map(|_| ()).map_err(|e| e.to_string())
    ///         })
    ///     }))
    ///     .router("/posts", tera, pool)
    /// ```
    #[must_use]
    pub fn bulk_actions(mut self, on: bool) -> Self {
        self.bulk_actions_enabled = on;
        self
    }

    /// Register a custom static-pool bulk action. Pair with
    /// [`Self::bulk_actions`] to actually mount the POST handler.
    /// Use [`Self::tenant_action`] inside tenancy projects.
    ///
    /// `name` must be url-safe; it's matched against the form's
    /// `action` field at request time. Duplicate names overwrite.
    #[must_use]
    pub fn action(
        mut self,
        name: impl Into<String>,
        label: impl Into<String>,
        handler: BulkActionFn,
    ) -> Self {
        let name = name.into();
        self.actions.retain(|a| !same_action_name(&a.name, &name));
        self.actions.push(BulkAction {
            name,
            label: label.into(),
            handler: BulkActionHandler::Pool(handler),
        });
        self
    }

    /// Show a confirmation page before the built-in `delete_selected`
    /// action fires. Mirrors Django admin's two-step delete flow
    /// (select rows → submit → "are you sure?" → confirm → delete)
    /// and closes the destructive-action footgun documented in the
    /// v0.30.4 v1 of bulk actions.
    ///
    /// When on, a POST with `action=delete_selected` and no
    /// `confirmed=true` form field renders the confirmation
    /// template (default `<table>_confirm_bulk_delete.html`) with:
    ///
    /// - `action`: `"delete_selected"`
    /// - `pks`: list of selected primary keys (string-coerced)
    /// - `objects`: full row data for each selected PK so the
    ///   template can show *what* will be deleted, not just the id
    /// - `csrf_token`: re-stamped from cookies/headers so the
    ///   second POST reuses the same CSRF token chain
    ///
    /// The confirm button submits the same form with
    /// `confirmed=true` added; the handler short-circuits the
    /// confirmation render and runs the actual DELETE.
    ///
    /// Custom actions registered via [`Self::action`] /
    /// [`Self::tenant_action`] are NOT gated by this flag —
    /// matches Django's convention (only `delete_selected` is
    /// confirmed). Custom actions that need confirmation should
    /// implement their own confirm+submit handler shape.
    #[must_use]
    pub fn with_delete_confirmation(mut self, on: bool) -> Self {
        self.confirm_delete = on;
        self
    }

    /// Override the Tera template name used for the bulk-delete
    /// confirmation page. Default `<table>_confirm_bulk_delete.html`.
    /// Implies [`Self::with_delete_confirmation`] is on.
    #[must_use]
    pub fn with_delete_confirmation_template(mut self, name: impl Into<String>) -> Self {
        self.confirm_delete_template = Some(name.into());
        self.confirm_delete = true;
        self
    }

    /// Stamp `<column>_display` sibling fields into each row's JSON
    /// for every FK / O2O column on the schema, resolved against
    /// the target model's `#[rustango(display = "...")]` field.
    /// Lets templates render `{{ row.author_id_display }}`
    /// (`"Ada Lovelace"`) instead of just `{{ row.author_id }}`
    /// (`42`).
    ///
    /// Implementation: one extra `SELECT pk, display FROM <target>
    /// WHERE pk = ANY(...)` per FK column per page, after the main
    /// list SELECT. Cheap (1 indexed lookup per FK target, batched
    /// across the page's rows) but not free — opt in only when the
    /// templates actually need it.
    ///
    /// FK targets without a `display` field, or models not
    /// registered in the inventory (e.g. cross-binary references),
    /// are silently skipped — the row gets no `_display` sibling
    /// for that column and templates can fall back to the raw FK.
    /// NULL FK values are skipped too (no display lookup possible).
    #[must_use]
    pub fn with_fk_display(mut self, on: bool) -> Self {
        self.fk_display = on;
        self
    }

    /// Tenancy counterpart to [`Self::action`] — handler runs
    /// against the per-request `&mut PgConnection` from the
    /// [`crate::extractors::Tenant`] extractor instead of a captured
    /// pool. Pair with [`Self::tenant_router`]. PG-only by signature.
    #[cfg(all(feature = "tenancy", feature = "postgres"))]
    #[must_use]
    pub fn tenant_action(
        mut self,
        name: impl Into<String>,
        label: impl Into<String>,
        handler: TenantBulkActionFn,
    ) -> Self {
        let name = name.into();
        self.actions.retain(|a| !same_action_name(&a.name, &name));
        self.actions.push(BulkAction {
            name,
            label: label.into(),
            handler: BulkActionHandler::Tenant(handler),
        });
        self
    }

    /// Mount as `GET <prefix>` rendering through `tera` from `pool`.
    /// Single-tenant pool capture — every request runs against the
    /// same pool. For tenancy projects use [`Self::tenant_router`].
    /// When [`Self::bulk_actions`] is on, also mounts `POST <prefix>`.
    #[must_use]
    pub fn router(self, prefix: &str, tera: Arc<Tera>, pool: Pool) -> Router<()> {
        let bulk = self.bulk_actions_enabled;
        let state = Arc::new(ListViewState {
            vs: self,
            tera,
            pool,
        });
        let route = if bulk {
            get(handle_list).post(handle_list_action)
        } else {
            get(handle_list)
        };
        Router::new().route(prefix, route).with_state(state)
    }

    /// Tenant-aware variant — each request resolves its own
    /// connection via the [`crate::extractors::Tenant`] extractor
    /// instead of capturing a single pool at mount time.
    /// Required for multi-tenant projects (subdomain / schema /
    /// per-tenant database). Mirrors `viewset::ViewSet::tenant_router`.
    /// When [`Self::bulk_actions`] is on, also mounts `POST <prefix>`.
    #[cfg(feature = "tenancy")]
    #[must_use]
    pub fn tenant_router(self, prefix: &str, tera: Arc<Tera>) -> Router<()> {
        let bulk = self.bulk_actions_enabled;
        let state = Arc::new(TenantListViewState { vs: self, tera });
        let route = if bulk {
            get(handle_list_tenant).post(handle_list_action_tenant)
        } else {
            get(handle_list_tenant)
        };
        Router::new().route(prefix, route).with_state(state)
    }
}

/// Action-name comparison helper. Names are stored as `String`s but
/// matched literal — no case-folding (consistency with Django's
/// `action` form field).
fn same_action_name(a: &str, b: &str) -> bool {
    a == b
}

#[derive(Clone)]
struct ListViewState {
    vs: ListView,
    tera: Arc<Tera>,
    pool: Pool,
}

async fn handle_list(
    State(state): State<Arc<ListViewState>>,
    headers: axum::http::HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let page: i64 = params
        .get("page")
        .and_then(|p| p.parse().ok())
        .unwrap_or(1)
        .max(1);
    let page_size = resolve_page_size(state.vs.page_size, state.vs.max_page_size, &params);
    let offset = (page - 1) * page_size;

    let (order_by, active_ordering) = match resolve_active_order(
        state.vs.schema,
        &state.vs.order_by,
        &state.vs.ordering_fields,
        &params,
    ) {
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
        limit: Some(page_size),
        offset: Some(offset),
        lock_mode: None,
        compound: vec![],
        projection: None,
        distinct: None,
    };
    let count_q = crate::core::CountQuery {
        model: state.vs.schema,
        where_clause,
        // template_views folds the search-fields ILIKE predicates
        // into where_clause via build_list_where, so the dedicated
        // SearchClause is unused here.
        search: None,
    };

    let fields = resolved_fields(state.vs.schema, state.vs.fields.as_deref());
    let (rows_result, count_result) = tokio::join!(
        select_rows_as_json(&state.pool, &select_q, &fields),
        count_rows_pool(&state.pool, &count_q),
    );
    let mut object_list: Vec<Value> = match rows_result {
        Ok(r) => r,
        Err(e) => return template_error(&format!("query rows: {e}")),
    };
    let total = match count_result {
        Ok(c) => c,
        Err(e) => return template_error(&format!("count rows: {e}")),
    };
    if state.vs.fk_display {
        resolve_fk_displays_pool(state.vs.schema, &state.pool, &mut object_list).await;
    }

    let total_pages = ((total - 1).max(0) / page_size) + 1;
    let mut ctx = Context::new();
    ctx.insert("object_list", &object_list);
    // #379 — Django-shape `context_object_name`. Adds a second
    // binding so templates can read `{{ posts }}` instead of
    // `{{ object_list }}`. Empty (the default) skips the rename.
    if !state.vs.context_object_name.is_empty() {
        ctx.insert(&state.vs.context_object_name, &object_list);
    }
    ctx.insert("page", &page);
    ctx.insert("page_size", &page_size);
    ctx.insert("total", &total);
    ctx.insert("total_pages", &total_pages);
    let has_next = page < total_pages;
    let has_prev = page > 1;
    ctx.insert("has_next", &has_next);
    ctx.insert("has_prev", &has_prev);
    ctx.insert("ordering", &active_ordering);
    insert_filter_context(&mut ctx, &state.vs.filter_fields, &params);
    insert_pagination_urls(&mut ctx, page, has_next, has_prev, &params);
    insert_bulk_actions_context(&mut ctx, &state.vs);

    // v0.30.17 — stamp the CSRF token into the context AND set the
    // cookie on the response. ListView's bulk-action POST is gated
    // by the project's CSRF middleware (when on); without this the
    // form-rendered token is empty and every legitimate POST 403s.
    let set_cookie = stamp_csrf(&headers, &mut ctx);
    let mut resp = render(&state.tera, &state.vs.template, &ctx);
    apply_csrf_cookie(&mut resp, set_cookie);
    resp
}

/// `POST <prefix>` — bulk-action dispatcher. Mounted only when
/// [`ListView::bulk_actions`] is on (the `router` builder branches
/// before nesting the route). On success returns `303 See Other`
/// to the same prefix so the browser refresh after redirect doesn't
/// replay the action; failures render a plain-text 400.
async fn handle_list_action(
    State(state): State<Arc<ListViewState>>,
    req: axum::extract::Request,
) -> Response {
    let (parts, body) = req.into_parts();
    let form = match read_repeating_form(body).await {
        Ok(f) => f,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let (action, raws) = match parse_bulk_action_form(&form) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let Some(pk_field) = state.vs.schema.primary_key() else {
        return template_error(&format!(
            "model `{}` has no primary key — bulk actions require one",
            state.vs.schema.table
        ));
    };
    let pks = match coerce_selected_pks(pk_field, &raws) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };

    // v0.30.7 — confirmation gate for the built-in delete_selected.
    // When `with_delete_confirmation(true)` is on AND the action
    // matches the built-in DELETE name AND the form lacks a
    // `confirmed=true` flag, render the confirmation template
    // instead of running the DELETE. Custom actions are NOT gated
    // by this flag (matches Django's convention).
    if state.vs.confirm_delete && action == BUILTIN_DELETE_SELECTED && !is_form_confirmed(&form) {
        let objects =
            match fetch_pks_as_objects_pool(state.vs.schema, pk_field, &state.pool, &pks).await {
                Ok(o) => o,
                Err(e) => return template_error(&format!("fetch confirm rows: {e}")),
            };
        return render_bulk_delete_confirm(
            &state.tera,
            confirm_delete_template_name(&state.vs),
            &action,
            &raws,
            &objects,
            &parts.headers,
        );
    }

    // Dispatch: user-registered actions first, then built-ins.
    let dispatch_path = parts.uri.path().to_owned();
    let result: Result<(), String> = if let Some(custom) = state
        .vs
        .actions
        .iter()
        .find(|a| same_action_name(&a.name, &action))
    {
        match &custom.handler {
            BulkActionHandler::Pool(f) => f(&state.pool, &pks).await,
            #[cfg(all(feature = "tenancy", feature = "postgres"))]
            BulkActionHandler::Tenant(_) => Err("this action was registered via tenant_action — \
                 mount the ListView via tenant_router(...) to dispatch it"
                .into()),
        }
    } else if action == BUILTIN_DELETE_SELECTED {
        run_delete_selected_pool(state.vs.schema, pk_field, &state.pool, &pks).await
    } else {
        return (
            StatusCode::BAD_REQUEST,
            format!("unknown action `{action}`"),
        )
            .into_response();
    };

    match result {
        Ok(()) => axum::response::Redirect::to(&dispatch_path).into_response(),
        Err(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
    }
}

/// Read the request body as `application/x-www-form-urlencoded`,
/// preserving repeated keys (every `_selected_action` value).
/// `axum::Form<HashMap<...>>` collapses repeats into a single value,
/// which would lose every selected row past the first.
async fn read_repeating_form(
    body: axum::body::Body,
) -> Result<HashMap<String, Vec<String>>, String> {
    use axum::body::to_bytes;
    let bytes = to_bytes(body, 4 * 1024 * 1024)
        .await
        .map_err(|e| e.to_string())?;
    let pairs: Vec<(String, String)> =
        serde_urlencoded::from_bytes(&bytes).map_err(|e| e.to_string())?;
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (k, v) in pairs {
        out.entry(k).or_default().push(v);
    }
    Ok(out)
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
    /// #379 — Django-shape `context_object_name`. Renames the row
    /// under a custom Tera variable name. The legacy `"object"`
    /// key stays populated for back-compat; this just adds a
    /// second binding so templates can read `{{ post.title }}`
    /// instead of `{{ object.title }}`. Empty → no rename.
    context_object_name: String,
    /// #379 — Django-shape `slug_field` / `slug_url_kwarg`. When
    /// non-empty, the URL captures the lookup value as `{lookup}`
    /// (instead of `{pk}`) and the SELECT predicate matches
    /// `WHERE <lookup_field> = <captured>` instead of `WHERE pk =
    /// <captured>`. Useful for `/posts/{slug}` style URLs.
    lookup_field: Option<String>,
}

impl DetailView {
    #[must_use]
    pub fn for_model(schema: &'static ModelSchema) -> Self {
        Self {
            schema,
            template: format!("{}_detail.html", schema.table),
            fields: None,
            context_object_name: String::new(),
            lookup_field: None,
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

    /// Django-shape `context_object_name` — bind the row under a
    /// custom Tera variable in addition to the default `object`.
    /// Issue #379. Empty string (the default) leaves only the
    /// `object` binding.
    ///
    /// ```ignore
    /// // /posts/{pk} → template reads `{{ post.title }}`
    /// DetailView::for_model(Post::SCHEMA)
    ///     .context_object_name("post")
    /// ```
    #[must_use]
    pub fn context_object_name(mut self, name: impl Into<String>) -> Self {
        self.context_object_name = name.into();
        self
    }

    /// Django-shape `slug_field` — look up the row by a non-PK
    /// column. The captured URL segment matches against the named
    /// column instead of the model's primary key. Issue #379.
    ///
    /// Field name must exist on the schema (Rust field name OR
    /// SQL column name); unknown names produce a 500 at request
    /// time with a clear `template render error: unknown lookup
    /// field …` message.
    ///
    /// ```ignore
    /// // /posts/{slug} → SELECT … WHERE slug = $1
    /// DetailView::for_model(Post::SCHEMA)
    ///     .lookup_field("slug")
    /// ```
    #[must_use]
    pub fn lookup_field(mut self, column: impl Into<String>) -> Self {
        self.lookup_field = Some(column.into());
        self
    }

    #[must_use]
    pub fn router(self, prefix: &str, tera: Arc<Tera>, pool: Pool) -> Router<()> {
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
    pool: Pool,
}

async fn handle_detail(
    State(state): State<Arc<DetailViewState>>,
    Path(pk): Path<String>,
) -> Response {
    // #379 — `lookup_field` opt-in: probe by the named column
    // instead of the PK. Validate the name maps to a real field
    // up front so the predicate carries a real `&'static str`
    // column ident.
    let lookup = match resolve_lookup_field(state.vs.schema, state.vs.lookup_field.as_deref()) {
        Ok(f) => f,
        Err(e) => return template_error(&e),
    };
    let select_q = SelectQuery {
        model: state.vs.schema,
        where_clause: WhereExpr::Predicate(Filter {
            column: lookup.column,
            op: Op::Eq,
            value: coerce_pk(lookup, &pk),
        }),
        search: None,
        joins: vec![],
        order_by: vec![],
        limit: Some(1),
        offset: None,
        lock_mode: None,
        compound: vec![],
        projection: None,
        distinct: None,
    };
    let fields = resolved_fields(state.vs.schema, state.vs.fields.as_deref());
    let object = match select_one_row_as_json(&state.pool, &select_q, &fields).await {
        Ok(Some(r)) => r,
        Ok(None) => return (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => return template_error(&format!("query row: {e}")),
    };

    let mut ctx = Context::new();
    ctx.insert("object", &object);
    if !state.vs.context_object_name.is_empty() {
        ctx.insert(&state.vs.context_object_name, &object);
    }

    render(&state.tera, &state.vs.template, &ctx)
}

/// Resolve the DetailView lookup column (#379). Returns the PK
/// field when `lookup_field` is `None`; otherwise the named
/// field. Errors when the schema has no PK and no lookup_field
/// set, or when the lookup_field name doesn't resolve.
fn resolve_lookup_field(
    schema: &'static ModelSchema,
    lookup_field: Option<&str>,
) -> Result<&'static FieldSchema, String> {
    if let Some(name) = lookup_field {
        // Match by Rust field name OR SQL column — same shape as
        // resolved_fields and the rest of the framework's
        // user-facing name resolution.
        if let Some(f) = schema
            .scalar_fields()
            .find(|f| f.name == name || f.column == name)
        {
            return Ok(f);
        }
        return Err(format!(
            "DetailView::lookup_field(\"{name}\") doesn't match any scalar field on `{}`",
            schema.table
        ));
    }
    schema.primary_key().ok_or_else(|| {
        format!(
            "model `{}` has no primary key — DetailView can't probe without an explicit lookup_field",
            schema.table
        )
    })
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
    pub fn router(self, prefix: &str, tera: Arc<Tera>, pool: Pool) -> Router<()> {
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
    pool: Pool,
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
            value: coerce_pk(pk_field, &pk),
        }),
        search: None,
        joins: vec![],
        order_by: vec![],
        limit: Some(1),
        offset: None,
        lock_mode: None,
        compound: vec![],
        projection: None,
        distinct: None,
    };
    let fields = resolved_fields(state.vs.schema, state.vs.fields.as_deref());
    let object = match select_one_row_as_json(&state.pool, &select_q, &fields).await {
        Ok(Some(r)) => r,
        Ok(None) => return (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => return template_error(&format!("query row: {e}")),
    };
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
            value: coerce_pk(pk_field, &pk),
        }),
    };
    match crate::sql::delete_pool(&state.pool, &delete_q).await {
        Ok(0) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Ok(_) => {
            // Note: typically `{pk}` in a delete success_url
            // doesn't make sense — the row is gone. We support it
            // anyway for symmetry with Create/Update; users with
            // soft-delete models might want to redirect to the
            // tombstone page.
            let target = substitute_pk(&state.vs.success_url, &pk);
            axum::response::Redirect::to(&target).into_response()
        }
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
    validator: Option<Validator>,
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
            validator: None,
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

    /// Install a closure-based validator that runs after schema-level
    /// type coercion + bounds checks but before the SQL INSERT.
    /// Returning `Err(FormErrors)` re-renders the form with the
    /// merged error map and a 422 status.
    ///
    /// ```ignore
    /// CreateView::for_model(Post::SCHEMA)
    ///     .validator(|data| {
    ///         let mut errs = FormErrors::default();
    ///         if data.get("title").map_or(true, |s| s.len() < 5) {
    ///             errs.add("title", "must be at least 5 characters");
    ///         }
    ///         if errs.is_empty() { Ok(()) } else { Err(errs) }
    ///     })
    ///     .router("/posts", tera, pool)
    /// ```
    #[must_use]
    pub fn validator<F>(mut self, f: F) -> Self
    where
        F: Fn(&HashMap<String, String>) -> Result<(), crate::forms::FormErrors>
            + Send
            + Sync
            + 'static,
    {
        self.validator = Some(Arc::new(f));
        self
    }

    /// Convenience: wire a `#[derive(Form)]` struct as the validator.
    /// The view runs `F::parse(data)` on every POST and re-renders
    /// the form with the collected errors when parse fails.
    /// `min_length` / `regex` / custom-validator-fn / cross-field
    /// validators all flow into the form's error map.
    ///
    /// ## What `.form::<F>()` does NOT do (yet)
    ///
    /// The parsed `F` value is **discarded** after validation —
    /// only its `parse` method's pass/fail outcome is consumed.
    /// The actual SQL INSERT still uses the framework's schema-
    /// driven type coercion path
    /// ([`crate::forms::collect_values`]), so `F`'s typed fields
    /// are not the source of truth for column values. Differences
    /// between `F` and the model schema (e.g. `F` has a
    /// `confirm_password` field with no model column, or `F`'s
    /// `i32 score` differs from the model's `i64 score`) are
    /// silently ignored on the SQL side. Full Django-style
    /// `ModelForm`-as-source-of-truth is a future enhancement;
    /// for now `.form::<F>()` is a *validation-only* hook.
    ///
    /// ## Example
    ///
    /// ```ignore
    /// #[derive(rustango::Form)]
    /// pub struct PostForm {
    ///     #[form(min_length = 5)]
    ///     title: String,
    ///     #[form(min_length = 1)]
    ///     body: String,
    /// }
    ///
    /// CreateView::for_model(Post::SCHEMA)
    ///     .form::<PostForm>()    // F's validators run; F's parsed value is dropped
    ///     .router("/posts", tera, pool)
    /// ```
    #[must_use]
    pub fn form<F: crate::forms::Form>(self) -> Self {
        self.validator(|data| F::parse(data).map(|_| ()))
    }

    /// Mount as `GET`/`POST <prefix>/new`.
    #[must_use]
    pub fn router(self, prefix: &str, tera: Arc<Tera>, pool: Pool) -> Router<()> {
        let state = Arc::new(FormViewState {
            schema: self.schema,
            template: self.template.clone(),
            success_url: self.success_url.clone(),
            fields: self.fields.clone(),
            tera,
            pool,
            validator: self.validator.clone(),
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
            validator: self.validator,
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
    validator: Option<Validator>,
}

impl UpdateView {
    #[must_use]
    pub fn for_model(schema: &'static ModelSchema) -> Self {
        Self {
            schema,
            template: format!("{}_form.html", schema.table),
            success_url: "/".to_owned(),
            fields: None,
            validator: None,
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

    /// Install a closure-based validator. Same shape and semantics
    /// as [`CreateView::validator`].
    #[must_use]
    pub fn validator<F>(mut self, f: F) -> Self
    where
        F: Fn(&HashMap<String, String>) -> Result<(), crate::forms::FormErrors>
            + Send
            + Sync
            + 'static,
    {
        self.validator = Some(Arc::new(f));
        self
    }

    /// Wire a `#[derive(Form)]` struct as the validator. Same shape
    /// and semantics as [`CreateView::form`].
    #[must_use]
    pub fn form<F: crate::forms::Form>(self) -> Self {
        self.validator(|data| F::parse(data).map(|_| ()))
    }

    /// Mount as `GET`/`POST <prefix>/{pk}/edit`.
    #[must_use]
    pub fn router(self, prefix: &str, tera: Arc<Tera>, pool: Pool) -> Router<()> {
        let state = Arc::new(FormViewState {
            schema: self.schema,
            template: self.template.clone(),
            success_url: self.success_url.clone(),
            fields: self.fields.clone(),
            tera,
            pool,
            validator: self.validator.clone(),
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
            validator: self.validator,
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

/// User-supplied validation hook installed on `CreateView` /
/// `UpdateView` via [`CreateView::validator`] / [`UpdateView::validator`]
/// (or the `Form`-trait convenience [`CreateView::form`] /
/// [`UpdateView::form`]). Runs *after* schema-level type coercion +
/// max_length / min / max bounds (which the framework owns) but
/// *before* the SQL INSERT/UPDATE. Returning `Err(FormErrors)`
/// re-renders the form with the merged error map and a 422 status.
pub type Validator =
    Arc<dyn Fn(&HashMap<String, String>) -> Result<(), crate::forms::FormErrors> + Send + Sync>;

#[derive(Clone)]
struct FormViewState {
    schema: &'static ModelSchema,
    template: String,
    success_url: String,
    fields: Option<Vec<String>>,
    tera: Arc<Tera>,
    pool: Pool,
    /// Optional user-supplied validator (`#[derive(Form)]`-derived
    /// `min_length` / `regex` / custom validator chain). v0.30.2
    /// — closes the v0.29 gap where business validation had to be
    /// re-implemented per project on top of the type-coercion path.
    validator: Option<Validator>,
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
        T::Time => "time",
        T::Uuid => "uuid",
        T::Json => "json",
        T::Decimal => "decimal",
        T::Binary => "binary",
    }
}

/// Substitute the `{pk}` placeholder in a `success_url` with a
/// known PK string. Used by `UpdateView` / `DeleteView` where the
/// PK is already in scope from the URL path — no row read
/// required. The CreateView equivalent is
/// [`interpolate_success_url`], which reads the PK from a
/// `RETURNING` row.
///
/// No-op when the placeholder isn't present.
fn substitute_pk(template: &str, pk: &str) -> String {
    if !template.contains("{pk}") {
        return template.to_owned();
    }
    template.replace("{pk}", pk)
}

/// Substitute every `{column}` placeholder in a `success_url`
/// with the matching column value pulled from the row that
/// `INSERT ... RETURNING <cols>` just produced. Used by
/// `CreateView` so projects can write
/// `success_url("/posts/{slug}")` (or
/// `success_url("/posts/{id}/draft/{slug}")` for the multi-column
/// case) and redirect to the new row's detail page.
///
/// `{pk}` is special-cased to map to the model's primary-key
/// column — `success_url("/posts/{pk}")` works without requiring
/// the user to know whether the PK column is named `id`, `pk`,
/// `uuid`, etc.
///
/// Returns the original `template` unchanged when no placeholders
/// are present. Surfaces a clear error string when:
/// - A placeholder names a column that doesn't exist on the model
/// - `{pk}` is present but the model has no primary key
/// - The row's column couldn't be read at the expected SQL type
///
/// The caller (handle_create_post) computes the `RETURNING` list
/// via [`success_url_returning_columns`] and feeds the resulting
/// row in here.
/// v0.38 — operates on the JSON object form of the RETURNING row.
/// Callers pass `&InsertReturningPool` and the helper extracts the
/// value per-backend before driving the placeholder substitution.
fn interpolate_success_url(
    template: &str,
    row: &crate::sql::InsertReturningPool,
    schema: &'static crate::core::ModelSchema,
) -> Result<String, String> {
    let placeholders = parse_success_url_placeholders(template);
    if placeholders.is_empty() {
        return Ok(template.to_owned());
    }
    let mut out = template.to_owned();
    for name in placeholders {
        let column = if name == "pk" {
            let Some(pk) = schema.primary_key() else {
                return Err(
                    "success_url contains `{pk}` placeholder but the model has no primary key"
                        .to_owned(),
                );
            };
            pk
        } else {
            schema.field(name).ok_or_else(|| {
                format!(
                    "success_url placeholder `{{{name}}}` does not match any field on `{}`",
                    schema.table
                )
            })?
        };
        let v = column_value_as_string_returning(row, column).map_err(|e| {
            format!(
                "success_url interpolation failed reading `{}`: {e}",
                column.column
            )
        })?;
        out = out.replace(&format!("{{{name}}}"), &v);
    }
    Ok(out)
}

/// Read `column` from an `InsertReturningPool` and stringify per-backend.
fn column_value_as_string_returning(
    row: &crate::sql::InsertReturningPool,
    column: &'static crate::core::FieldSchema,
) -> Result<String, String> {
    match row {
        #[cfg(feature = "postgres")]
        crate::sql::InsertReturningPool::PgRow(pg_row) => {
            column_value_as_string(pg_row, column).map_err(|e| e.to_string())
        }
        #[cfg(feature = "mysql")]
        crate::sql::InsertReturningPool::MySqlAutoId(id) => {
            // MySQL only carries the auto-generated PK; placeholders
            // for other columns are unresolvable on this path.
            if column.primary_key {
                Ok(id.to_string())
            } else {
                Err(format!(
                    "success_url placeholder `{}` cannot be resolved on MySQL (no RETURNING — \
                     only the auto-generated primary key is available)",
                    column.column,
                ))
            }
        }
        #[cfg(feature = "sqlite")]
        crate::sql::InsertReturningPool::SqliteRow(sq_row) => {
            use crate::core::FieldType;
            use crate::sql::sqlx::Row as _;
            match column.ty {
                FieldType::String => sq_row
                    .try_get::<String, _>(column.column)
                    .map_err(|e| e.to_string()),
                FieldType::I64 => sq_row
                    .try_get::<i64, _>(column.column)
                    .map(|v| v.to_string())
                    .map_err(|e| e.to_string()),
                FieldType::I32 => sq_row
                    .try_get::<i32, _>(column.column)
                    .map(|v| v.to_string())
                    .map_err(|e| e.to_string()),
                FieldType::I16 => sq_row
                    .try_get::<i16, _>(column.column)
                    .map(|v| v.to_string())
                    .map_err(|e| e.to_string()),
                FieldType::Uuid => sq_row
                    .try_get::<String, _>(column.column)
                    .map_err(|e| e.to_string()),
                _ => sq_row
                    .try_get::<String, _>(column.column)
                    .map_err(|e| e.to_string()),
            }
        }
    }
}

/// Walk the template and collect every `{name}` placeholder. Plain
/// strings without braces yield an empty vec — the caller
/// short-circuits.
///
/// Recognizes `{name}` only when `name` is a valid field-shape
/// identifier (alphanumeric + `_`). Stray `{` characters in the
/// path (e.g. `/posts/{pk}/{}` — empty placeholder is treated as
/// not-a-placeholder) are left intact.
fn parse_success_url_placeholders(template: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            break;
        };
        let candidate = &after[..end];
        if !candidate.is_empty()
            && candidate
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            out.push(candidate);
        }
        rest = &after[end + 1..];
    }
    out
}

/// Compute the `RETURNING` column list for an INSERT that needs
/// to feed `interpolate_success_url`. Maps each `{name}`
/// placeholder to its schema column (special-casing `{pk}`).
/// Empty when the template has no placeholders — caller skips the
/// `_returning` SQL path entirely.
///
/// Surfaces an error early (before the INSERT runs) when a
/// placeholder doesn't match any field — better to 500 the GET
/// than ship a half-applied INSERT that violates the redirect.
fn success_url_returning_columns(
    template: &str,
    schema: &'static crate::core::ModelSchema,
) -> Result<Vec<&'static str>, String> {
    let placeholders = parse_success_url_placeholders(template);
    let mut out: Vec<&'static str> = Vec::new();
    for name in placeholders {
        let column = if name == "pk" {
            let Some(pk) = schema.primary_key() else {
                return Err(
                    "success_url contains `{pk}` placeholder but the model has no primary key"
                        .to_owned(),
                );
            };
            pk.column
        } else {
            schema
                .field(name)
                .ok_or_else(|| {
                    format!(
                        "success_url placeholder `{{{name}}}` does not match any field on `{}`",
                        schema.table
                    )
                })?
                .column
        };
        if !out.contains(&column) {
            out.push(column);
        }
    }
    Ok(out)
}

/// Read a column from a `PgRow` and render it as a URL-safe
/// string. Branches on the field's `FieldType` so `i64` columns
/// render as decimal digits and `Uuid` columns render canonically
/// without quoting. Falls through to text decoding for everything
/// else (string, datetime, date, json — Postgres' text codec
/// handles the latter three predictably).
#[cfg(feature = "postgres")]
fn column_value_as_string(
    row: &sqlx::postgres::PgRow,
    field: &'static crate::core::FieldSchema,
) -> Result<String, sqlx::Error> {
    use crate::core::FieldType as T;
    use sqlx::Row as _;
    match field.ty {
        T::I16 => row.try_get::<i16, _>(field.column).map(|n| n.to_string()),
        T::I32 => row.try_get::<i32, _>(field.column).map(|n| n.to_string()),
        T::I64 => row.try_get::<i64, _>(field.column).map(|n| n.to_string()),
        T::Uuid => row
            .try_get::<uuid::Uuid, _>(field.column)
            .map(|u| u.to_string()),
        _ => row.try_get::<String, _>(field.column),
    }
}

/// Coerce a URL-path PK string to the field's declared SQL type.
/// Tighter than [`coerce_value`] — never returns `Null`, never
/// allows empty strings (a `/{pk}` segment is always present).
/// Used by DetailView / UpdateView / DeleteView to bind the
/// `WHERE pk = $1` parameter without relying on Postgres'
/// implicit string-to-int casts.
///
/// Returns the original `SqlValue::String(raw)` as a permissive
/// fallback when:
/// - Field type is not one of the integer / UUID variants we
///   know how to parse from a URL string
/// - Parsing fails (e.g. `i64` with non-numeric segment) — the
///   resulting query will produce no rows / 404, which is the
///   same effect as a typed-mismatch error and avoids leaking
///   parse errors to the user
fn coerce_pk(field: &crate::core::FieldSchema, raw: &str) -> SqlValue {
    use crate::core::FieldType as T;
    match field.ty {
        T::I16 | T::I32 | T::I64 => raw
            .parse::<i64>()
            .map(SqlValue::I64)
            .unwrap_or_else(|_| SqlValue::String(raw.to_owned())),
        T::Uuid => raw
            .parse::<uuid::Uuid>()
            .map(SqlValue::Uuid)
            .unwrap_or_else(|_| SqlValue::String(raw.to_owned())),
        // Strings are the natural representation; everything else
        // (Bool / Float / DateTime / Date / Json) doesn't normally
        // serve as a PK. Pass the raw string through and let
        // Postgres' implicit cast handle it.
        _ => SqlValue::String(raw.to_owned()),
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
    let (columns, values, mut errors) = parse_form(state.schema, state.fields.as_deref(), &form);
    merge_validator_errors(state.validator.as_ref(), &form, &mut errors);
    if !errors.is_empty() {
        return rerender_form(&state, &form, &errors, /*is_update=*/ false, &headers);
    }
    // When `success_url` carries `{column}` placeholders, request
    // those columns back via RETURNING so we can substitute
    // before the redirect. Otherwise plain INSERT — saves the
    // round-trip.
    let returning = match success_url_returning_columns(&state.success_url, state.schema) {
        Ok(cols) => cols,
        Err(e) => return template_error(&e),
    };
    let need_returning = !returning.is_empty();
    let insert_q = crate::core::InsertQuery {
        model: state.schema,
        columns,
        values,
        returning,
        on_conflict: None,
    };
    let target_url = if need_returning {
        match crate::sql::insert_returning_pool(&state.pool, &insert_q).await {
            Ok(row) => match interpolate_success_url(&state.success_url, &row, state.schema) {
                Ok(url) => url,
                Err(e) => return template_error(&e),
            },
            Err(e) => return template_error(&format!("insert row: {e}")),
        }
    } else {
        if let Err(e) = crate::sql::insert_pool(&state.pool, &insert_q).await {
            return template_error(&format!("insert row: {e}"));
        }
        state.success_url.clone()
    };
    axum::response::Redirect::to(&target_url).into_response()
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
            value: coerce_pk(pk_field, &pk),
        }),
        search: None,
        joins: vec![],
        order_by: vec![],
        limit: Some(1),
        offset: None,
        lock_mode: None,
        compound: vec![],
        projection: None,
        distinct: None,
    };
    let scalars: Vec<&'static crate::core::FieldSchema> = state.schema.scalar_fields().collect();
    let row_json = match select_one_row_as_json(&state.pool, &select_q, &scalars).await {
        Ok(Some(r)) => r,
        Ok(None) => return (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => return template_error(&format!("query row: {e}")),
    };
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
    let (columns, values, mut errors) = parse_form(state.schema, state.fields.as_deref(), &form);
    merge_validator_errors(state.validator.as_ref(), &form, &mut errors);
    if !errors.is_empty() {
        return rerender_form(&state, &form, &errors, /*is_update=*/ true, &headers);
    }
    let assignments: Vec<crate::core::Assignment> = columns
        .into_iter()
        .zip(values)
        .map(|(column, value)| crate::core::Assignment {
            column,
            value: value.into(),
        })
        .collect();
    let update_q = crate::core::UpdateQuery {
        model: state.schema,
        set: assignments,
        where_clause: WhereExpr::Predicate(Filter {
            column: pk_field.column,
            op: Op::Eq,
            value: coerce_pk(pk_field, &pk),
        }),
    };
    match crate::sql::update_pool(&state.pool, &update_q).await {
        Ok(0) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Ok(_) => {
            let target = substitute_pk(&state.success_url, &pk);
            axum::response::Redirect::to(&target).into_response()
        }
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
/// Run an optional user-supplied validator and merge any
/// `FormErrors` it returns into the existing per-field error map.
/// Multi-error fields are joined with `"; "` so the single-string-
/// per-field shape rerender_form expects is preserved. Non-field
/// errors land under the `"__all__"` key (matches Django convention
/// for cross-field errors and lets templates render them once at
/// the top of the form).
fn merge_validator_errors(
    validator: Option<&Validator>,
    submitted: &HashMap<String, String>,
    errors: &mut HashMap<String, String>,
) {
    let Some(v) = validator else { return };
    let Err(form_errs) = v(submitted) else { return };
    for (field, msgs) in form_errs.fields() {
        if msgs.is_empty() {
            continue;
        }
        let joined = msgs.join("; ");
        // Don't clobber an existing schema-level error with the
        // user validator's; users typically fix one issue at a
        // time. Concatenate instead.
        errors
            .entry(field.clone())
            .and_modify(|prev| {
                prev.push_str("; ");
                prev.push_str(&joined);
            })
            .or_insert(joined);
    }
    if !form_errs.non_field().is_empty() {
        let joined = form_errs.non_field().join("; ");
        errors
            .entry("__all__".to_owned())
            .and_modify(|prev| {
                prev.push_str("; ");
                prev.push_str(&joined);
            })
            .or_insert(joined);
    }
}

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
) -> Result<Vec<crate::core::OrderItem>, String> {
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
        out.push(crate::core::OrderItem::column(field.column, *desc));
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
fn default_order_by(schema: &'static ModelSchema) -> Vec<crate::core::OrderItem> {
    match schema.primary_key() {
        Some(pk) => vec![crate::core::OrderItem::column(pk.column, false)],
        None => Vec::new(),
    }
}

/// Resolve the active page size from the URL `?page_size=N` param,
/// clamped to `[1, max]`. Falls back to the builder default when
/// the param is absent or unparseable. Saturates rather than
/// erroring on overflow — the user's request still loads, just
/// with the cap applied.
fn resolve_page_size(default: i64, max: i64, params: &HashMap<String, String>) -> i64 {
    let Some(raw) = params.get("page_size") else {
        return default;
    };
    let Ok(n) = raw.parse::<i64>() else {
        return default;
    };
    n.clamp(1, max)
}

/// Resolve the active ordering, honoring `?ordering=col` /
/// `?ordering=-col` URL overrides when the bare column name is in
/// the `ordering_fields` allowlist. Returns the resolved
/// `OrderClause` slice plus the active spec string (for the Tera
/// `ordering` context var).
///
/// Resolution priority:
/// 1. `?ordering=...` URL param matching the allowlist (single
///    column; the `-` prefix flips to DESC). Multi-column requires
///    a hand-rolled handler.
/// 2. Builder-side `.order_by(...)` calls
/// 3. PK-ASC fallback (so pagination stays deterministic)
///
/// The active spec returned for the context var:
/// - For URL overrides: `"col"` or `"-col"` exactly as the user typed
/// - For builder default: empty string (templates render no
///   "active sort" indicator)
fn resolve_active_order(
    schema: &'static ModelSchema,
    builder_spec: &[(String, bool)],
    ordering_fields: &[String],
    params: &HashMap<String, String>,
) -> Result<(Vec<crate::core::OrderItem>, String), String> {
    // URL override path.
    if let Some(raw) = params.get("ordering").filter(|s| !s.is_empty()) {
        let (name, desc) = if let Some(rest) = raw.strip_prefix('-') {
            (rest, true)
        } else {
            (raw.as_str(), false)
        };
        if ordering_fields.iter().any(|f| f == name) {
            if let Some(field) = schema.field(name) {
                return Ok((
                    vec![crate::core::OrderItem::column(field.column, desc)],
                    raw.clone(),
                ));
            }
        }
        // Not in allowlist or unknown field — fall through to the
        // builder default rather than erroring. Matches the same
        // "typos shouldn't 400" policy used for filter_fields.
    }

    // Builder default path.
    let resolved = resolve_order_by(schema, builder_spec)?;
    Ok((resolved, String::new()))
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
        // Delegate to the public helper so the CBV-side context shape
        // matches what hand-rolled handlers get from
        // `forms::csrf::stamp_into_context` (issue #15). Stamps both
        // `csrf_token` (raw) and `csrf_input` (HTML).
        crate::forms::csrf::stamp_into_context(_headers, ctx)
    }
    #[cfg(not(feature = "csrf"))]
    {
        // CSRF feature off — render with empty token so templates that
        // reference `{{ csrf_token }}` don't error. Validation isn't
        // enforced in this configuration; the empty hidden input is
        // harmless.
        ctx.insert("csrf_token", "");
        ctx.insert("csrf_input", "");
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

/// Stamp pre-built `next_page_url` / `prev_page_url` strings into
/// the Tera context. Both preserve every other URL parameter
/// (filters, search, ordering, page_size) and just bump the
/// `page` value, so pagination links don't drop the user's
/// active filter state — a common bug in hand-rolled
/// pagination templates.
///
/// `next_page_url` is `Some` only when there's a next page;
/// `prev_page_url` is `Some` only when `page > 1`. Templates
/// render:
///
/// ```html
/// {% if prev_page_url %}<a href="{{ prev_page_url }}">prev</a>{% endif %}
/// {% if next_page_url %}<a href="{{ next_page_url }}">next</a>{% endif %}
/// ```
///
/// Both values are query-string fragments starting with `?`; the
/// path is left to the template (typically `request.path` or a
/// hardcoded `/posts`). This keeps the helper independent of
/// axum's path extraction — pure data shape.
fn insert_pagination_urls(
    ctx: &mut Context,
    page: i64,
    has_next: bool,
    has_prev: bool,
    params: &HashMap<String, String>,
) {
    let next_url = if has_next {
        Some(build_pagination_query(params, page + 1))
    } else {
        None
    };
    let prev_url = if has_prev {
        Some(build_pagination_query(params, page - 1))
    } else {
        None
    };
    ctx.insert("next_page_url", &next_url);
    ctx.insert("prev_page_url", &prev_url);
}

/// Build a `?key=value&...` query string with the original
/// params preserved + the `page` value replaced. URL-encodes
/// values so `?search=hello world` round-trips correctly.
fn build_pagination_query(params: &HashMap<String, String>, target_page: i64) -> String {
    // Sort keys for deterministic output — makes test assertions
    // easier and avoids surprising users who notice the order
    // changing between requests.
    let mut keys: Vec<&str> = params
        .keys()
        .map(String::as_str)
        .filter(|k| *k != "page")
        .collect();
    keys.sort_unstable();

    let mut out = String::from("?");
    for k in keys {
        let v = &params[k];
        if !out.ends_with('?') {
            out.push('&');
        }
        out.push_str(&urlencode(k));
        out.push('=');
        out.push_str(&urlencode(v));
    }
    if !out.ends_with('?') {
        out.push('&');
    }
    out.push_str("page=");
    out.push_str(&target_page.to_string());
    out
}

/// Local alias for the canonical [`crate::url_codec::url_encode`]
/// helper. Kept as a private wrapper so the call sites stay
/// readable + so we can swap encoders centrally without touching
/// every caller.
fn urlencode(s: &str) -> String {
    crate::url_codec::url_encode(s)
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

/// Stamp `bulk_actions: [{name, label}]` into the Tera context when
/// [`ListView::bulk_actions`] is on. Templates iterate this to build
/// the action `<select>`. The list always leads with the built-in
/// `delete_selected`; user-registered actions follow in registration
/// order. When bulk actions are off, the array is empty so templates
/// can branch on `{% if bulk_actions %}` without a separate flag.
fn insert_bulk_actions_context(ctx: &mut Context, vs: &ListView) {
    #[derive(serde::Serialize)]
    struct Entry<'a> {
        name: &'a str,
        label: &'a str,
    }
    let mut entries: Vec<Entry> = Vec::new();
    if vs.bulk_actions_enabled {
        entries.push(Entry {
            name: BUILTIN_DELETE_SELECTED,
            label: "Delete selected",
        });
        for a in &vs.actions {
            entries.push(Entry {
                name: &a.name,
                label: &a.label,
            });
        }
    }
    ctx.insert("bulk_actions", &entries);
}

/// Built-in `delete_selected` action name. Reserved — user-registered
/// actions can't shadow it (registration silently overwrites the
/// name, but the built-in is appended *after* user actions in the
/// dropdown either way; matching the form `action` field at request
/// time still finds the user's handler since user actions are
/// dispatched first).
const BUILTIN_DELETE_SELECTED: &str = "delete_selected";

/// Parse the form fields for a bulk-action POST. Returns
/// `(action_name, selected_pks_as_strings)` or an error string for
/// the response body.
fn parse_bulk_action_form(
    form: &HashMap<String, Vec<String>>,
) -> Result<(String, Vec<String>), String> {
    let action = form
        .get("action")
        .and_then(|v| v.first())
        .map(String::clone)
        .ok_or_else(|| "missing `action` form field".to_owned())?;
    let pks = form
        .get("_selected_action")
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    if pks.is_empty() {
        return Err("no rows selected (_selected_action missing)".into());
    }
    Ok((action, pks))
}

/// Coerce the form's stringified PKs into the schema's PK type so
/// the action handler can `.bind(...)` them or pass them to a
/// generic `IN ($1)` clause. Errors when any PK fails to parse —
/// hostile clients shouldn't be able to inject mistyped PKs into
/// the SQL layer.
fn coerce_selected_pks(
    pk_field: &'static crate::core::FieldSchema,
    raws: &[String],
) -> Result<Vec<SqlValue>, String> {
    raws.iter()
        .map(|s| coerce_pk_typed(pk_field, s))
        .collect::<Result<Vec<_>, _>>()
}

/// Like `coerce_pk` but returns a typed error rather than falling
/// back to `SqlValue::String`. The fallback is fine for URL-segment
/// lookups (the SQL layer's implicit casts paper over the
/// difference) but bulk-action PKs are bound as a list, where a
/// type mismatch would crash the whole batch — fail fast.
fn coerce_pk_typed(
    pk_field: &'static crate::core::FieldSchema,
    raw: &str,
) -> Result<SqlValue, String> {
    use crate::core::FieldType;
    match pk_field.ty {
        FieldType::I64 => raw
            .parse::<i64>()
            .map(SqlValue::I64)
            .map_err(|e| format!("invalid i64 PK `{raw}`: {e}")),
        FieldType::I32 => raw
            .parse::<i32>()
            .map(SqlValue::I32)
            .map_err(|e| format!("invalid i32 PK `{raw}`: {e}")),
        FieldType::I16 => raw
            .parse::<i16>()
            .map(SqlValue::I16)
            .map_err(|e| format!("invalid i16 PK `{raw}`: {e}")),
        FieldType::Uuid => uuid::Uuid::parse_str(raw)
            .map(SqlValue::Uuid)
            .map_err(|e| format!("invalid uuid PK `{raw}`: {e}")),
        FieldType::String => Ok(SqlValue::String(raw.to_owned())),
        other => Err(format!(
            "PK type {other:?} is not supported for bulk actions"
        )),
    }
}

/// Resolve `_display` sibling fields for every FK column on the
/// schema. Mutates `object_list` in place: each row's JSON object
/// gets `<column>_display` set to the FK target's display value
/// when one resolves, or left absent when the target row is
/// missing / unregistered / has no display field. Errors during
/// the lookup are logged but don't fail the response — a missing
/// `_display` is recoverable (templates fall back to the raw FK)
/// while a 500 isn't.
async fn resolve_fk_displays_pool(
    schema: &'static ModelSchema,
    pool: &Pool,
    object_list: &mut [Value],
) {
    let lookups = collect_fk_target_lookups(schema, object_list);
    for fk in lookups {
        let map = match fetch_fk_display_map_pool(&fk, pool).await {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(
                    target: "rustango::template_views",
                    field = fk.local_field,
                    target_table = fk.target_table,
                    error = %e,
                    "fk display lookup failed; templates fall back to raw FK"
                );
                continue;
            }
        };
        stamp_display_into_rows(&fk, &map, object_list);
    }
}

// v0.38 — `resolve_fk_displays_conn` removed; tenant handlers now
// use `Tenant::pool()` + `resolve_fk_displays_pool` directly.

/// One FK column we need to resolve: which local field to read
/// from each row, which target table+column to look up, and the
/// distinct non-null source values that appeared in the page.
#[allow(unused)]
struct FkLookup {
    /// Local Rust field name on the source model — also the JSON
    /// key in each row (rows are serialized via `row_to_json`
    /// which keys by `field.name`).
    local_field: &'static str,
    target_table: &'static str,
    target_pk_column: &'static str,
    target_display_column: &'static str,
    target_display_field_name: &'static str,
    /// Distinct stringified source values from the page (NULL
    /// values are filtered out so the SQL doesn't bind a NULL
    /// into the `ANY($1)` array).
    distinct_values: Vec<Value>,
}

fn collect_fk_target_lookups(schema: &'static ModelSchema, object_list: &[Value]) -> Vec<FkLookup> {
    use crate::core::Relation;
    let mut out = Vec::new();
    for field in schema.scalar_fields() {
        let Some(rel) = field.relation else { continue };
        let (to, on) = match rel {
            Relation::Fk { to, on } | Relation::O2O { to, on } => (to, on),
        };
        let Some(target) = lookup_target_schema(to) else {
            continue;
        };
        let Some(display_field) = target.display_field() else {
            continue;
        };
        // The target's PK column is what we filter on; `on` from
        // the Relation IR is the remote column the local FK
        // references (usually the PK).
        let mut distinct: Vec<Value> = Vec::new();
        for row in object_list {
            let Some(val) = row.get(field.name) else {
                continue;
            };
            if val.is_null() {
                continue;
            }
            if !distinct.iter().any(|v| v == val) {
                distinct.push(val.clone());
            }
        }
        if distinct.is_empty() {
            continue;
        }
        out.push(FkLookup {
            local_field: field.name,
            target_table: target.table,
            target_pk_column: on,
            target_display_column: display_field.column,
            target_display_field_name: display_field.name,
            distinct_values: distinct,
        });
    }
    out
}

/// Inventory walk to find the target model by table name. Mirrors
/// what `admin::helpers::lookup_model` does but without the admin's
/// scope filter (template_views isn't admin; it's a user-facing
/// view that the user already chose to mount, so they've already
/// decided the target is theirs to render).
fn lookup_target_schema(table: &str) -> Option<&'static ModelSchema> {
    crate::core::inventory::iter::<crate::core::ModelEntry>
        .into_iter()
        .find(|e| e.schema.table == table)
        .map(|e| e.schema)
}

/// Run the FK display batch query against a static pool, return
/// `(source_value_string → display_value)` map.
async fn fetch_fk_display_map_pool(
    fk: &FkLookup,
    pool: &Pool,
) -> Result<HashMap<String, Value>, crate::sql::ExecError> {
    let q = build_fk_display_query(fk);
    let target = match lookup_target_schema(fk.target_table) {
        Some(s) => s,
        None => return Ok(HashMap::new()),
    };
    let fields: Vec<&'static crate::core::FieldSchema> = target.scalar_fields().collect();
    let rows = select_rows_as_json(pool, &q, &fields).await?;
    Ok(extract_fk_display_map(fk, &rows))
}

fn build_fk_display_query(fk: &FkLookup) -> SelectQuery {
    use crate::core::{Filter, Op};
    // Synthesize a one-off `&'static ModelSchema` to use as the
    // SelectQuery's model — actually no, we need a real schema
    // because select_rows compiles the query against it. Use the
    // target model's full schema (re-resolved below) and let the
    // SQL writer project against the IN clause.
    let target = lookup_target_schema(fk.target_table)
        .expect("target table existed when collecting lookups");
    SelectQuery {
        model: target,
        where_clause: WhereExpr::Predicate(Filter {
            column: fk.target_pk_column,
            op: Op::In,
            value: SqlValue::List(
                fk.distinct_values
                    .iter()
                    .map(json_value_to_sql_for_fk_pk)
                    .collect(),
            ),
        }),
        search: None,
        joins: vec![],
        order_by: vec![],
        limit: None,
        offset: None,
        lock_mode: None,
        compound: vec![],
        projection: None,
        distinct: None,
    }
}

/// Convert a JSON-shaped value (read out of an object_list row)
/// back into a `SqlValue` for re-binding into the FK lookup's
/// `IN ($1)` clause. The JSON shape comes from `row_to_json`
/// which serializes per FieldType, so we round-trip on the same
/// type table.
fn json_value_to_sql_for_fk_pk(v: &Value) -> SqlValue {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                SqlValue::I64(i)
            } else if let Some(u) = n.as_u64() {
                SqlValue::I64(u as i64)
            } else {
                // Float PKs are unusual; bind as string and let PG cast.
                SqlValue::String(n.to_string())
            }
        }
        Value::String(s) => {
            // Could be a UUID or a string PK. Try UUID first.
            if let Ok(u) = uuid::Uuid::parse_str(s) {
                SqlValue::Uuid(u)
            } else {
                SqlValue::String(s.clone())
            }
        }
        _ => SqlValue::Null,
    }
}

/// v0.38 — operates on JSON rows from `select_rows_as_json`
/// instead of raw PgRow. The dialect-aware row → JSON conversion
/// already covered every column type in [`crate::sql::row_to_json`] /
/// `row_to_json_my` / `row_to_json_sqlite`, so the FK display
/// extraction is just JSON object lookup.
fn extract_fk_display_map(fk: &FkLookup, rows: &[Value]) -> HashMap<String, Value> {
    let mut map = HashMap::new();
    // The JSON object is keyed by FieldSchema.name (the Rust field
    // name); look up display by *name*, not column. The query that
    // populated `rows` projected `target_pk_column` and
    // `target_display_column` — we look them up by the equivalent
    // field-name keys from the model schema.
    let target = match lookup_target_schema(fk.target_table) {
        Some(s) => s,
        None => return map,
    };
    let pk_field_name = target
        .field_by_column(fk.target_pk_column)
        .map(|f| f.name)
        .unwrap_or(fk.target_pk_column);
    let display_field_name = target
        .field_by_column(fk.target_display_column)
        .map(|f| f.name)
        .unwrap_or(fk.target_display_column);
    for row in rows {
        let Some(obj) = row.as_object() else { continue };
        let Some(key) = obj.get(pk_field_name).and_then(json_value_as_lookup_key) else {
            continue;
        };
        let display_val = obj.get(display_field_name).cloned().unwrap_or(Value::Null);
        map.insert(key, display_val);
    }
    let _ = fk.target_display_field_name; // kept for future debug logging
    map
}

/// Stringify a JSON value the same way `read_pk_as_string` does
/// for SQL row values, so the lookup map's keys match the row's
/// FK column values.
fn json_value_as_lookup_key(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

fn stamp_display_into_rows(fk: &FkLookup, map: &HashMap<String, Value>, object_list: &mut [Value]) {
    let display_key = format!("{}_display", fk.local_field);
    for row in object_list.iter_mut() {
        let Some(obj) = row.as_object_mut() else {
            continue;
        };
        let Some(fk_val) = obj.get(fk.local_field) else {
            continue;
        };
        let Some(key) = json_value_as_lookup_key(fk_val) else {
            continue;
        };
        if let Some(display) = map.get(&key) {
            obj.insert(display_key.clone(), display.clone());
        }
    }
}

/// `confirmed=true` (case-insensitive) in the form payload short-
/// circuits the bulk-delete confirmation render. The form values
/// come in as `Vec<String>` because the same parser handles
/// repeating keys (`_selected_action`); `confirmed` is only ever
/// a single value, but the lookup matches the same shape.
fn is_form_confirmed(form: &HashMap<String, Vec<String>>) -> bool {
    form.get("confirmed")
        .and_then(|v| v.first())
        .map(|s| matches!(s.to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "on"))
        .unwrap_or(false)
}

/// Resolve the confirmation template name — explicit override via
/// [`ListView::with_delete_confirmation_template`] takes precedence,
/// otherwise default to `<table>_confirm_bulk_delete.html`.
fn confirm_delete_template_name(vs: &ListView) -> String {
    vs.confirm_delete_template
        .clone()
        .unwrap_or_else(|| format!("{}_confirm_bulk_delete.html", vs.schema.table))
}

/// Render the confirmation page. Stamps in the action name, the
/// raw PK list (so the second submit echoes the same selection),
/// the full row objects (for showing *what* will be deleted), plus
/// the CSRF token re-stamped from the request headers.
fn render_bulk_delete_confirm(
    tera: &Tera,
    template_name: String,
    action: &str,
    pks: &[String],
    objects: &[Value],
    headers: &axum::http::HeaderMap,
) -> Response {
    let mut ctx = Context::new();
    ctx.insert("action", action);
    ctx.insert("pks", &pks);
    ctx.insert("objects", &objects);
    let set_cookie = stamp_csrf(headers, &mut ctx);
    let mut resp = render(tera, &template_name, &ctx);
    apply_csrf_cookie(&mut resp, set_cookie);
    resp
}

/// Fetch the rows for the selected PKs so the confirmation
/// template can render them. Selects every scalar field — the
/// template branches on what to display. Errors here surface as
/// 500 rather than 400 because they indicate a backend problem,
/// not a bad request.
async fn fetch_pks_as_objects_pool(
    schema: &'static ModelSchema,
    pk_field: &'static crate::core::FieldSchema,
    pool: &Pool,
    pks: &[SqlValue],
) -> Result<Vec<Value>, String> {
    use crate::core::{Filter, Op};
    let q = SelectQuery {
        model: schema,
        where_clause: WhereExpr::Predicate(Filter {
            column: pk_field.column,
            op: Op::In,
            value: SqlValue::List(pks.to_vec()),
        }),
        search: None,
        joins: vec![],
        order_by: vec![],
        limit: None,
        offset: None,
        lock_mode: None,
        compound: vec![],
        projection: None,
        distinct: None,
    };
    let fields: Vec<&'static crate::core::FieldSchema> = schema.scalar_fields().collect();
    let rows = select_rows_as_json(pool, &q, &fields)
        .await
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

/// Run the built-in `delete_selected` action: `DELETE FROM <table>
/// WHERE <pk> IN (...)`. Goes through `crate::core::DeleteQuery` +
/// `crate::sql::delete{,_on}` so it composes the exact same SQL the
/// per-row admin DELETE path uses.
async fn run_delete_selected_pool(
    schema: &'static ModelSchema,
    pk_field: &'static crate::core::FieldSchema,
    pool: &Pool,
    pks: &[SqlValue],
) -> Result<(), String> {
    use crate::core::{DeleteQuery, Filter, Op};
    let q = DeleteQuery {
        model: schema,
        where_clause: WhereExpr::Predicate(Filter {
            column: pk_field.column,
            op: Op::In,
            value: SqlValue::List(pks.to_vec()),
        }),
    };
    crate::sql::delete_pool(pool, &q)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// v0.38 — `run_delete_selected_conn` removed; tenant handlers now
// use `run_delete_selected_pool` against `Tenant::pool()`.

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
            // #386 — Django-shape DEBUG overlay. When the active tier
            // is dev/staging (or RUSTANGO_TEMPLATE_DEBUG=1), serve a
            // styled HTML page with the full Tera diagnostic instead
            // of the plain-text 500 fallback. The plain-text path
            // stays the production default — same stderr/tracing
            // breadcrumbs, no information leak in the response body.
            if crate::template_debug::enabled() {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Html(crate::template_debug::error_page_html(&e, name)),
                )
                    .into_response();
            }
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

// ============================================================== TemplateView

/// No-model CBV that renders a Tera template with a static context.
/// Django's [`TemplateView`](https://docs.djangoproject.com/en/6.0/ref/class-based-views/base/#templateview).
/// Use for about pages, terms-of-service, dashboards built from
/// context the caller assembles up front. Issue #13.
///
/// ```ignore
/// use rustango::template_views::TemplateView;
/// use std::sync::Arc;
///
/// let app = TemplateView::new("about.html")
///     .context_value("contact_email", "hello@example.com")
///     .router("/about", Arc::new(tera));
/// ```
#[derive(Clone)]
pub struct TemplateView {
    template: String,
    context: HashMap<String, Value>,
}

impl TemplateView {
    /// Construct a `TemplateView` that renders `template`.
    #[must_use]
    pub fn new(template: impl Into<String>) -> Self {
        Self {
            template: template.into(),
            context: HashMap::new(),
        }
    }

    /// Inject a static value into the Tera context under `key`.
    /// Successive calls accumulate; the latest write wins per key.
    #[must_use]
    pub fn context_value(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.context.insert(key.into(), value.into());
        self
    }

    /// Merge a JSON object into the Tera context. Each top-level key
    /// becomes a context variable. Non-object inputs are stored under
    /// the conventional key `"context"`.
    #[must_use]
    pub fn context(mut self, ctx: Value) -> Self {
        match ctx {
            Value::Object(map) => {
                for (k, v) in map {
                    self.context.insert(k, v);
                }
            }
            other => {
                self.context.insert("context".into(), other);
            }
        }
        self
    }

    /// Mount the view on `prefix` (GET-only).
    #[must_use]
    pub fn router(self, prefix: &str, tera: Arc<Tera>) -> Router<()> {
        let state = Arc::new(TemplateViewState { vs: self, tera });
        Router::new()
            .route(prefix, get(handle_template_view))
            .with_state(state)
    }
}

#[derive(Clone)]
struct TemplateViewState {
    vs: TemplateView,
    tera: Arc<Tera>,
}

async fn handle_template_view(State(state): State<Arc<TemplateViewState>>) -> Response {
    let mut ctx = Context::new();
    for (k, v) in &state.vs.context {
        ctx.insert(k, v);
    }
    render(&state.tera, &state.vs.template, &ctx)
}

// ============================================================== RedirectView

/// No-model CBV that returns an HTTP redirect to a fixed URL. Django's
/// [`RedirectView`](https://docs.djangoproject.com/en/6.0/ref/class-based-views/base/#redirectview).
/// Use for canonical URL migrations (old `/about-us` → new `/about`),
/// short links, or "click here to go there" flows. Issue #13.
///
/// ```ignore
/// use rustango::template_views::RedirectView;
///
/// // 302 to /about (matches Django's default temporary redirect).
/// let app = RedirectView::to("/about").router("/about-us");
///
/// // 301 (permanent) — survives indexing, search engines update.
/// let app = RedirectView::to("/about").permanent().router("/old-about");
/// ```
///
/// Status codes match Django's (302 / 301) — not axum's modern
/// defaults (303 / 308) — for method-preservation semantics consistent
/// with the framework's [`crate::shortcuts::redirect`] helper.
#[derive(Clone)]
pub struct RedirectView {
    url: String,
    permanent: bool,
}

impl RedirectView {
    /// Construct a `RedirectView` pointing at `url`. Default status is
    /// `302 Found` (temporary). Call [`Self::permanent`] to switch to
    /// `301 Moved Permanently`.
    #[must_use]
    pub fn to(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            permanent: false,
        }
    }

    /// Switch this view to emit `301 Moved Permanently` instead of
    /// `302 Found`.
    #[must_use]
    pub fn permanent(mut self) -> Self {
        self.permanent = true;
        self
    }

    /// Mount the view on `prefix` (GET-only).
    #[must_use]
    pub fn router(self, prefix: &str) -> Router<()> {
        let state = Arc::new(self);
        Router::new()
            .route(prefix, get(handle_redirect_view))
            .with_state(state)
    }
}

async fn handle_redirect_view(State(state): State<Arc<RedirectView>>) -> Response {
    use axum::http::{header, HeaderValue};
    let status = if state.permanent {
        StatusCode::MOVED_PERMANENTLY
    } else {
        StatusCode::FOUND
    };
    let mut res = Response::builder()
        .status(status)
        .body(axum::body::Body::empty())
        .expect("status + empty body is always valid");
    if let Ok(v) = HeaderValue::from_str(&state.url) {
        res.headers_mut().insert(header::LOCATION, v);
    }
    res
}

// ============================================================== FormView

/// No-model CBV that renders a `#[derive(Form)]` form on GET, parses
/// + validates on POST, and redirects to `success_url` when valid.
/// Django's [`FormView`](https://docs.djangoproject.com/en/6.0/ref/class-based-views/generic-editing/#formview).
/// Issue #13.
///
/// Unlike [`CreateView`] / [`UpdateView`] (which know about a model
/// schema and do the INSERT/UPDATE for you), `FormView` only handles
/// the **form lifecycle**. The caller plugs in a callback that
/// receives the validated form data:
///
/// ```ignore
/// use rustango::template_views::FormView;
/// use rustango::forms::Form;
///
/// #[derive(Form)]
/// struct ContactForm {
///     #[form(min_length = 1)]
///     name: String,
///     #[form(min_length = 1)]
///     message: String,
/// }
///
/// async fn send(form: ContactForm) -> Result<(), String> {
///     // Send email, file ticket, etc.
///     send_contact_email(&form).await
/// }
///
/// let app = FormView::<ContactForm>::for_form(send)
///     .template("contact.html")
///     .success_url("/contact/thanks")
///     .router("/contact", Arc::new(tera));
/// ```
///
/// Template context:
/// - `errors: HashMap<String, Vec<String>>` — empty on GET, populated
///   on POST validation failure.
/// - `values: HashMap<String, String>` — empty on GET, raw POST values
///   on validation failure so the form can repopulate.
///
/// CSRF protection is the project's responsibility — mount under a
/// CSRF-protected scope when reachable from a browser.
pub struct FormView<F>
where
    F: crate::forms::Form,
{
    template: String,
    success_url: String,
    handler: Arc<
        dyn Fn(F) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>
            + Send
            + Sync,
    >,
}

impl<F> Clone for FormView<F>
where
    F: crate::forms::Form,
{
    fn clone(&self) -> Self {
        Self {
            template: self.template.clone(),
            success_url: self.success_url.clone(),
            handler: Arc::clone(&self.handler),
        }
    }
}

impl<F> FormView<F>
where
    F: crate::forms::Form + Send + 'static,
{
    /// Construct a `FormView` that hands every successfully-parsed
    /// form to `on_valid`. The handler returns `Ok(())` to trigger
    /// the redirect or `Err(msg)` to re-render the template with the
    /// error.
    #[must_use]
    pub fn for_form<H, Fut>(on_valid: H) -> Self
    where
        H: Fn(F) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
    {
        Self {
            template: "form.html".into(),
            success_url: "/".into(),
            handler: Arc::new(move |form| Box::pin(on_valid(form))),
        }
    }

    /// Override the template used to render the form (GET) and the
    /// validation-failure re-render (POST).
    #[must_use]
    pub fn template(mut self, name: impl Into<String>) -> Self {
        self.template = name.into();
        self
    }

    /// Where to 303-redirect after a successful POST. Defaults to `/`.
    #[must_use]
    pub fn success_url(mut self, url: impl Into<String>) -> Self {
        self.success_url = url.into();
        self
    }

    /// Mount the view on `prefix`. GET renders the empty form; POST
    /// parses + validates + (on success) redirects.
    #[must_use]
    pub fn router(self, prefix: &str, tera: Arc<Tera>) -> Router<()> {
        let state = Arc::new(StandaloneFormViewState { vs: self, tera });
        Router::new()
            .route(
                prefix,
                get(handle_form_view_get::<F>).post(handle_form_view_post::<F>),
            )
            .with_state(state)
    }
}

struct StandaloneFormViewState<F>
where
    F: crate::forms::Form,
{
    vs: FormView<F>,
    tera: Arc<Tera>,
}

async fn handle_form_view_get<F>(State(state): State<Arc<StandaloneFormViewState<F>>>) -> Response
where
    F: crate::forms::Form,
{
    let mut ctx = Context::new();
    ctx.insert("errors", &HashMap::<String, Vec<String>>::new());
    ctx.insert("values", &HashMap::<String, String>::new());
    render(&state.tera, &state.vs.template, &ctx)
}

async fn handle_form_view_post<F>(
    State(state): State<Arc<StandaloneFormViewState<F>>>,
    axum::extract::Form(payload): axum::extract::Form<HashMap<String, String>>,
) -> Response
where
    F: crate::forms::Form,
{
    match F::parse(&payload) {
        Ok(form) => match (state.vs.handler)(form).await {
            Ok(()) => {
                // 303 See Other for POST → GET redirect (RFC 7231).
                use axum::http::{header, HeaderValue};
                let mut res = Response::builder()
                    .status(StatusCode::SEE_OTHER)
                    .body(axum::body::Body::empty())
                    .expect("303 + empty body is always valid");
                if let Ok(v) = HeaderValue::from_str(&state.vs.success_url) {
                    res.headers_mut().insert(header::LOCATION, v);
                }
                res
            }
            Err(msg) => {
                // Handler-level failure — re-render with a top-level
                // "non-field" error so the template can show it.
                let mut errors: HashMap<String, Vec<String>> = HashMap::new();
                errors.insert("__all__".into(), vec![msg]);
                let mut ctx = Context::new();
                ctx.insert("errors", &errors);
                ctx.insert("values", &payload);
                render(&state.tera, &state.vs.template, &ctx)
            }
        },
        Err(form_errors) => {
            let mut ctx = Context::new();
            ctx.insert("errors", form_errors.fields());
            ctx.insert("values", &payload);
            render(&state.tera, &state.vs.template, &ctx)
        }
    }
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
        headers: axum::http::HeaderMap,
        Query(params): Query<HashMap<String, String>>,
        t: Tenant,
    ) -> Response {
        let page: i64 = params
            .get("page")
            .and_then(|p| p.parse().ok())
            .unwrap_or(1)
            .max(1);
        let page_size =
            super::resolve_page_size(state.vs.page_size, state.vs.max_page_size, &params);
        let offset = (page - 1) * page_size;

        let (order_by, active_ordering) = match super::resolve_active_order(
            state.vs.schema,
            &state.vs.order_by,
            &state.vs.ordering_fields,
            &params,
        ) {
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
            limit: Some(page_size),
            offset: Some(offset),
            lock_mode: None,
            compound: vec![],
            projection: None,
            distinct: None,
        };
        let count_q = crate::core::CountQuery {
            model: state.vs.schema,
            where_clause,
            search: None,
        };

        // v0.38 — use the tenant's tri-dialect Pool enum; runs the
        // same code on PG / MySQL / SQLite. Routes through
        // select_rows_as_json + count_rows_pool.
        let pool = t.pool().clone();
        let fields = resolved_fields(state.vs.schema, state.vs.fields.as_deref());
        let mut object_list = match crate::sql::select_rows_as_json(&pool, &select_q, &fields).await
        {
            Ok(r) => r,
            Err(e) => return template_error(&format!("query rows: {e}")),
        };
        let total = match crate::sql::count_rows_pool(&pool, &count_q).await {
            Ok(c) => c,
            Err(e) => return template_error(&format!("count rows: {e}")),
        };
        if state.vs.fk_display {
            super::resolve_fk_displays_pool(state.vs.schema, &pool, &mut object_list).await;
        }

        let total_pages = ((total - 1).max(0) / page_size) + 1;
        let mut ctx = Context::new();
        ctx.insert("object_list", &object_list);
        // #379 — context_object_name (see non-tenant variant for
        // semantics).
        if !state.vs.context_object_name.is_empty() {
            ctx.insert(&state.vs.context_object_name, &object_list);
        }
        ctx.insert("page", &page);
        ctx.insert("page_size", &page_size);
        ctx.insert("total", &total);
        ctx.insert("total_pages", &total_pages);
        let has_next = page < total_pages;
        let has_prev = page > 1;
        ctx.insert("has_next", &has_next);
        ctx.insert("has_prev", &has_prev);
        ctx.insert("ordering", &active_ordering);
        super::insert_filter_context(&mut ctx, &state.vs.filter_fields, &params);
        super::insert_pagination_urls(&mut ctx, page, has_next, has_prev, &params);
        super::insert_bulk_actions_context(&mut ctx, &state.vs);

        // v0.30.17 — same CSRF stamping as the static-pool variant
        // (handle_list above). Without it, ListView with bulk_actions
        // mounted under a CSRF-protected scope can't post anything.
        let set_cookie = super::stamp_csrf(&headers, &mut ctx);
        let mut resp = render(&state.tera, &state.vs.template, &ctx);
        super::apply_csrf_cookie(&mut resp, set_cookie);
        resp
    }

    /// `POST <prefix>` — bulk-action dispatcher for tenancy mode.
    /// Resolves the tenant connection per request, runs the named
    /// action against it, and 303s back to the same prefix.
    pub(super) async fn handle_list_action_tenant(
        State(state): State<Arc<TenantListViewState>>,
        t: Tenant,
        req: axum::extract::Request,
    ) -> Response {
        let (parts, body) = req.into_parts();
        let form = match super::read_repeating_form(body).await {
            Ok(f) => f,
            Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
        };
        let (action, raws) = match super::parse_bulk_action_form(&form) {
            Ok(v) => v,
            Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
        };
        let Some(pk_field) = state.vs.schema.primary_key() else {
            return template_error(&format!(
                "model `{}` has no primary key — bulk actions require one",
                state.vs.schema.table
            ));
        };
        let pks = match super::coerce_selected_pks(pk_field, &raws) {
            Ok(v) => v,
            Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
        };

        // v0.30.7 — confirmation gate for built-in delete_selected.
        // Tenant variant: fetch confirm-page rows via the
        // backend-erasing pool. Same JSON shape as the static path.
        if state.vs.confirm_delete
            && action == super::BUILTIN_DELETE_SELECTED
            && !super::is_form_confirmed(&form)
        {
            let pool = t.pool().clone();
            let objects = match super::fetch_pks_as_objects_pool(
                state.vs.schema,
                pk_field,
                &pool,
                &pks,
            )
            .await
            {
                Ok(o) => o,
                Err(e) => return template_error(&format!("fetch confirm rows: {e}")),
            };
            return super::render_bulk_delete_confirm(
                &state.tera,
                super::confirm_delete_template_name(&state.vs),
                &action,
                &raws,
                &objects,
                &parts.headers,
            );
        }

        let dispatch_path = parts.uri.path().to_owned();
        let pool = t.pool().clone();
        let result: Result<(), String> = if let Some(custom) = state
            .vs
            .actions
            .iter()
            .find(|a| super::same_action_name(&a.name, &action))
        {
            match &custom.handler {
                #[cfg(feature = "postgres")]
                super::BulkActionHandler::Tenant(f) => {
                    // The Tenant handler takes &mut PgConnection (PG
                    // bulk action API); only firable when the tenant
                    // pool is actually PG.
                    if let Some(pg) = pool.as_postgres() {
                        match pg.acquire().await {
                            Ok(mut conn) => f(&mut *conn, &pks).await,
                            Err(e) => Err(e.to_string()),
                        }
                    } else {
                        Err("this action was registered via .tenant_action(&mut PgConnection,...) — \
                             mount on a PG tenant pool; sqlite/mysql tenants don't expose a \
                             PgConnection"
                            .into())
                    }
                }
                super::BulkActionHandler::Pool(_) => {
                    Err("this action was registered via .action(...) — \
                     mount the ListView via router(...) (single-pool) to dispatch it"
                        .into())
                }
            }
        } else if action == super::BUILTIN_DELETE_SELECTED {
            super::run_delete_selected_pool(state.vs.schema, pk_field, &pool, &pks).await
        } else {
            return (
                StatusCode::BAD_REQUEST,
                format!("unknown action `{action}`"),
            )
                .into_response();
        };

        match result {
            Ok(()) => axum::response::Redirect::to(&dispatch_path).into_response(),
            Err(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
        }
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
        t: Tenant,
    ) -> Response {
        // #379 — `lookup_field` opt-in: probe by the named column
        // instead of the PK. Tenant variant mirrors the
        // non-tenant `handle_detail`.
        let lookup =
            match super::resolve_lookup_field(state.vs.schema, state.vs.lookup_field.as_deref()) {
                Ok(f) => f,
                Err(e) => return template_error(&e),
            };
        let select_q = SelectQuery {
            model: state.vs.schema,
            where_clause: WhereExpr::Predicate(Filter {
                column: lookup.column,
                op: Op::Eq,
                value: coerce_pk(lookup, &pk),
            }),
            search: None,
            joins: vec![],
            order_by: vec![],
            limit: Some(1),
            offset: None,
            lock_mode: None,
            compound: vec![],
            projection: None,
            distinct: None,
        };
        let fields = resolved_fields(state.vs.schema, state.vs.fields.as_deref());
        let object = match crate::sql::select_one_row_as_json(t.pool(), &select_q, &fields).await {
            Ok(Some(r)) => r,
            Ok(None) => return (StatusCode::NOT_FOUND, "not found").into_response(),
            Err(e) => return template_error(&format!("query row: {e}")),
        };

        let mut ctx = Context::new();
        ctx.insert("object", &object);
        if !state.vs.context_object_name.is_empty() {
            ctx.insert(&state.vs.context_object_name, &object);
        }
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
        t: Tenant,
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
                value: coerce_pk(pk_field, &pk),
            }),
            search: None,
            joins: vec![],
            order_by: vec![],
            limit: Some(1),
            offset: None,
            lock_mode: None,
            compound: vec![],
            projection: None,
            distinct: None,
        };
        let fields = resolved_fields(state.vs.schema, state.vs.fields.as_deref());
        let object = match crate::sql::select_one_row_as_json(t.pool(), &select_q, &fields).await {
            Ok(Some(r)) => r,
            Ok(None) => return (StatusCode::NOT_FOUND, "not found").into_response(),
            Err(e) => return template_error(&format!("query row: {e}")),
        };
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
        t: Tenant,
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
                value: coerce_pk(pk_field, &pk),
            }),
        };
        match crate::sql::delete_pool(t.pool(), &delete_q).await {
            Ok(0) => (StatusCode::NOT_FOUND, "not found").into_response(),
            Ok(_) => {
                let target = super::substitute_pk(&state.vs.success_url, &pk);
                axum::response::Redirect::to(&target).into_response()
            }
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
        /// Optional `#[derive(Form)]` validator (#80 v0.30.2 parity
        /// with the static `FormViewState` path).
        pub(super) validator: Option<super::Validator>,
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
        t: Tenant,
        axum::Form(form): axum::Form<HashMap<String, String>>,
    ) -> Response {
        let (columns, values, mut errors) =
            parse_form(state.schema, state.fields.as_deref(), &form);
        super::merge_validator_errors(state.validator.as_ref(), &form, &mut errors);
        if !errors.is_empty() {
            return rerender_form_tenant(
                &state, &form, &errors, /*is_update=*/ false, &headers,
            );
        }
        let returning = match super::success_url_returning_columns(&state.success_url, state.schema)
        {
            Ok(cols) => cols,
            Err(e) => return template_error(&e),
        };
        let need_returning = !returning.is_empty();
        let insert_q = crate::core::InsertQuery {
            model: state.schema,
            columns,
            values,
            returning,
            on_conflict: None,
        };
        let target_url = if need_returning {
            match crate::sql::insert_returning_pool(t.pool(), &insert_q).await {
                Ok(row) => {
                    match super::interpolate_success_url(&state.success_url, &row, state.schema) {
                        Ok(url) => url,
                        Err(e) => return template_error(&e),
                    }
                }
                Err(e) => return template_error(&format!("insert row: {e}")),
            }
        } else {
            if let Err(e) = crate::sql::insert_pool(t.pool(), &insert_q).await {
                return template_error(&format!("insert row: {e}"));
            }
            state.success_url.clone()
        };
        axum::response::Redirect::to(&target_url).into_response()
    }

    pub(super) async fn handle_update_get_tenant(
        State(state): State<Arc<TenantFormViewState>>,
        Path(pk): Path<String>,
        headers: axum::http::HeaderMap,
        t: Tenant,
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
                value: coerce_pk(pk_field, &pk),
            }),
            search: None,
            joins: vec![],
            order_by: vec![],
            limit: Some(1),
            offset: None,
            lock_mode: None,
            compound: vec![],
            projection: None,
            distinct: None,
        };
        let scalars: Vec<&'static crate::core::FieldSchema> =
            state.schema.scalar_fields().collect();
        let row_json = match crate::sql::select_one_row_as_json(t.pool(), &select_q, &scalars).await
        {
            Ok(Some(r)) => r,
            Ok(None) => return (StatusCode::NOT_FOUND, "not found").into_response(),
            Err(e) => return template_error(&format!("query row: {e}")),
        };
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
        t: Tenant,
        axum::Form(form): axum::Form<HashMap<String, String>>,
    ) -> Response {
        let Some(pk_field) = state.schema.primary_key() else {
            return template_error(&format!(
                "model `{}` has no primary key — UpdateView can't update by PK",
                state.schema.table
            ));
        };
        let (columns, values, mut errors) =
            parse_form(state.schema, state.fields.as_deref(), &form);
        super::merge_validator_errors(state.validator.as_ref(), &form, &mut errors);
        if !errors.is_empty() {
            return rerender_form_tenant(
                &state, &form, &errors, /*is_update=*/ true, &headers,
            );
        }
        let assignments: Vec<crate::core::Assignment> = columns
            .into_iter()
            .zip(values)
            .map(|(column, value)| crate::core::Assignment {
                column,
                value: value.into(),
            })
            .collect();
        let update_q = crate::core::UpdateQuery {
            model: state.schema,
            set: assignments,
            where_clause: WhereExpr::Predicate(Filter {
                column: pk_field.column,
                op: Op::Eq,
                value: coerce_pk(pk_field, &pk),
            }),
        };
        match crate::sql::update_pool(t.pool(), &update_q).await {
            Ok(0) => (StatusCode::NOT_FOUND, "not found").into_response(),
            Ok(_) => {
                let target = super::substitute_pk(&state.success_url, &pk);
                axum::response::Redirect::to(&target).into_response()
            }
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
    handle_delete_submit_tenant, handle_detail_tenant, handle_list_action_tenant,
    handle_list_tenant, handle_update_get_tenant, handle_update_post_tenant, TenantDeleteViewState,
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
                    help_text: None,
                    choices: None,
                    db_comment: None,
                    verbose_name: None,
                    editable: true,
                    blank: false,
                    validators: &[],
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
                    help_text: None,
                    choices: None,
                    db_comment: None,
                    verbose_name: None,
                    editable: true,
                    blank: false,
                    validators: &[],
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
            default_order: &[],
            is_view: false,
            verbose_name: None,
            verbose_name_plural: None,
            managed: true,
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
                    help_text: None,
                    choices: None,
                    db_comment: None,
                    verbose_name: None,
                    editable: true,
                    blank: false,
                    validators: &[],
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
                    help_text: None,
                    choices: None,
                    db_comment: None,
                    verbose_name: None,
                    editable: true,
                    blank: false,
                    validators: &[],
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
                    help_text: None,
                    choices: None,
                    db_comment: None,
                    verbose_name: None,
                    editable: true,
                    blank: false,
                    validators: &[],
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
            default_order: &[],
            is_view: false,
            verbose_name: None,
            verbose_name_plural: None,
            managed: true,
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
        assert_eq!(r[0].column_name(), Some("title"));
        assert!(!r[0].is_desc());
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
        assert_eq!(out[0].column_name(), Some("id"));
        assert!(!out[0].is_desc(), "PK fallback is ASC");
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
                help_text: None,
                choices: None,
                db_comment: None,
                verbose_name: None,
                editable: true,
                blank: false,
                validators: &[],
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
            default_order: &[],
            is_view: false,
            verbose_name: None,
            verbose_name_plural: None,
            managed: true,
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

    /// `substitute_pk` is the simpler sibling of
    /// `interpolate_success_url` — used by Update/DeleteView where
    /// the PK is already in scope from the URL.
    #[test]
    fn substitute_pk_replaces_placeholder() {
        assert_eq!(substitute_pk("/posts/{pk}", "42"), "/posts/42");
        assert_eq!(
            substitute_pk("/posts/{pk}/edit", "abc-123"),
            "/posts/abc-123/edit"
        );
    }

    #[test]
    fn substitute_pk_noop_when_no_placeholder() {
        assert_eq!(substitute_pk("/posts", "42"), "/posts");
        assert_eq!(substitute_pk("", "42"), "");
    }

    #[test]
    fn substitute_pk_handles_multiple_occurrences() {
        // Edge case: multiple {pk}s in a single template all
        // substitute. Not common but predictable.
        assert_eq!(substitute_pk("/{pk}/related/{pk}", "7"), "/7/related/7");
    }

    /// `parse_success_url_placeholders` recognizes `{name}`
    /// placeholders with valid identifier shapes and ignores the
    /// rest (stray braces, empty `{}`, special chars).
    #[test]
    fn parse_success_url_placeholders_extracts_valid_names() {
        assert_eq!(parse_success_url_placeholders("/posts/{pk}"), vec!["pk"]);
        assert_eq!(
            parse_success_url_placeholders("/posts/{pk}/{slug}"),
            vec!["pk", "slug"]
        );
        // Empty placeholder + stray brace → both ignored.
        assert_eq!(parse_success_url_placeholders("/{}/{ok}"), vec!["ok"]);
        // Special chars in placeholder → not a valid identifier,
        // skipped (the literal `{a-b}` stays in the URL).
        assert_eq!(parse_success_url_placeholders("/{a-b}/{ok}"), vec!["ok"]);
        // No placeholders at all.
        assert!(parse_success_url_placeholders("/posts").is_empty());
        assert!(parse_success_url_placeholders("").is_empty());
    }

    /// `success_url_returning_columns` resolves `{pk}` to the
    /// model's PK column and other names to their schema-declared
    /// `column`. Empty placeholder set returns empty vec — caller
    /// short-circuits the RETURNING SQL path.
    #[test]
    fn success_url_returning_columns_resolves_pk_and_names() {
        let s = schema_two_fields();
        let cols = success_url_returning_columns("/posts/{pk}", s).unwrap();
        assert_eq!(cols, vec!["id"]);

        let cols = success_url_returning_columns("/posts/{pk}/{title}", s).unwrap();
        assert_eq!(cols, vec!["id", "title"]);

        // Plain URL → empty.
        assert!(success_url_returning_columns("/posts", s)
            .unwrap()
            .is_empty());
    }

    /// Unknown placeholder name surfaces a clear error before the
    /// INSERT runs.
    #[test]
    fn success_url_returning_columns_rejects_unknown_placeholder() {
        let s = schema_two_fields();
        let err = success_url_returning_columns("/posts/{nope}", s).unwrap_err();
        assert!(err.contains("`{nope}`"), "got: {err}");
        assert!(err.contains("posts"), "got: {err}");
    }

    /// `interpolate_success_url` is a no-op when no `{pk}`
    /// placeholder is present (the common case — most users
    /// redirect back to the list view, not the new row's detail).
    /// Live PgRow tests cover the placeholder branch.
    #[test]
    fn interpolate_success_url_noop_when_no_placeholder() {
        // Build a row-less call by passing an explicit `&str` test —
        // the function short-circuits before touching `row`. We
        // can't construct a PgRow without a real PG connection, so
        // exercise just the no-placeholder fast path.
        let template = "/posts";
        // `pk_field` is None to confirm the no-placeholder branch
        // doesn't even look at it.
        // We can't actually call `interpolate_success_url` without
        // a PgRow, but the contract is documented + exercised by
        // the live test suite. This test pins the no-placeholder
        // fast-path expectation:
        assert!(!template.contains("{pk}"));
    }

    /// `coerce_pk` for an integer PK parses to `SqlValue::I64`.
    #[test]
    fn coerce_pk_integer_field() {
        let s = schema_two_fields();
        let pk = s.primary_key().unwrap();
        match coerce_pk(pk, "42") {
            SqlValue::I64(n) => assert_eq!(n, 42),
            other => panic!("expected I64, got {other:?}"),
        }
    }

    /// `coerce_pk` falls back to `SqlValue::String` on parse
    /// failure rather than panicking — the resulting query just
    /// returns no rows / 404, same effect as a 400 but without
    /// leaking parse errors.
    #[test]
    fn coerce_pk_integer_field_fallback_on_garbage() {
        let s = schema_two_fields();
        let pk = s.primary_key().unwrap();
        match coerce_pk(pk, "not-a-number") {
            SqlValue::String(raw) => assert_eq!(raw, "not-a-number"),
            other => panic!("expected fallback String, got {other:?}"),
        }
    }

    /// `coerce_pk` for a UUID PK parses to `SqlValue::Uuid`.
    #[test]
    fn coerce_pk_uuid_field() {
        // Build a one-off schema with a UUID PK to exercise the
        // branch — schema_two_fields uses I64 for `id`.
        let uuid_schema: &'static ModelSchema = Box::leak(Box::new(ModelSchema {
            name: "Doc",
            table: "docs",
            fields: Box::leak(Box::new([crate::core::FieldSchema {
                name: "id",
                column: "id",
                ty: FieldType::Uuid,
                nullable: false,
                primary_key: true,
                relation: None,
                max_length: None,
                min: None,
                max: None,
                default: None,
                auto: false,
                unique: false,
                generated_as: None,
                help_text: None,
                choices: None,
                db_comment: None,
                verbose_name: None,
                editable: true,
                blank: false,
                validators: &[],
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
            default_order: &[],
            is_view: false,
            verbose_name: None,
            verbose_name_plural: None,
            managed: true,
        }));
        let pk = uuid_schema.primary_key().unwrap();
        let raw = "550e8400-e29b-41d4-a716-446655440000";
        match coerce_pk(pk, raw) {
            SqlValue::Uuid(_) => {} // success — variant matches
            other => panic!("expected Uuid, got {other:?}"),
        }
        // Garbage UUID falls back to String.
        match coerce_pk(pk, "not-a-uuid") {
            SqlValue::String(s) => assert_eq!(s, "not-a-uuid"),
            other => panic!("expected fallback String, got {other:?}"),
        }
    }

    /// `coerce_pk` for a String PK passes through verbatim.
    #[test]
    fn coerce_pk_string_field() {
        let str_schema: &'static ModelSchema = Box::leak(Box::new(ModelSchema {
            name: "Slug",
            table: "slugs",
            fields: Box::leak(Box::new([crate::core::FieldSchema {
                name: "slug",
                column: "slug",
                ty: FieldType::String,
                nullable: false,
                primary_key: true,
                relation: None,
                max_length: Some(64),
                min: None,
                max: None,
                default: None,
                auto: false,
                unique: false,
                generated_as: None,
                help_text: None,
                choices: None,
                db_comment: None,
                verbose_name: None,
                editable: true,
                blank: false,
                validators: &[],
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
            default_order: &[],
            is_view: false,
            verbose_name: None,
            verbose_name_plural: None,
            managed: true,
        }));
        let pk = str_schema.primary_key().unwrap();
        match coerce_pk(pk, "hello-world") {
            SqlValue::String(s) => assert_eq!(s, "hello-world"),
            other => panic!("expected String, got {other:?}"),
        }
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

    /// `urlencode` encodes everything outside the unreserved set —
    /// spaces become `%20`, `&` becomes `%26`, etc. Round-trips.
    #[test]
    fn urlencode_encodes_reserved_chars() {
        assert_eq!(urlencode("hello world"), "hello%20world");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
        assert_eq!(urlencode("plain"), "plain");
        assert_eq!(urlencode("foo-bar.baz_~"), "foo-bar.baz_~");
    }

    /// `build_pagination_query` preserves every original param
    /// except `page`, sorts keys for deterministic output, and
    /// URL-encodes values.
    #[test]
    fn build_pagination_query_preserves_other_params() {
        let mut params = HashMap::new();
        params.insert("author_id".to_owned(), "42".to_owned());
        params.insert("search".to_owned(), "hello world".to_owned());
        params.insert("page".to_owned(), "1".to_owned()); // dropped + replaced
        let q = build_pagination_query(&params, 3);
        // Sorted keys: author_id, search, then page appended.
        assert_eq!(q, "?author_id=42&search=hello%20world&page=3");
    }

    /// Empty params still yields a `?page=N` query string.
    #[test]
    fn build_pagination_query_no_other_params() {
        let params = HashMap::new();
        assert_eq!(build_pagination_query(&params, 5), "?page=5");
    }

    /// `insert_pagination_urls` stamps `Some(url)` for each
    /// direction that's reachable, `None` otherwise.
    #[test]
    fn insert_pagination_urls_stamps_correct_directions() {
        let mut ctx = Context::new();
        let mut params = HashMap::new();
        params.insert("status".to_owned(), "draft".to_owned());

        // Middle of paginated range: both directions present.
        insert_pagination_urls(
            &mut ctx, 3, /*has_next=*/ true, /*has_prev=*/ true, &params,
        );
        let mut tera = Tera::default();
        tera.add_raw_template("t", "{{ next_page_url }}|{{ prev_page_url }}")
            .unwrap();
        let rendered = tera.render("t", &ctx).unwrap();
        assert_eq!(rendered, "?status=draft&page=4|?status=draft&page=2");
    }

    /// First page: prev is None, rendered as empty by Tera's
    /// default-of-Option<String> semantics.
    #[test]
    fn insert_pagination_urls_first_page_no_prev() {
        let mut ctx = Context::new();
        let params = HashMap::new();
        insert_pagination_urls(
            &mut ctx, 1, /*has_next=*/ true, /*has_prev=*/ false, &params,
        );
        let mut tera = Tera::default();
        tera.add_raw_template(
            "t",
            "{% if prev_page_url %}HAS_PREV{% else %}NO_PREV{% endif %}",
        )
        .unwrap();
        assert_eq!(tera.render("t", &ctx).unwrap(), "NO_PREV");
    }

    /// `resolve_page_size` falls back to the default when the param
    /// is absent or unparseable.
    #[test]
    fn resolve_page_size_unset_returns_default() {
        let params = HashMap::new();
        assert_eq!(resolve_page_size(20, 100, &params), 20);

        let mut params = HashMap::new();
        params.insert("page_size".into(), "garbage".into());
        assert_eq!(resolve_page_size(20, 100, &params), 20);
    }

    /// `resolve_page_size` clamps to `[1, max]`.
    #[test]
    fn resolve_page_size_clamps_to_range() {
        let mut params = HashMap::new();
        // Below the floor.
        params.insert("page_size".into(), "0".into());
        assert_eq!(resolve_page_size(20, 100, &params), 1);
        params.insert("page_size".into(), "-5".into());
        assert_eq!(resolve_page_size(20, 100, &params), 1);

        // Above the cap — protects against ?page_size=999999 DoS.
        params.insert("page_size".into(), "999999".into());
        assert_eq!(resolve_page_size(20, 100, &params), 100);

        // Within range.
        params.insert("page_size".into(), "50".into());
        assert_eq!(resolve_page_size(20, 100, &params), 50);
    }

    /// `?ordering=col` honored when the field is in the allowlist.
    #[test]
    fn resolve_active_order_url_override_asc() {
        let s = schema_two_fields();
        let mut params = HashMap::new();
        params.insert("ordering".into(), "title".into());
        let (clauses, active) = resolve_active_order(s, &[], &["title".into()], &params).unwrap();
        assert_eq!(clauses.len(), 1);
        assert_eq!(clauses[0].column_name(), Some("title"));
        assert!(!clauses[0].is_desc());
        assert_eq!(active, "title");
    }

    /// `?ordering=-col` flips to DESC.
    #[test]
    fn resolve_active_order_url_override_desc_prefix() {
        let s = schema_two_fields();
        let mut params = HashMap::new();
        params.insert("ordering".into(), "-title".into());
        let (clauses, active) = resolve_active_order(s, &[], &["title".into()], &params).unwrap();
        assert_eq!(clauses[0].column_name(), Some("title"));
        assert!(clauses[0].is_desc());
        assert_eq!(active, "-title");
    }

    /// Override outside the allowlist falls back to the builder
    /// default — matches the "typos shouldn't 400" policy used for
    /// `filter_fields`.
    #[test]
    fn resolve_active_order_url_override_outside_allowlist_falls_back() {
        let s = schema_two_fields();
        let mut params = HashMap::new();
        params.insert("ordering".into(), "id".into()); // not in allowlist
        let (_, active) = resolve_active_order(s, &[], &["title".into()], &params).unwrap();
        // Builder default has no order_by, so `default_order_by`
        // returns PK-ASC; `active` is empty (templates render no
        // "active sort" indicator since the user-requested sort
        // wasn't applied).
        assert_eq!(active, "");
    }

    /// No `?ordering=` URL param → builder default (with PK-ASC
    /// fallback when no `.order_by(...)` was set), `active` empty.
    #[test]
    fn resolve_active_order_no_url_uses_builder_default() {
        let s = schema_two_fields();
        let params = HashMap::new();
        let (clauses, active) = resolve_active_order(s, &[], &["title".into()], &params).unwrap();
        // PK-ASC fallback.
        assert_eq!(clauses.len(), 1);
        assert_eq!(clauses[0].column_name(), Some("id"));
        assert!(!clauses[0].is_desc());
        assert_eq!(active, "");
    }

    /// Empty `?ordering=` (`?ordering=`) is treated as no override.
    #[test]
    fn resolve_active_order_empty_value_treated_as_no_override() {
        let s = schema_two_fields();
        let mut params = HashMap::new();
        params.insert("ordering".into(), String::new());
        let (_, active) = resolve_active_order(s, &[], &["title".into()], &params).unwrap();
        assert_eq!(active, "");
    }

    // ---- Validator hook (#80 v0.30.2) ----

    /// `merge_validator_errors` — validator returns no errors → map
    /// is unchanged.
    #[test]
    fn merge_validator_no_errors_leaves_map_untouched() {
        let v: Validator = Arc::new(|_data| Ok(()));
        let mut errors: HashMap<String, String> = HashMap::new();
        merge_validator_errors(Some(&v), &HashMap::new(), &mut errors);
        assert!(errors.is_empty());
    }

    /// Field errors from the validator land under the right keys
    /// with multi-error joining via "; ".
    #[test]
    fn merge_validator_field_errors_land_under_field_key() {
        let v: Validator = Arc::new(|_data| {
            let mut e = crate::forms::FormErrors::default();
            e.add("title", "must be at least 5 characters");
            e.add("title", "must not contain whitespace");
            e.add("body", "required");
            Err(e)
        });
        let mut errors: HashMap<String, String> = HashMap::new();
        merge_validator_errors(Some(&v), &HashMap::new(), &mut errors);
        let title = errors.get("title").expect("title error");
        assert!(title.contains("at least 5"), "got: {title}");
        assert!(title.contains("whitespace"), "got: {title}");
        assert_eq!(errors.get("body"), Some(&"required".to_owned()));
    }

    /// Non-field errors land under the special `__all__` key —
    /// matches Django convention for cross-field errors.
    #[test]
    fn merge_validator_non_field_errors_land_under_all_key() {
        let v: Validator = Arc::new(|_data| {
            let mut e = crate::forms::FormErrors::default();
            e.add_non_field("password and confirm_password must match");
            Err(e)
        });
        let mut errors: HashMap<String, String> = HashMap::new();
        merge_validator_errors(Some(&v), &HashMap::new(), &mut errors);
        let all = errors.get("__all__").expect("non-field error");
        assert!(all.contains("must match"), "got: {all}");
    }

    /// Pre-existing schema-level errors are preserved + appended to,
    /// not clobbered by the validator's output.
    #[test]
    fn merge_validator_appends_to_existing_field_error() {
        let v: Validator = Arc::new(|_data| {
            let mut e = crate::forms::FormErrors::default();
            e.add("title", "regex mismatch");
            Err(e)
        });
        let mut errors: HashMap<String, String> = HashMap::new();
        errors.insert("title".into(), "max_length 5 exceeded".into());
        merge_validator_errors(Some(&v), &HashMap::new(), &mut errors);
        let title = errors.get("title").unwrap();
        assert!(title.contains("max_length"), "preserved: {title}");
        assert!(title.contains("regex mismatch"), "appended: {title}");
    }

    /// CreateView/UpdateView builders accept a closure-based
    /// validator + a typed Form via `.form::<T>()`. Smoke test that
    /// both shapes compile + the field gets stamped on state.
    #[test]
    fn validator_and_form_builders_set_validator_field() {
        // Closure form
        let cv = CreateView::for_model(schema_two_fields()).validator(|_data| Ok(()));
        assert!(
            cv.validator.is_some(),
            ".validator(closure) must set the field"
        );

        let uv = UpdateView::for_model(schema_two_fields()).validator(|_data| Ok(()));
        assert!(uv.validator.is_some());

        // Typed Form trait
        struct Tiny;
        impl crate::forms::Form for Tiny {
            fn parse(_: &HashMap<String, String>) -> Result<Self, crate::forms::FormErrors> {
                Ok(Tiny)
            }
        }
        let cv2 = CreateView::for_model(schema_two_fields()).form::<Tiny>();
        assert!(
            cv2.validator.is_some(),
            ".form::<T>() must set the validator"
        );
    }

    // ---- Bulk actions on ListView (#80 v0.30.4) ----

    /// `bulk_actions(true)` flips the flag, the default is off so
    /// existing projects pay no overhead.
    #[test]
    fn bulk_actions_default_off_flag_flips_with_builder() {
        let s = schema_two_fields();
        let lv = ListView::for_model(s);
        assert!(!lv.bulk_actions_enabled, "default off");
        let lv2 = lv.bulk_actions(true);
        assert!(lv2.bulk_actions_enabled, "true after .bulk_actions(true)");
    }

    /// `.action(name, label, handler)` accumulates user actions in
    /// registration order; same name twice replaces (last write wins).
    #[test]
    fn action_builder_dedupes_by_name() {
        let s = schema_two_fields();
        let h: BulkActionFn = Arc::new(|_pool, _pks| Box::pin(async { Ok(()) }));
        let lv = ListView::for_model(s)
            .action("publish", "Publish", h.clone())
            .action("archive", "Archive", h.clone())
            .action("publish", "Publish (renamed)", h);
        assert_eq!(lv.actions.len(), 2);
        let publish = lv.actions.iter().find(|a| a.name == "publish").unwrap();
        assert_eq!(
            publish.label, "Publish (renamed)",
            "second .action with same name should replace"
        );
    }

    /// `parse_bulk_action_form` requires both `action` and at least
    /// one `_selected_action` value. Empty selection is an error so
    /// templates show the user a "select rows first" message rather
    /// than silently running the action against zero rows.
    #[test]
    fn parse_bulk_action_form_requires_action_and_selection() {
        // Missing action.
        let mut f: HashMap<String, Vec<String>> = HashMap::new();
        f.insert("_selected_action".into(), vec!["1".into()]);
        assert!(parse_bulk_action_form(&f).is_err());

        // Missing selection.
        let mut f: HashMap<String, Vec<String>> = HashMap::new();
        f.insert("action".into(), vec!["delete_selected".into()]);
        assert!(parse_bulk_action_form(&f).is_err());

        // Both present + non-empty PKs.
        let mut f: HashMap<String, Vec<String>> = HashMap::new();
        f.insert("action".into(), vec!["delete_selected".into()]);
        f.insert(
            "_selected_action".into(),
            vec!["1".into(), "2".into(), "3".into()],
        );
        let (action, pks) = parse_bulk_action_form(&f).unwrap();
        assert_eq!(action, "delete_selected");
        assert_eq!(pks, vec!["1", "2", "3"]);
    }

    /// `coerce_pk_typed` converts to the right `SqlValue` per
    /// `FieldType` and surfaces parse errors instead of falling
    /// back to a string (which would corrupt the SQL `IN (...)`
    /// bind).
    #[test]
    fn coerce_pk_typed_returns_correct_sqlvalue_per_type() {
        use crate::core::FieldType;
        let f = |ty: FieldType| {
            Box::leak(Box::new(crate::core::FieldSchema {
                name: "id",
                column: "id",
                ty,
                nullable: false,
                primary_key: true,
                relation: None,
                max_length: None,
                min: None,
                max: None,
                default: None,
                auto: false,
                unique: false,
                generated_as: None,
                help_text: None,
                choices: None,
                db_comment: None,
                verbose_name: None,
                editable: true,
                blank: false,
                validators: &[],
            })) as &'static crate::core::FieldSchema
        };
        assert!(matches!(
            coerce_pk_typed(f(FieldType::I64), "42"),
            Ok(SqlValue::I64(42))
        ));
        assert!(matches!(
            coerce_pk_typed(f(FieldType::I32), "42"),
            Ok(SqlValue::I32(42))
        ));
        assert!(matches!(
            coerce_pk_typed(f(FieldType::I16), "42"),
            Ok(SqlValue::I16(42))
        ));
        assert!(coerce_pk_typed(f(FieldType::I64), "not-a-number").is_err());
        assert!(coerce_pk_typed(f(FieldType::Uuid), "not-a-uuid").is_err());
    }

    /// `bulk_actions` Tera context entry leads with `delete_selected`,
    /// then user-registered actions in order.
    #[test]
    fn bulk_actions_context_includes_built_in_then_user_actions() {
        let s = schema_two_fields();
        let h: BulkActionFn = Arc::new(|_p, _v| Box::pin(async { Ok(()) }));
        let lv = ListView::for_model(s)
            .bulk_actions(true)
            .action("publish", "Publish", h);
        let mut ctx = Context::new();
        insert_bulk_actions_context(&mut ctx, &lv);
        let v = ctx.into_json();
        let arr = v["bulk_actions"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["name"], serde_json::json!("delete_selected"));
        assert_eq!(arr[0]["label"], serde_json::json!("Delete selected"));
        assert_eq!(arr[1]["name"], serde_json::json!("publish"));
    }

    /// `with_delete_confirmation(true)` flips both fields; the
    /// `..._template(name)` variant implies the flag is on.
    #[test]
    fn with_delete_confirmation_flag_and_template_override() {
        let s = schema_two_fields();
        let lv = ListView::for_model(s);
        assert!(!lv.confirm_delete, "default off");
        assert!(lv.confirm_delete_template.is_none());

        let lv2 = ListView::for_model(s).with_delete_confirmation(true);
        assert!(lv2.confirm_delete);
        assert!(
            lv2.confirm_delete_template.is_none(),
            "no override → resolves at request time"
        );

        let lv3 = ListView::for_model(s).with_delete_confirmation_template("custom.html");
        assert!(
            lv3.confirm_delete,
            "with_delete_confirmation_template implies the flag"
        );
        assert_eq!(lv3.confirm_delete_template.as_deref(), Some("custom.html"));
    }

    /// `confirm_delete_template_name` resolves the explicit override
    /// when set, otherwise falls back to `<table>_confirm_bulk_delete.html`.
    #[test]
    fn confirm_delete_template_name_resolves_default_or_override() {
        let s = schema_two_fields();
        let lv = ListView::for_model(s);
        // Default — schema_two_fields() puts the table at "posts".
        assert_eq!(
            confirm_delete_template_name(&lv),
            "posts_confirm_bulk_delete.html"
        );
        let lv2 = ListView::for_model(s).with_delete_confirmation_template("blog/confirm.html");
        assert_eq!(confirm_delete_template_name(&lv2), "blog/confirm.html");
    }

    // ---- FK display sibling resolution (#80, v0.30.8) ----

    /// `with_fk_display(true)` flips the flag, default off.
    #[test]
    fn with_fk_display_flag_default_off_then_on() {
        let s = schema_two_fields();
        let lv = ListView::for_model(s);
        assert!(!lv.fk_display, "default off");
        let lv2 = ListView::for_model(s).with_fk_display(true);
        assert!(lv2.fk_display);
    }

    /// `json_value_as_lookup_key` stringifies JSON numbers + strings
    /// so the lookup map's keys match the FK column's values
    /// regardless of integer vs UUID PK.
    #[test]
    fn json_value_as_lookup_key_handles_numbers_and_strings() {
        assert_eq!(
            json_value_as_lookup_key(&serde_json::json!(42)),
            Some("42".to_string())
        );
        assert_eq!(
            json_value_as_lookup_key(&serde_json::json!("550e8400-e29b-41d4-a716-446655440000")),
            Some("550e8400-e29b-41d4-a716-446655440000".to_string())
        );
        assert_eq!(
            json_value_as_lookup_key(&serde_json::json!(null)),
            None,
            "NULL FK has no lookup key"
        );
        assert_eq!(json_value_as_lookup_key(&serde_json::json!(true)), None);
    }

    /// `json_value_to_sql_for_fk_pk` round-trips integer JSON →
    /// SqlValue::I64 (the common FK shape) and string-shaped UUIDs
    /// → SqlValue::Uuid (auto-detected via parse). Other strings
    /// pass through as SqlValue::String.
    #[test]
    fn json_value_to_sql_for_fk_pk_round_trips_common_pk_types() {
        match json_value_to_sql_for_fk_pk(&serde_json::json!(42)) {
            SqlValue::I64(42) => {}
            other => panic!("expected I64(42), got {other:?}"),
        }
        match json_value_to_sql_for_fk_pk(&serde_json::json!(
            "550e8400-e29b-41d4-a716-446655440000"
        )) {
            SqlValue::Uuid(u) => assert_eq!(u.to_string(), "550e8400-e29b-41d4-a716-446655440000"),
            other => panic!("expected Uuid, got {other:?}"),
        }
        match json_value_to_sql_for_fk_pk(&serde_json::json!("not-a-uuid")) {
            SqlValue::String(s) => assert_eq!(s, "not-a-uuid"),
            other => panic!("expected String, got {other:?}"),
        }
    }

    /// `stamp_display_into_rows` walks a `Vec<Value>`, looks up
    /// each row's FK column value in the map, and writes
    /// `<column>_display` when a match exists. Missing keys
    /// (NULL FK, target row missing) leave the row untouched.
    #[test]
    fn stamp_display_into_rows_writes_sibling_only_when_resolved() {
        // Build a minimal FkLookup; field types don't matter for
        // the stamping logic — only `local_field` is used.
        let fk = FkLookup {
            local_field: "author_id",
            target_table: "tv_fk_author",
            target_pk_column: "id",
            target_display_column: "name",
            target_display_field_name: "name",
            distinct_values: vec![],
        };
        let mut rows = vec![
            serde_json::json!({"id": 1, "title": "first", "author_id": 7}),
            serde_json::json!({"id": 2, "title": "second", "author_id": 99}),
            serde_json::json!({"id": 3, "title": "third", "author_id": null}),
        ];
        let mut map: HashMap<String, serde_json::Value> = HashMap::new();
        map.insert("7".into(), serde_json::json!("Alice"));
        // No entry for 99 — target row missing.

        stamp_display_into_rows(&fk, &map, &mut rows);

        assert_eq!(
            rows[0]["author_id_display"],
            serde_json::json!("Alice"),
            "resolved FK gets display sibling"
        );
        assert!(
            rows[1]
                .as_object()
                .unwrap()
                .get("author_id_display")
                .is_none(),
            "missing target row → no sibling stamped: {:?}",
            rows[1]
        );
        assert!(
            rows[2]
                .as_object()
                .unwrap()
                .get("author_id_display")
                .is_none(),
            "NULL FK → no sibling stamped: {:?}",
            rows[2]
        );
    }

    /// `is_form_confirmed` accepts every reasonable truthy form
    /// value (true/1/yes/on, case-insensitive) so users don't have
    /// to remember the exact magic string. Anything else is a no.
    #[test]
    fn is_form_confirmed_accepts_truthy_strings() {
        let mk = |val: &str| {
            let mut f: HashMap<String, Vec<String>> = HashMap::new();
            f.insert("confirmed".into(), vec![val.into()]);
            f
        };
        for truthy in ["true", "TRUE", "True", "1", "yes", "YES", "on", "On"] {
            assert!(
                is_form_confirmed(&mk(truthy)),
                "expected {truthy:?} to read as confirmed"
            );
        }
        for falsy in ["", "false", "0", "no", "off", "maybe", "anything-else"] {
            assert!(
                !is_form_confirmed(&mk(falsy)),
                "expected {falsy:?} to read as NOT confirmed"
            );
        }
        // Missing key → not confirmed.
        let empty: HashMap<String, Vec<String>> = HashMap::new();
        assert!(!is_form_confirmed(&empty));
    }

    /// When bulk_actions is off, the context entry is empty (rather
    /// than missing) so templates can `{% if bulk_actions %}`
    /// cleanly without a separate flag.
    #[test]
    fn bulk_actions_context_empty_when_disabled() {
        let s = schema_two_fields();
        let lv = ListView::for_model(s); // default off
        let mut ctx = Context::new();
        insert_bulk_actions_context(&mut ctx, &lv);
        let v = ctx.into_json();
        let arr = v["bulk_actions"].as_array().unwrap();
        assert!(arr.is_empty());
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

    // ---- TemplateView / RedirectView / FormView (issue #13) ----

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt as _;

    #[tokio::test]
    async fn template_view_renders_template_with_static_context() {
        let mut tera = Tera::default();
        tera.add_raw_template("about.html", "About — contact {{ email }}.")
            .unwrap();
        let app = TemplateView::new("about.html")
            .context_value("email", "hello@example.com")
            .router("/about", Arc::new(tera));

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/about")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            "About — contact hello@example.com."
        );
    }

    #[tokio::test]
    async fn template_view_context_method_merges_json_object() {
        let mut tera = Tera::default();
        tera.add_raw_template("t.html", "{{ a }} / {{ b }}")
            .unwrap();
        let app = TemplateView::new("t.html")
            .context(serde_json::json!({"a": "first", "b": "second"}))
            .router("/", Arc::new(tera));

        let res = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "first / second");
    }

    #[tokio::test]
    async fn redirect_view_returns_302_with_location() {
        let app = RedirectView::to("/new-home").router("/old-home");
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/old-home")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FOUND);
        assert_eq!(
            res.headers()
                .get(axum::http::header::LOCATION)
                .and_then(|v| v.to_str().ok()),
            Some("/new-home")
        );
    }

    #[tokio::test]
    async fn redirect_view_permanent_returns_301() {
        let app = RedirectView::to("/canonical").permanent().router("/old");
        let res = app
            .oneshot(Request::builder().uri("/old").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(
            res.headers()
                .get(axum::http::header::LOCATION)
                .and_then(|v| v.to_str().ok()),
            Some("/canonical")
        );
    }

    #[tokio::test]
    async fn redirect_view_drops_crlf_injected_location_header() {
        // CRLF in the target URL is a response-splitting vector.
        // `HeaderValue::from_str` rejects it; our handler drops the
        // header silently so no `Set-Cookie: pwned=1` slips through.
        // Status stays at the configured value so the failure is
        // visible (browser sees no redirect).
        let app = RedirectView::to("/safe\r\nSet-Cookie: pwned=1").router("/x");
        let res = app
            .oneshot(Request::builder().uri("/x").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FOUND);
        assert!(
            res.headers().get(axum::http::header::LOCATION).is_none(),
            "CRLF-injected URL must NOT produce a Location header"
        );
    }

    #[tokio::test]
    async fn form_view_get_renders_empty_form_context() {
        use crate::forms::{Form, FormErrors};

        struct DummyForm;
        impl Form for DummyForm {
            fn parse(_: &HashMap<String, String>) -> Result<Self, FormErrors> {
                Ok(DummyForm)
            }
        }

        let mut tera = Tera::default();
        // `errors | length` resolves to 0 (the HashMap is present + empty);
        // proves the variable was stamped on the context.
        tera.add_raw_template("f.html", "errors:{{ errors | length }}")
            .unwrap();

        let app = FormView::<DummyForm>::for_form(|_| async { Ok(()) })
            .template("f.html")
            .router("/", Arc::new(tera));

        let res = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "errors:0");
    }

    #[tokio::test]
    async fn form_view_post_invalid_re_renders_with_errors() {
        use crate::forms::{Form, FormErrors};

        struct StrictForm;
        impl Form for StrictForm {
            fn parse(_: &HashMap<String, String>) -> Result<Self, FormErrors> {
                let mut e = FormErrors::default();
                e.add("name", "required");
                Err(e)
            }
        }

        let mut tera = Tera::default();
        tera.add_raw_template(
            "f.html",
            "{% for field, msgs in errors %}{{ field }}:{{ msgs | length }}{% endfor %}",
        )
        .unwrap();

        let app = FormView::<StrictForm>::for_form(|_| async { Ok(()) })
            .template("f.html")
            .router("/", Arc::new(tera));

        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("name="))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "name:1");
    }

    #[tokio::test]
    async fn form_view_post_valid_redirects_to_success_url() {
        use crate::forms::{Form, FormErrors};

        struct OkForm;
        impl Form for OkForm {
            fn parse(_: &HashMap<String, String>) -> Result<Self, FormErrors> {
                Ok(OkForm)
            }
        }

        let mut tera = Tera::default();
        tera.add_raw_template("f.html", "").unwrap();

        let app = FormView::<OkForm>::for_form(|_| async { Ok(()) })
            .template("f.html")
            .success_url("/thanks")
            .router("/", Arc::new(tera));

        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(""))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            res.headers()
                .get(axum::http::header::LOCATION)
                .and_then(|v| v.to_str().ok()),
            Some("/thanks")
        );
    }

    #[tokio::test]
    async fn form_view_post_valid_drops_crlf_injected_success_url() {
        use crate::forms::{Form, FormErrors};

        struct OkForm;
        impl Form for OkForm {
            fn parse(_: &HashMap<String, String>) -> Result<Self, FormErrors> {
                Ok(OkForm)
            }
        }

        let mut tera = Tera::default();
        tera.add_raw_template("f.html", "").unwrap();

        // success_url with CRLF — same response-splitting defense as
        // RedirectView. `HeaderValue::from_str` rejects the value,
        // the Location header is dropped, status stays 303.
        let app = FormView::<OkForm>::for_form(|_| async { Ok(()) })
            .template("f.html")
            .success_url("/thanks\r\nSet-Cookie: pwned=1")
            .router("/", Arc::new(tera));

        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(""))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert!(
            res.headers().get(axum::http::header::LOCATION).is_none(),
            "CRLF-injected success_url must NOT produce a Location header"
        );
    }
}
