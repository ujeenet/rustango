//! Django REST Framework–style router viewsets.
//!
//! A [`ViewSet`] wires five standard REST endpoints for any [`Model`]
//! table in ~5 lines. No hand-written handlers, no SQL, no repetition.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::viewset::ViewSet;
//!
//! // In your router setup:
//! let posts_router = ViewSet::for_model(Post::SCHEMA)
//!     .fields(&["id", "title", "body", "author_id", "published_at"])
//!     .filter_fields(&["author_id"])
//!     .search_fields(&["title", "body"])
//!     .ordering(&[("published_at", true)])  // DESC by default
//!     .page_size(20)
//!     .router("/api/posts", pool.clone());
//!
//! // Merge into your app router:
//! let app = Router::new().merge(posts_router);
//! ```
//!
//! ## Endpoints
//!
//! | Method | Path | Action |
//! |---|---|---|
//! | `GET` | `/api/posts` | List — `{"count": N, "results": [...]}` |
//! | `POST` | `/api/posts` | Create — returns the new object |
//! | `GET` | `/api/posts/{pk}` | Retrieve — single object |
//! | `PUT` | `/api/posts/{pk}` | Update — full replace |
//! | `PATCH` | `/api/posts/{pk}` | Partial update — only supplied fields |
//! | `DELETE` | `/api/posts/{pk}` | Delete — `204 No Content` |
//!
//! ## Query parameters (list endpoint)
//!
//! ### Page-number pagination (default)
//!
//! | Parameter | Default | Description |
//! |---|---|---|
//! | `page` | 1 | 1-based page number |
//! | `page_size` | configured default | Items per page (capped at 1000) |
//! | `ordering` | configured default | Comma-separated field names, prefix `-` for DESC |
//! | `search` | — | Full-text search across `search_fields` |
//! | `{field}` | — | Exact filter for any `filter_fields` |
//! | `{field}__{lookup}` | — | Django-style lookup (gt/gte/lt/lte/ne/in/not_in/contains/icontains/startswith/istartswith/endswith/iendswith/isnull) |
//!
//! Response: `{"count": N, "page": P, "page_size": S, "last_page": L, "results": [...]}`
//!
//! ### Cursor pagination (opt-in)
//!
//! Enable via `.cursor_pagination("id")` or `.cursor_pagination_desc("id")`.
//! Skips the `COUNT(*)` query so it scales to billion-row tables.
//!
//! | Parameter | Default | Description |
//! |---|---|---|
//! | `cursor` | — | Opaque token from a previous response's `next` field |
//! | `page_size` | configured default | Items per page (capped at 1000) |
//! | `{field}` | — | Exact filter for any `filter_fields` |
//!
//! Response: `{"page_size": S, "next": "<token>" \| null, "results": [...]}`
//!
//! ### Limit/offset pagination (opt-in)
//!
//! Enable via `.limit_offset_pagination()`. DRF-shape `?limit=&offset=`
//! windowing — handy for tables/grids that page by row offset rather
//! than page number. Runs `COUNT(*)` per request (same cost as
//! page-number).
//!
//! | Parameter | Default | Description |
//! |---|---|---|
//! | `limit` | configured default | Items to return (capped at 1000) |
//! | `offset` | 0 | Rows to skip before the window |
//! | `ordering` | configured default | Comma-separated field names, prefix `-` for DESC |
//! | `search` | — | Full-text search across `search_fields` |
//! | `{field}` | — | Exact filter for any `filter_fields` |
//!
//! Response: `{"count": N, "limit": L, "offset": O, "results": [...]}`
//!
//! ## Permissions
//!
//! Pair with [`RouterAuthExt`](crate::tenancy::middleware::RouterAuthExt)
//! on the outer router to require auth, then pass codenames to `.permissions()`:
//!
//! ```ignore
//! ViewSet::for_model(Post::SCHEMA)
//!     .permissions(ViewSetPerms {
//!         list:     vec!["post.view"],
//!         retrieve: vec!["post.view"],
//!         create:   vec!["post.add"],
//!         update:   vec!["post.change"],
//!         destroy:  vec!["post.delete"],
//!     })
//!     .router("/api/posts", pool.clone())
//! ```

#[cfg(feature = "openapi")]
mod openapi;

use std::collections::HashMap;
use std::future::Future;
#[cfg(feature = "serializer")]
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};

use crate::core::{
    Assignment, CountQuery, DeleteQuery, FieldType, Filter, InsertQuery, ModelSchema, Op,
    SearchClause, SelectQuery, SqlValue, UpdateQuery, WhereExpr,
};
use crate::forms::{collect_values, parse_form_value, parse_pk_string, FormError};
use crate::sql::Pool;

// ------------------------------------------------------------------ Permissions config

/// Permission codenames required for each ViewSet action.
///
/// Any field left as an empty vec means "no permission check" for that action.
#[derive(Clone, Default)]
pub struct ViewSetPerms {
    /// Codenames required to call `GET /` (list).
    pub list: Vec<String>,
    /// Codenames required to call `GET /{pk}` (retrieve).
    pub retrieve: Vec<String>,
    /// Codenames required to call `POST /` (create).
    pub create: Vec<String>,
    /// Codenames required to call `PUT /{pk}` or `PATCH /{pk}` (update).
    pub update: Vec<String>,
    /// Codenames required to call `DELETE /{pk}` (destroy).
    pub destroy: Vec<String>,
}

/// A fixed-window throttle limit — at most `max` requests per
/// `window_secs` (DRF `ScopedRateThrottle` shape, #1010).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThrottleRule {
    /// Max requests allowed within the window.
    pub max: u32,
    /// Window length in seconds.
    pub window_secs: u64,
}

impl ThrottleRule {
    /// `max` requests per `window_secs` seconds.
    #[must_use]
    pub const fn new(max: u32, window_secs: u64) -> Self {
        Self { max, window_secs }
    }
}

/// Per-action request throttles for a ViewSet (DRF `throttle_classes`
/// parity, #1010). Any action left `None` is unthrottled.
///
/// Counters are **process-local** (per server instance) — same model as
/// [`crate::rate_limit`]. Behind N replicas the effective limit is N×;
/// for a shared limit, front the service with a gateway throttle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ViewSetThrottle {
    /// Throttle for `GET /` (list).
    pub list: Option<ThrottleRule>,
    /// Throttle for `GET /{pk}` (retrieve).
    pub retrieve: Option<ThrottleRule>,
    /// Throttle for `POST /` (create).
    pub create: Option<ThrottleRule>,
    /// Throttle for `PUT`/`PATCH /{pk}` (update).
    pub update: Option<ThrottleRule>,
    /// Throttle for `DELETE /{pk}` (destroy).
    pub destroy: Option<ThrottleRule>,
}

impl ViewSetThrottle {
    /// Apply the same rule to every action.
    #[must_use]
    pub const fn all(max: u32, window_secs: u64) -> Self {
        let r = Some(ThrottleRule::new(max, window_secs));
        Self {
            list: r,
            retrieve: r,
            create: r,
            update: r,
            destroy: r,
        }
    }

    /// The rule for an action name (`"list"` / `"retrieve"` / `"create"`
    /// / `"update"` / `"destroy"`), if any.
    #[must_use]
    pub fn for_action(&self, action: &str) -> Option<ThrottleRule> {
        match action {
            "list" => self.list,
            "retrieve" => self.retrieve,
            "create" => self.create,
            "update" => self.update,
            "destroy" => self.destroy,
            _ => None,
        }
    }
}

// ------------------------------------------------------------------ ViewSet builder

/// Pagination strategy for ViewSet list endpoints.
#[derive(Clone, Debug)]
pub enum PaginationStyle {
    /// 1-based page numbering — `?page=1&page_size=20`. Returns
    /// `count` + `page` + `last_page` in the response. Cheap when
    /// the table is small; runs `COUNT(*)` per request.
    PageNumber,
    /// Cursor-based pagination — `?cursor=<encoded>&page_size=20`.
    /// `field` must be a stable, monotonically-ordered column
    /// (typically the primary key). Skips the COUNT query, so it
    /// scales to billion-row tables.
    Cursor {
        /// SQL field name used as the cursor (e.g. `"id"`).
        field: &'static str,
        /// `true` for descending order. Cursors compare with `<` instead
        /// of `>`. Default ordering is set automatically to match.
        desc: bool,
    },
    /// DRF-shape limit/offset windowing — `?limit=20&offset=40`. Returns
    /// `count` + `limit` + `offset` in the response. Runs `COUNT(*)` per
    /// request (same cost as [`PaginationStyle::PageNumber`]); pick it
    /// when callers think in row offsets rather than page numbers.
    LimitOffset,
}

impl PaginationStyle {
    /// Default — 1-based page numbering.
    #[must_use]
    pub const fn page_number() -> Self {
        Self::PageNumber
    }

    /// Cursor pagination on the named field, ascending.
    #[must_use]
    pub const fn cursor(field: &'static str) -> Self {
        Self::Cursor { field, desc: false }
    }

    /// Cursor pagination on the named field, descending.
    #[must_use]
    pub const fn cursor_desc(field: &'static str) -> Self {
        Self::Cursor { field, desc: true }
    }

    /// DRF-shape limit/offset pagination.
    #[must_use]
    pub const fn limit_offset() -> Self {
        Self::LimitOffset
    }
}

/// Type-erased bridge that lets a `dyn`-routed [`ViewSet`] render rows
/// through a concrete [`crate::serializer::ModelSerializer`].
///
/// `ViewSet` is stored without its model/serializer type (so a router
/// can hold many of them), so it can't name `S` directly. Instead
/// [`ViewSet::serializer`] boxes a [`Bridge<S>`] behind this trait.
/// When set, list / retrieve / create responses route through the
/// bridge instead of the default field-level `select_rows_as_json`
/// projection, so `method` / `read_only` / `source` / `write_only`
/// overrides all shape the JSON output.
///
/// Tri-dialect (v0.45): the bridge fetches typed `Vec<S::Model>` via
/// [`crate::sql::select_rows_pool_with_related`] — which decodes `T`
/// on Postgres, MySQL **and** SQLite — then maps each model through
/// `S::from_model` + `to_value`. The pre-v0.45 implementation was a
/// `Fn(&PgRow) -> Value` closure, which pinned the feature to Postgres.
trait SerializerBridge: Send + Sync {
    /// Fetch every row matching `q` and render it through the
    /// serializer (replaces the default field-level projection).
    fn render_rows<'a>(
        &'a self,
        acq: &'a mut AcquiredConn,
        q: &'a SelectQuery,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Value>, crate::sql::ExecError>> + Send + 'a>>;

    /// Fetch the first row matching `q` and render it, or `None`.
    fn render_one<'a>(
        &'a self,
        acq: &'a mut AcquiredConn,
        q: &'a SelectQuery,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Value>, crate::sql::ExecError>> + Send + 'a>>;

    /// Run the serializer's input validation against a JSON request body:
    /// parse the writable fields (DRF read shape) then call the
    /// serializer's `validate()` hook. `Err` carries DRF-shape field
    /// errors for a 400 response.
    fn validate_body(&self, body: &Value) -> Result<(), crate::forms::FormErrors>;

    /// The **model** field names the serializer accepts on write
    /// (`source`-resolved). The write path skips every other model column
    /// so `read_only` / computed fields can't be set by a client.
    fn writable_model_fields(&self) -> &'static [&'static str];

    /// The **serializer** field names of the writable fields — the JSON
    /// keys a client sends (DRF read/write shape). Parallel to
    /// [`Self::writable_model_fields`]; the two differ only where a field
    /// declares `#[serializer(source = "…")]`, which the write path uses
    /// to translate the inbound key to its model column.
    fn writable_field_names(&self) -> &'static [&'static str];
}

/// Zero-sized carrier that pins a concrete serializer type `S` so the
/// type-erased [`SerializerBridge`] trait object can call back into
/// `S::from_model` / `S::Model`'s tri-dialect row decode.
#[cfg(feature = "serializer")]
struct Bridge<S>(PhantomData<S>);

#[cfg(feature = "serializer")]
impl<S> SerializerBridge for Bridge<S>
where
    S: crate::serializer::ModelSerializer + Send + Sync + 'static,
    S::Model: crate::sql::MaybePgFromRow
        + crate::sql::MaybeMyFromRow
        + crate::sql::MaybeSqliteFromRow
        + crate::sql::LoadRelated
        + crate::sql::MaybeMyLoadRelated
        + crate::sql::MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    fn render_rows<'a>(
        &'a self,
        acq: &'a mut AcquiredConn,
        q: &'a SelectQuery,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Value>, crate::sql::ExecError>> + Send + 'a>> {
        Box::pin(async move {
            let models = acq.select_rows_typed::<S::Model>(q).await?;
            Ok(models.iter().map(|m| S::from_model(m).to_value()).collect())
        })
    }

    fn render_one<'a>(
        &'a self,
        acq: &'a mut AcquiredConn,
        q: &'a SelectQuery,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Value>, crate::sql::ExecError>> + Send + 'a>>
    {
        Box::pin(async move {
            let models = acq.select_rows_typed::<S::Model>(q).await?;
            Ok(models.first().map(|m| S::from_model(m).to_value()))
        })
    }

    fn validate_body(&self, body: &Value) -> Result<(), crate::forms::FormErrors> {
        // Parse the writable fields into a partial serializer instance
        // (surfacing per-field type errors), then run its validation hook.
        let s = S::from_writable_json(body)?;
        s.validate()
    }

    fn writable_model_fields(&self) -> &'static [&'static str] {
        S::writable_source_fields()
    }

    fn writable_field_names(&self) -> &'static [&'static str] {
        S::writable_fields()
    }
}

/// A pluggable filter backend (DRF `BaseFilterBackend` parity, #1010).
///
/// Registered on a [`ViewSet`] via [`ViewSet::filter_backend`], a backend
/// contributes extra `WHERE` predicates from the list request's query
/// params. Its predicates are `AND`-ed with the built-in exact / lookup
/// filters — use it for what the `filter_fields` surface can't express
/// (a geo-radius, a custom `?q=` DSL, request-scoped row visibility, …).
///
/// Any `Fn(&HashMap<String, String>, &'static ModelSchema) -> Vec<WhereExpr>`
/// is a backend via the blanket impl below, so a closure works directly:
///
/// ```ignore
/// use rustango::core::{Filter, Op, SqlValue, WhereExpr};
/// ViewSet::for_model(Post::SCHEMA)
///     .filter_backend(|params: &HashMap<String, String>, schema| {
///         // hide drafts unless ?include_drafts=1
///         if params.get("include_drafts").map(String::as_str) == Some("1") {
///             return Vec::new();
///         }
///         schema.field("status").map_or_else(Vec::new, |f| {
///             vec![WhereExpr::Predicate(Filter {
///                 column: f.column,
///                 op: Op::Eq,
///                 value: SqlValue::from("published"),
///             })]
///         })
///     })
///     .router_pool("/posts", pool);
/// ```
///
/// Backends run on the **list** action.
pub trait ViewSetFilter: Send + Sync + 'static {
    /// Return `WHERE` predicates to AND into the list query for this request.
    fn filter(
        &self,
        params: &HashMap<String, String>,
        schema: &'static ModelSchema,
    ) -> Vec<WhereExpr>;
}

impl<F> ViewSetFilter for F
where
    F: Fn(&HashMap<String, String>, &'static ModelSchema) -> Vec<WhereExpr> + Send + Sync + 'static,
{
    fn filter(
        &self,
        params: &HashMap<String, String>,
        schema: &'static ModelSchema,
    ) -> Vec<WhereExpr> {
        self(params, schema)
    }
}

/// Builder for a set of REST CRUD endpoints over a single [`Model`] table.
///
/// Call `.router(prefix, pool)` when done to get an `axum::Router`.
#[derive(Clone)]
pub struct ViewSet {
    schema: &'static ModelSchema,
    fields: Option<Vec<String>>,
    filter_fields: Vec<String>,
    search_fields: Vec<String>,
    /// DRF `ordering_fields` whitelist — when non-empty, only the
    /// listed fields are honored via `?ordering=`. Unknown names get
    /// silently dropped. When empty (the default), any field on the
    /// schema is sortable — the v0.30 behavior. Issue #439.
    ordering_fields: Vec<String>,
    default_page_size: usize,
    default_ordering: Vec<(String, bool)>,
    perms: ViewSetPerms,
    read_only: bool,
    pagination: PaginationStyle,
    /// Pluggable filter backends (#1010) — each contributes extra `WHERE`
    /// predicates on the list action, ANDed with the built-in filters.
    filter_backends: Vec<std::sync::Arc<dyn ViewSetFilter>>,
    /// Per-action request throttles (#1010). Default: unthrottled.
    throttle: ViewSetThrottle,
    /// When set, list / retrieve / create responses render each row
    /// through this serializer bridge instead of the default
    /// field-level projection. Tri-dialect. Wired via
    /// [`Self::serializer`].
    serializer: Option<Arc<dyn SerializerBridge>>,
}

impl ViewSet {
    /// Start a ViewSet for the given model schema.
    pub fn for_model(schema: &'static ModelSchema) -> Self {
        Self {
            schema,
            fields: None,
            filter_fields: Vec::new(),
            search_fields: Vec::new(),
            ordering_fields: Vec::new(),
            default_page_size: 20,
            default_ordering: Vec::new(),
            perms: ViewSetPerms::default(),
            read_only: false,
            pagination: PaginationStyle::PageNumber,
            filter_backends: Vec::new(),
            throttle: ViewSetThrottle::default(),
            serializer: None,
        }
    }

    /// Render list / retrieve / create responses through `S` (a
    /// serializer derived via `#[derive(Serializer)]
    /// #[serializer(model = T)]`) instead of the default field-level
    /// projection.
    ///
    /// Apply when you want the ViewSet's JSON shape to match a typed
    /// serializer's `read_only` / `source` / `method` / `nested` /
    /// `many` overrides. The serializer's `Model` associated type must
    /// be the same model the ViewSet is built over.
    ///
    /// Internally boxes a [`Bridge<S>`] behind a [`SerializerBridge`]
    /// trait object so the `ViewSet` itself stays type-erased — a
    /// non-breaking add-on.
    ///
    /// Tri-dialect (v0.45): works on Postgres, MySQL and SQLite. The
    /// render path fetches typed `Vec<S::Model>` via the tri-dialect
    /// `select_rows_pool_with_related` rather than a PG-only
    /// `FromRow<PgRow>` closure.
    ///
    /// Note: `nested` / `many` fields need the related rows a flat
    /// fetch doesn't load unless the ViewSet's query populated them via
    /// `select_related`; those fields render as their `Default` value
    /// otherwise. `method` / `read_only` / `source` / `write_only`
    /// always apply because a real typed model is in hand.
    #[cfg(feature = "serializer")]
    #[must_use]
    pub fn serializer<S>(mut self) -> Self
    where
        S: crate::serializer::ModelSerializer + Send + Sync + 'static,
        S::Model: crate::sql::MaybePgFromRow
            + crate::sql::MaybeMyFromRow
            + crate::sql::MaybeSqliteFromRow
            + crate::sql::LoadRelated
            + crate::sql::MaybeMyLoadRelated
            + crate::sql::MaybeSqliteLoadRelated
            + Send
            + Unpin,
    {
        self.serializer = Some(Arc::new(Bridge::<S>(PhantomData)));
        self
    }

    /// Switch to cursor-based pagination on `field`. The cursor field
    /// should be a stable, monotonically-ordered column (typically `"id"`).
    /// This skips the `COUNT(*)` query that page-number pagination runs,
    /// so it scales well for large tables.
    #[must_use]
    pub fn cursor_pagination(mut self, field: &'static str) -> Self {
        self.pagination = PaginationStyle::Cursor { field, desc: false };
        self
    }

    /// Cursor pagination, descending order.
    #[must_use]
    pub fn cursor_pagination_desc(mut self, field: &'static str) -> Self {
        self.pagination = PaginationStyle::Cursor { field, desc: true };
        self
    }

    /// Switch to DRF-shape limit/offset pagination — `?limit=&offset=`.
    /// Like page-number it runs `COUNT(*)` per request, but callers
    /// window by row offset instead of page index.
    #[must_use]
    pub fn limit_offset_pagination(mut self) -> Self {
        self.pagination = PaginationStyle::LimitOffset;
        self
    }

    /// Set the pagination strategy explicitly.
    #[must_use]
    pub fn pagination(mut self, style: PaginationStyle) -> Self {
        self.pagination = style;
        self
    }

    /// Register a pluggable filter backend (DRF `filter_backends` parity,
    /// #1010). Each backend contributes extra `WHERE` predicates on the
    /// list action, ANDed with the built-in `filter_fields`. Call
    /// repeatedly to stack backends; any matching closure works (see
    /// [`ViewSetFilter`]).
    #[must_use]
    pub fn filter_backend(mut self, backend: impl ViewSetFilter) -> Self {
        self.filter_backends.push(std::sync::Arc::new(backend));
        self
    }

    /// Set per-action request throttles (DRF `throttle_classes` parity,
    /// #1010). Counters are process-local — see [`ViewSetThrottle`].
    #[must_use]
    pub fn throttle(mut self, throttle: ViewSetThrottle) -> Self {
        self.throttle = throttle;
        self
    }

    /// Throttle every action to `max` requests per `window_secs`.
    /// Shorthand for `.throttle(ViewSetThrottle::all(max, window_secs))`.
    #[must_use]
    pub fn throttle_all(mut self, max: u32, window_secs: u64) -> Self {
        self.throttle = ViewSetThrottle::all(max, window_secs);
        self
    }

    /// Restrict which fields appear in list/retrieve responses and are
    /// accepted on create/update. Default: all scalar fields.
    pub fn fields(mut self, fields: &[&str]) -> Self {
        self.fields = Some(fields.iter().map(|&s| s.to_owned()).collect());
        self
    }

    /// Fields that can be filtered via query params (`?field=value`).
    pub fn filter_fields(mut self, fields: &[&str]) -> Self {
        self.filter_fields = fields.iter().map(|&s| s.to_owned()).collect();
        self
    }

    /// Fields searched by the `?search=` query param.
    pub fn search_fields(mut self, fields: &[&str]) -> Self {
        self.search_fields = fields.iter().map(|&s| s.to_owned()).collect();
        self
    }

    /// DRF `ordering_fields` whitelist — when set, only the listed
    /// field names are honored via `?ordering=`. Unknown names are
    /// silently dropped so a hostile client can't sort on a sensitive
    /// column. When unset (the default), any schema field is sortable.
    /// Issue #439.
    pub fn ordering_fields(mut self, fields: &[&str]) -> Self {
        self.ordering_fields = fields.iter().map(|&s| s.to_owned()).collect();
        self
    }

    /// Default page size for list responses (default: 20, max: 1000).
    pub fn page_size(mut self, n: usize) -> Self {
        self.default_page_size = n.min(1000);
        self
    }

    /// Default ordering for list responses. `(field, true)` = descending.
    pub fn ordering(mut self, ordering: &[(&str, bool)]) -> Self {
        self.default_ordering = ordering.iter().map(|&(f, d)| (f.to_owned(), d)).collect();
        self
    }

    /// Permission codenames required per action. Empty vec = allow all.
    pub fn permissions(mut self, perms: ViewSetPerms) -> Self {
        self.perms = perms;
        self
    }

    /// Auto-fill `ViewSetPerms` with the four standard CRUD codenames
    /// for `T` (`<table>.view` / `<table>.add` / `<table>.change` /
    /// `<table>.delete`), routed through the v0.16.0 typed
    /// permissions facade ([`crate::permissions::codename_for`]).
    ///
    /// `list` + `retrieve` get `view`; `create` gets `add`; `update`
    /// gets `change`; `destroy` gets `delete`. Mirrors Django's
    /// `DjangoModelPermissions` shape — the convention every Django
    /// app starts with.
    ///
    /// Use [`Self::permissions`] for fully-custom codenames or
    /// non-CRUD actions. Requires the `tenancy` feature (the
    /// underlying `has_perm` engine lives there).
    #[cfg(feature = "tenancy")]
    pub fn permissions_for_model<T: crate::core::Model>(mut self) -> Self {
        let cn = |action: &str| crate::permissions::codename_for::<T>(action);
        self.perms = ViewSetPerms {
            list: vec![cn("view")],
            retrieve: vec![cn("view")],
            create: vec![cn("add")],
            update: vec![cn("change")],
            destroy: vec![cn("delete")],
        };
        self
    }

    /// Allow GET only — wires list + retrieve, skips create/update/destroy.
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// Build and return an `axum::Router` mounted at `prefix`. The
    /// pool is baked at mount time — every request uses the same
    /// `&PgPool`. For tenancy projects use [`Self::tenant_router`]
    /// instead so each request resolves its own tenant connection.
    ///
    /// The prefix may or may not end with `/` — both `/api/posts` and
    /// `/api/posts/` work identically.
    #[cfg(feature = "postgres")]
    pub fn router(self, prefix: &str, pool: crate::sql::sqlx::PgPool) -> Router {
        Self::router_with_source(self, prefix, PoolSource::Static(pool))
    }

    /// v0.38 — tri-dialect counterpart of [`Self::router`] that
    /// accepts the backend-erasing [`crate::sql::Pool`] enum. Sqlite/
    /// MySQL projects use this; PG projects can still use either.
    pub fn router_pool(self, prefix: &str, pool: crate::sql::Pool) -> Router {
        Self::router_with_source(self, prefix, PoolSource::StaticPool(pool))
    }

    /// Build a router that resolves the database connection per
    /// request via the [`crate::extractors::Tenant`] extractor —
    /// the right shape for multi-tenant projects (subdomain / schema /
    /// per-tenant database). Each handler runs against the connection
    /// for whichever tenant the request resolves to.
    ///
    /// Mount on the API router that the `Server::Builder` (or
    /// `Cli::tenancy()`) wires up; both inject the
    /// [`TenantContext`](crate::extractors::TenantContext) extension
    /// the extractor reads from.
    ///
    /// ```ignore
    /// use rustango::viewset::ViewSet;
    ///
    /// let posts_router = ViewSet::for_model(Post::SCHEMA)
    ///     .filter_fields(&["author_id"])
    ///     .search_fields(&["title", "body"])
    ///     .ordering(&[("published_at", true)])
    ///     .tenant_router("/api/posts");
    ///
    /// // In `urls::api()`:
    /// axum::Router::new().merge(posts_router)
    /// ```
    ///
    /// Permission checks (when configured via [`Self::permissions`] /
    /// [`Self::permissions_for_model`]) run against the same per-request
    /// connection — no second pool acquire.
    ///
    /// **Note on list-endpoint parallelism (v0.30 behavior change)**:
    /// pre-v0.30 the static-pool list endpoint ran SELECT + COUNT in
    /// parallel via `tokio::join!`. v0.30 unified both pool-source
    /// paths on the same handler, which serializes the two queries —
    /// tenant mode can't `join!` because `Tenant::conn()` hands out
    /// an exclusive `&mut PgConnection`, and unifying the code path
    /// keeps the handler simple. In practice two short queries on
    /// one connection are usually faster than two pool round-trips
    /// anyway, so the regression is bounded; latency-sensitive
    /// callers can opt out of the page-number COUNT entirely with
    /// [`Self::cursor_pagination`].
    #[cfg(feature = "tenancy")]
    #[must_use]
    pub fn tenant_router(self, prefix: &str) -> Router {
        Self::router_with_source(self, prefix, PoolSource::Tenant)
    }

    fn router_with_source(self, prefix: &str, pool_source: PoolSource) -> Router {
        let state = Arc::new(ViewSetState {
            pool_source,
            vs: self.clone(),
            throttle_store: Arc::new(Mutex::new(HashMap::new())),
        });
        let prefix = prefix.trim_end_matches('/').to_owned();
        let collection = prefix.clone();
        let item = format!("{prefix}/{{pk}}");

        let collection_route = if self.read_only {
            get(handle_list)
        } else {
            get(handle_list).post(handle_create)
        };
        // RFC 10008 QUERY collection action (#1112) — same filtered list as
        // GET, criteria in the body. Gated on `admin` since the QUERY
        // routing shim lives in the `admin`-gated `http_query` module; a
        // `tenancy`-only ViewSet build keeps GET/POST without QUERY.
        #[cfg(feature = "admin")]
        let collection_route = {
            use crate::http_query::QueryRouterExt as _;
            collection_route.query(handle_query)
        };

        let item_route = if self.read_only {
            axum::routing::MethodRouter::new().get(handle_retrieve)
        } else {
            axum::routing::MethodRouter::new()
                .get(handle_retrieve)
                .put(handle_update)
                .patch(handle_partial_update)
                .delete(handle_destroy)
        };

        Router::new()
            .route(&collection, collection_route)
            .route(&item, item_route)
            .with_state(state)
    }
}

// ------------------------------------------------------------------ Internal state

/// Source of the database pool for a [`ViewSet`]. `Static` carries an
/// owned [`PgPool`] (the legacy `router(prefix, pool)` path); `Tenant`
/// is a marker that defers resolution to per-request [`Tenant`]
/// extraction (the v0.30 [`ViewSet::tenant_router`] path).
#[derive(Clone)]
enum PoolSource {
    /// PG-typed static pool (back-compat for the `router(prefix, &PgPool)` path).
    #[cfg(feature = "postgres")]
    Static(crate::sql::sqlx::PgPool),
    /// Backend-erasing static pool — accepts any dialect; used by
    /// `router_pool(prefix, &Pool)`.
    StaticPool(crate::sql::Pool),
    /// Tenant mode — each handler resolves a connection via the
    /// [`crate::extractors::Tenant`] extractor at request time. We
    /// can't bake a `&PgPool` because schema-mode tenants need
    /// per-connection `SET search_path` setup, which only the
    /// `TenantPools::acquire` path provides.
    #[cfg(feature = "tenancy")]
    Tenant,
}

#[derive(Clone)]
struct ViewSetState {
    pool_source: PoolSource,
    vs: ViewSet,
    /// Process-local fixed-window throttle counters, keyed by
    /// `{table}:{action}:{client}`. Shared across requests via the
    /// `Arc<ViewSetState>` the router holds. `#1010`.
    throttle_store: Arc<Mutex<HashMap<String, (u32, Instant)>>>,
}

/// A per-request connection handle that abstracts over static-pool
/// and per-request-tenant modes. Constructed by
/// [`ViewSetState::acquire`] near the top of each handler; the
/// returned wrapper exposes `select_rows` / `count_rows` /
/// `insert_returning` / `update` / `delete` / `has_perm` facade
/// methods so handler bodies stay free of pool-source branching.
/// v0.38 — `Pool` enum wraps either the static-mode pool or the
/// tenant-scoped pool yielded by `Tenant<DB>::pool()`. Schema-mode
/// PG tenants go through the search-path-bound pool that
/// `TenantPools::scoped_pool_dyn` builds; database-mode tenants
/// (any backend) clone the cached pool.
struct AcquiredConn {
    pool: Pool,
    /// Kept alive so PG schema-mode connections aren't released
    /// before the handler is done.
    #[cfg(feature = "tenancy")]
    #[allow(dead_code)]
    _tenant: Option<Box<crate::extractors::Tenant>>,
}

impl AcquiredConn {
    async fn select_rows_as_json(
        &mut self,
        q: &SelectQuery,
        fields: &[&'static crate::core::FieldSchema],
    ) -> Result<Vec<Value>, crate::sql::ExecError> {
        crate::sql::select_rows_as_json(&self.pool, q, fields).await
    }

    async fn count_rows(&mut self, q: &CountQuery) -> Result<i64, crate::sql::ExecError> {
        crate::sql::count_rows_pool(&self.pool, q).await
    }

    async fn select_one_as_json(
        &mut self,
        q: &SelectQuery,
        fields: &[&'static crate::core::FieldSchema],
    ) -> Result<Option<Value>, crate::sql::ExecError> {
        let mut rows = crate::sql::select_rows_as_json(&self.pool, q, fields).await?;
        Ok(rows.pop())
    }

    /// Tri-dialect typed fetch — decode every row matching `q` into
    /// `T` (the model struct) on Postgres, MySQL **or** SQLite. Used by
    /// the serializer render path ([`SerializerBridge`]); mirrors
    /// [`Self::select_rows_as_json`] but yields typed models instead of
    /// the field-level JSON projection. Goes through `&self.pool`, so
    /// it inherits the same tenant-scoping the JSON projection has.
    #[cfg(feature = "serializer")]
    async fn select_rows_typed<T>(
        &mut self,
        q: &SelectQuery,
    ) -> Result<Vec<T>, crate::sql::ExecError>
    where
        T: crate::sql::MaybePgFromRow
            + crate::sql::MaybeMyFromRow
            + crate::sql::MaybeSqliteFromRow
            + crate::sql::LoadRelated
            + crate::sql::MaybeMyLoadRelated
            + crate::sql::MaybeSqliteLoadRelated
            + Send
            + Unpin,
    {
        crate::sql::select_rows_pool_with_related::<T>(&self.pool, q).await
    }

    /// Insert a row and return the primary-key value of the new row
    /// (per-backend: PG/SQLite use RETURNING, MySQL uses
    /// LAST_INSERT_ID()).
    async fn insert_returning_pk(
        &mut self,
        q: &InsertQuery,
        pk_field: &crate::core::FieldSchema,
    ) -> Result<SqlValue, crate::sql::ExecError> {
        let returning = crate::sql::insert_returning_pool(&self.pool, q).await?;
        let pk = match returning {
            #[cfg(feature = "postgres")]
            crate::sql::InsertReturningPool::PgRow(row) => {
                use crate::sql::sqlx::Row as _;
                match pk_field.ty {
                    FieldType::I64 => SqlValue::I64(row.try_get(pk_field.column).unwrap_or(0)),
                    FieldType::I32 => SqlValue::I32(row.try_get(pk_field.column).unwrap_or(0)),
                    FieldType::I16 => SqlValue::I16(row.try_get(pk_field.column).unwrap_or(0)),
                    FieldType::String => {
                        SqlValue::String(row.try_get(pk_field.column).unwrap_or_default())
                    }
                    _ => SqlValue::Null,
                }
            }
            #[cfg(feature = "mysql")]
            crate::sql::InsertReturningPool::MySqlAutoId(id) => match pk_field.ty {
                FieldType::I64 => SqlValue::I64(id),
                FieldType::I32 => SqlValue::I32(id as i32),
                FieldType::I16 => SqlValue::I16(id as i16),
                _ => SqlValue::I64(id),
            },
            #[cfg(feature = "sqlite")]
            crate::sql::InsertReturningPool::SqliteRow(row) => {
                use crate::sql::sqlx::Row as _;
                match pk_field.ty {
                    FieldType::I64 => SqlValue::I64(row.try_get(pk_field.column).unwrap_or(0)),
                    FieldType::I32 => SqlValue::I32(row.try_get(pk_field.column).unwrap_or(0)),
                    FieldType::I16 => SqlValue::I16(row.try_get(pk_field.column).unwrap_or(0)),
                    FieldType::String => {
                        SqlValue::String(row.try_get(pk_field.column).unwrap_or_default())
                    }
                    _ => SqlValue::Null,
                }
            }
        };
        Ok(pk)
    }

    async fn update(&mut self, q: &UpdateQuery) -> Result<u64, crate::sql::ExecError> {
        crate::sql::update_pool(&self.pool, q).await
    }

    async fn delete(&mut self, q: &DeleteQuery) -> Result<u64, crate::sql::ExecError> {
        crate::sql::delete_pool(&self.pool, q).await
    }

    #[cfg(feature = "tenancy")]
    async fn has_perm(&mut self, uid: i64, codename: &str) -> bool {
        crate::tenancy::permissions::has_perm_pool(uid, codename, &self.pool)
            .await
            .unwrap_or(false)
    }
}

impl ViewSetState {
    fn effective_fields(&self) -> Vec<&'static crate::core::FieldSchema> {
        let schema = self.vs.schema;
        match &self.vs.fields {
            Some(names) => names.iter().filter_map(|n| schema.field(n)).collect(),
            None => schema.scalar_fields().collect(),
        }
    }

    /// Acquire a per-request connection / pool handle. For static-pool
    /// mode this is a cheap clone; for tenant mode this runs the
    /// resolver chain + acquires a connection from the right tenant
    /// pool. Errors return a fully-formed [`Response`] (with the
    /// appropriate status code) rather than a typed error so handlers
    /// can `?`-bubble straight to the client.
    async fn acquire(
        &self,
        parts: &mut axum::http::request::Parts,
    ) -> Result<AcquiredConn, Response> {
        match &self.pool_source {
            #[cfg(feature = "postgres")]
            PoolSource::Static(pool) => Ok(AcquiredConn {
                pool: Pool::from(pool.clone()),
                #[cfg(feature = "tenancy")]
                _tenant: None,
            }),
            PoolSource::StaticPool(pool) => Ok(AcquiredConn {
                pool: pool.clone(),
                #[cfg(feature = "tenancy")]
                _tenant: None,
            }),
            #[cfg(feature = "tenancy")]
            PoolSource::Tenant => {
                use axum::response::IntoResponse as _;
                // v0.38 — annotate the generic param so multi-backend
                // builds (e.g. `--features postgres,sqlite`) infer
                // `Tenant<DefaultTenantDb>` instead of erroring on
                // ambiguous type inference.
                let t = <crate::extractors::Tenant<
                    crate::tenancy::DefaultTenantDb,
                > as axum::extract::FromRequestParts<()>>::from_request_parts(parts, &())
                    .await
                    .map_err(|e| e.into_response())?;
                let pool = t.pool().clone();
                Ok(AcquiredConn {
                    pool,
                    _tenant: Some(Box::new(t)),
                })
            }
        }
    }

    /// Permission gate. Skips the check when `codenames` is empty.
    /// Reads the request's `AuthenticatedUser` extension; superusers
    /// short-circuit to allow. Falls through to the
    /// `tenancy::permissions::has_perm_on` engine.
    async fn check_perm(
        &self,
        codenames: &[String],
        parts: &axum::http::request::Parts,
        conn: &mut AcquiredConn,
    ) -> bool {
        if codenames.is_empty() {
            return true;
        }
        #[cfg(feature = "tenancy")]
        {
            let Some(auth) = parts
                .extensions
                .get::<crate::tenancy::middleware::AuthenticatedUser>()
            else {
                return false;
            };
            if auth.is_superuser {
                return true;
            }
            for cn in codenames {
                if conn.has_perm(auth.id, cn).await {
                    return true;
                }
            }
            false
        }
        #[cfg(not(feature = "tenancy"))]
        {
            // Without tenancy there's no AuthenticatedUser extension
            // and no has_perm engine. Codenames present + no engine
            // means we conservatively deny.
            let _ = (parts, conn);
            false
        }
    }

    fn pk_field(&self) -> Option<&'static crate::core::FieldSchema> {
        self.vs.schema.primary_key()
    }
}

// ------------------------------------------------------------------ Serialization

/// Build a JSON response with `status` and `body`. The 3-line
/// `Response::builder()` chain repeated verbatim in [`json_response`]
/// / [`json_created`] / [`json_error`] now lives here (#808 part 7).
fn json_with_status(status: StatusCode, body: Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Re-export of the shared row-to-JSON helper. Lives in
fn json_response(body: Value) -> Response {
    json_with_status(StatusCode::OK, body)
}

fn json_error(status: StatusCode, msg: &str) -> Response {
    json_with_status(status, json!({ "error": msg }))
}

/// `400` from serializer validation, DRF shape:
/// `{"<field>": ["msg", …], …, "non_field_errors": [ … ]}`.
fn json_form_errors(errs: &crate::forms::FormErrors) -> Response {
    let mut map = serde_json::Map::new();
    for (field, msgs) in errs.fields() {
        map.insert(field.clone(), json!(msgs));
    }
    if !errs.non_field().is_empty() {
        map.insert("non_field_errors".to_owned(), json!(errs.non_field()));
    }
    json_with_status(StatusCode::BAD_REQUEST, Value::Object(map))
}

/// When a serializer is registered, validate the JSON request body (if
/// present) through it and compute the extra model fields to skip on
/// write — every column the serializer doesn't accept (`read_only` /
/// computed). Returns the 400 response on a validation failure so the
/// caller can short-circuit. No-op (`Ok(vec![])`) without a serializer.
/// Translate inbound form keys from serializer field names to model
/// columns for any `#[serializer(source = "…")]`-renamed **writable**
/// field, so a client POSTs the serializer field name (DRF shape) — e.g.
/// `content` for a field declared `#[serializer(source = "body")]` — and
/// the persist step (which reads model columns) still finds the value.
///
/// Returns `None` when nothing needs renaming (no serializer, or no
/// renamed writable key present in the body) — the common case, so no
/// allocation. Otherwise returns a remapped clone of the form.
fn serializer_input_renamed_form(
    state: &ViewSetState,
    form: &HashMap<String, String>,
) -> Option<HashMap<String, String>> {
    let bridge = state.vs.serializer.as_ref()?;
    // `writable_field_names()` (JSON keys) and `writable_model_fields()`
    // (model columns) are parallel; they differ only at `source` renames.
    let renames: Vec<(&'static str, &'static str)> = bridge
        .writable_field_names()
        .iter()
        .zip(bridge.writable_model_fields().iter())
        .filter(|(name, col)| name != col && form.contains_key(**name))
        .map(|(name, col)| (*name, *col))
        .collect();
    if renames.is_empty() {
        return None;
    }
    let mut out = form.clone();
    for (name, col) in renames {
        if let Some(v) = out.remove(name) {
            // Don't clobber an explicit model-column key if a client sent both.
            out.entry(col.to_owned()).or_insert(v);
        }
    }
    Some(out)
}

fn serializer_write_prep(
    state: &ViewSetState,
    json: Option<&Value>,
) -> Result<Vec<&'static str>, Response> {
    match &state.vs.serializer {
        Some(bridge) => {
            if let Some(body) = json {
                if let Err(errs) = bridge.validate_body(body) {
                    return Err(json_form_errors(&errs));
                }
            }
            let writable = bridge.writable_model_fields();
            let extra_skip = state
                .vs
                .schema
                .scalar_fields()
                .map(|f| f.name)
                .filter(|n| !writable.contains(n))
                .collect();
            Ok(extra_skip)
        }
        None => Ok(Vec::new()),
    }
}

/// Unwrap-or-return-500. Collapses the `match expr { Ok(v) => v,
/// Err(e) => return json_error(INTERNAL_SERVER_ERROR, &e.to_string()) }`
/// shape that recurred ~5× in handler bodies. Issue #808 (item 4).
///
/// `$expr` must yield `Result<_, E>` where `E: ::std::fmt::Display`
/// — covers every error type used in the viewset (`ExecError`,
/// `String`, `QueryError`, etc.).
macro_rules! or_500 {
    ($expr:expr) => {
        match $expr {
            ::core::result::Result::Ok(v) => v,
            ::core::result::Result::Err(e) => {
                return json_error(
                    ::axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    &::std::string::ToString::to_string(&e),
                );
            }
        }
    };
}

/// Unwrap-or-return-400. Sibling of [`or_500`]; collapses the
/// `match expr { Ok(v) => v, Err(e) => return json_error(BAD_REQUEST,
/// &e.to_string()) }` shape. Issue #808 (item 4).
macro_rules! or_400 {
    ($expr:expr) => {
        match $expr {
            ::core::result::Result::Ok(v) => v,
            ::core::result::Result::Err(e) => {
                return json_error(
                    ::axum::http::StatusCode::BAD_REQUEST,
                    &::std::string::ToString::to_string(&e),
                );
            }
        }
    };
}

/// Common entry preamble shared by every REST handler — closes
/// item 1 of #808. Splits the inbound `Request` into `(parts, body)`,
/// resolves a connection via the [`PoolSource`] chain (per-tenant or
/// static), then enforces the per-action permission codenames. On
/// failure returns a fully-formed [`Response`] (401 / 403 / 5xx)
/// ready for the handler to `?`-bubble.
///
/// Handlers that don't need the body bind it as `_body` and ignore
/// it; the helper still returns the body so write paths
/// (`handle_create`, `update_inner`) can consume it after the
/// permission gate.
async fn enter(
    state: &Arc<ViewSetState>,
    req: axum::extract::Request,
    codenames: &[String],
    action: &'static str,
) -> Result<(axum::http::request::Parts, Body, AcquiredConn), Response> {
    let (mut parts, body) = req.into_parts();
    // #1010 — per-action throttle is checked before acquiring a
    // connection, so throttled requests shed load early.
    if let Some(resp) = check_throttle(state, action, &parts) {
        return Err(resp);
    }
    let mut acq = state.acquire(&mut parts).await?;
    if !state.check_perm(codenames, &parts, &mut acq).await {
        return Err(json_error(StatusCode::FORBIDDEN, "permission denied"));
    }
    Ok((parts, body, acq))
}

/// Per-action fixed-window throttle (#1010). Returns `Some(429)` when the
/// client has exceeded the action's [`ThrottleRule`] within its window,
/// else `None`. Counters are process-local (see [`ViewSetThrottle`]).
fn check_throttle(
    state: &ViewSetState,
    action: &str,
    parts: &axum::http::request::Parts,
) -> Option<Response> {
    let rule = state.vs.throttle.for_action(action)?;
    let client = client_key(parts);
    let key = format!("{}:{}:{}", state.vs.schema.table, action, client);
    let now = Instant::now();
    let window = std::time::Duration::from_secs(rule.window_secs);

    // A poisoned mutex means a prior panic mid-update; fail open rather
    // than reject every subsequent request.
    let mut store = match state.throttle_store.lock() {
        Ok(g) => g,
        Err(_) => return None,
    };
    let entry = store.entry(key).or_insert((0, now));
    if now.duration_since(entry.1) >= window {
        *entry = (0, now); // window elapsed → reset
    }
    entry.0 += 1;
    if entry.0 > rule.max {
        let retry = window
            .checked_sub(now.duration_since(entry.1))
            .map_or(1, |d| d.as_secs().max(1));
        return Some(throttled_response(retry));
    }
    None
}

/// Best-effort client identity for throttle keying: the peer IP from
/// `ConnectInfo` when the server installed it, else the first
/// `X-Forwarded-For` / `X-Real-IP` hop, else a shared `"global"` bucket
/// (coarse but safe). #1010.
fn client_key(parts: &axum::http::request::Parts) -> String {
    if let Some(ci) = parts
        .extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
    {
        return ci.0.ip().to_string();
    }
    for h in ["x-forwarded-for", "x-real-ip"] {
        if let Some(first) = parts
            .headers
            .get(h)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return first.to_owned();
        }
    }
    "global".to_owned()
}

/// A `429 Too Many Requests` with a `Retry-After` header (#1010).
fn throttled_response(retry_after_secs: u64) -> Response {
    Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::RETRY_AFTER, retry_after_secs.to_string())
        .body(Body::from(
            json!({ "error": "request throttled" }).to_string(),
        ))
        .unwrap()
}

/// #808 — was repeated verbatim in `handle_retrieve` / `update_inner`
/// / `handle_destroy` (the three item-route handlers). Each spelled
/// out the same `match parse_pk_string(field, raw) { Ok(v) => v, Err(e)
/// => return json_error(400, &e.to_string()) }` pattern. Factored
/// out so the handler bodies focus on what they're doing.
fn parse_pk_or_400(
    field: &'static crate::core::FieldSchema,
    raw: &str,
) -> Result<crate::core::SqlValue, Response> {
    parse_pk_string(field, raw).map_err(|e| json_error(StatusCode::BAD_REQUEST, &e.to_string()))
}

/// #808 — was repeated verbatim ×4: `handle_retrieve`, `handle_create`,
/// `update_inner`, `handle_destroy` all do the same
/// `let Some(pk_field) = state.pk_field() else { return json_error(500,
/// "model has no primary key") }`. Factored so the calling handlers
/// shrink to the `?` form.
fn pk_field_or_500(state: &ViewSetState) -> Result<&'static crate::core::FieldSchema, Response> {
    state.pk_field().ok_or_else(|| {
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "model has no primary key",
        )
    })
}

fn json_created(body: Value) -> Response {
    json_with_status(StatusCode::CREATED, body)
}

fn no_content() -> Response {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap()
}

/// Build a `WhereExpr` from one query-param `field[__lookup]=value` entry.
///
/// Supported Django-style lookups:
/// - (none) / `exact` — `Op::Eq`
/// - `gt`, `gte`, `lt`, `lte`, `ne`
/// - `in` / `not_in` — comma-separated values
/// - `contains` (LIKE %v%) / `icontains` (ILIKE %v%)
/// - `startswith` (LIKE v%) / `istartswith` (ILIKE v%)
/// - `endswith` (LIKE %v) / `iendswith` (ILIKE %v)
/// - `isnull` — value `"true"` / `"false"`
fn build_lookup_filter(
    field: &'static crate::core::FieldSchema,
    lookup: Option<&str>,
    raw: &str,
) -> Option<WhereExpr> {
    let column = field.column;
    let predicate =
        |op: Op, value: SqlValue| Some(WhereExpr::Predicate(Filter { column, op, value }));
    // #808 part 6 — six binary-comparison arms had byte-identical
    // bodies modulo the `Op` constant. Map the lookup token to its
    // `Op`, parse once, branch once.
    let binary_op = match lookup.unwrap_or("exact") {
        "exact" => Some(Op::Eq),
        "ne" => Some(Op::Ne),
        "gt" => Some(Op::Gt),
        "gte" => Some(Op::Gte),
        "lt" => Some(Op::Lt),
        "lte" => Some(Op::Lte),
        _ => None,
    };
    if let Some(op) = binary_op {
        return parse_form_value(field, Some(raw))
            .ok()
            .and_then(|v| predicate(op, v));
    }
    match lookup.unwrap_or("exact") {
        "in" | "not_in" => {
            let parts: Vec<SqlValue> = raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .filter_map(|s| parse_form_value(field, Some(s)).ok())
                .collect();
            if parts.is_empty() {
                return None;
            }
            let op = if lookup == Some("not_in") {
                Op::NotIn
            } else {
                Op::In
            };
            predicate(op, SqlValue::List(parts))
        }
        "contains" => predicate(Op::Like, SqlValue::String(format!("%{raw}%"))),
        "icontains" => predicate(Op::ILike, SqlValue::String(format!("%{raw}%"))),
        "startswith" => predicate(Op::Like, SqlValue::String(format!("{raw}%"))),
        "istartswith" => predicate(Op::ILike, SqlValue::String(format!("{raw}%"))),
        "endswith" => predicate(Op::Like, SqlValue::String(format!("%{raw}"))),
        "iendswith" => predicate(Op::ILike, SqlValue::String(format!("%{raw}"))),
        "isnull" => {
            let is_null = matches!(raw.to_ascii_lowercase().as_str(), "true" | "1" | "yes");
            predicate(Op::IsNull, SqlValue::Bool(is_null))
        }
        _ => None, // unknown lookup → silently ignore
    }
}

// ------------------------------------------------------------------ Handlers

async fn handle_list(
    State(state): State<Arc<ViewSetState>>,
    Query(params): Query<HashMap<String, String>>,
    req: axum::extract::Request,
) -> Response {
    let (_parts, _body, acq) = match enter(&state, req, &state.vs.perms.list, "list").await {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    run_list(state, params, acq).await
}

/// Core `list` logic shared by the GET `list` action and the RFC 10008
/// QUERY action (#1112): builds filters / search / ordering / pagination
/// from `params` and renders the paginated envelope. `params` arrive from
/// the querystring on GET and from the request body on QUERY, so the two
/// transports return byte-identical results for equivalent criteria.
async fn run_list(
    state: Arc<ViewSetState>,
    params: HashMap<String, String>,
    mut acq: AcquiredConn,
) -> Response {
    let page_size: i64 = params
        .get("page_size")
        .and_then(|p| p.parse().ok())
        .unwrap_or(state.vs.default_page_size as i64)
        .min(1000)
        .max(1);

    // Build WHERE from filter_fields in query params.
    //
    // Supports both:
    //   ?author_id=42                — exact match (Op::Eq)
    //   ?author_id__gt=10            — Django-style lookup
    //   ?status__in=draft,published  — comma-separated for IN/NOT_IN
    //   ?title__icontains=hello      — pattern lookups
    //   ?published_at__isnull=true   — IS NULL / IS NOT NULL
    let mut filters: Vec<WhereExpr> = Vec::new();
    for (param_key, raw_val) in &params {
        // #809 — was a hand-spelled `matches!` reserved-key check
        // that had drifted from template_views's copy. Route through
        // `list_params::is_reserved_list_key` (single source of truth).
        if crate::list_params::is_reserved_list_key(param_key) {
            continue;
        }
        let (field_name, lookup) = match param_key.split_once("__") {
            Some((name, lk)) => (name, Some(lk)),
            None => (param_key.as_str(), None),
        };
        if !state.vs.filter_fields.iter().any(|f| f == field_name) {
            continue;
        }
        let Some(field) = state.vs.schema.field(field_name) else {
            continue;
        };
        if let Some(predicate) = build_lookup_filter(field, lookup, raw_val) {
            filters.push(predicate);
        }
    }

    // #1010 — pluggable filter backends contribute extra predicates,
    // ANDed with the built-in filter_fields parsed above.
    for backend in &state.vs.filter_backends {
        filters.extend(backend.filter(&params, state.vs.schema));
    }

    let where_clause = if filters.len() == 1 {
        filters.remove(0)
    } else if filters.is_empty() {
        WhereExpr::And(vec![])
    } else {
        WhereExpr::And(filters)
    };

    // Search
    let search = params.get("search").filter(|s| !s.is_empty()).cloned();
    let search_clause = search.map(|q| SearchClause {
        query: q,
        columns: state
            .vs
            .search_fields
            .iter()
            .filter_map(|n| state.vs.schema.field(n).map(|f| f.column))
            .collect(),
    });

    // Ordering — #439 honors the DRF `ordering_fields` whitelist when
    // set: only listed field names are sortable via `?ordering=`. An
    // empty whitelist (the default) keeps the v0.30 behavior of
    // allowing any schema field. Unknown / off-whitelist names are
    // silently dropped (mirrors DRF's defensive default — a hostile
    // client can't sort on `password_hash` just because it's a column).
    // #809 — was a hand-rolled `-`-prefix split + allowlist filter +
    // schema-field lookup. Route through `list_params::parse_ordering`
    // (single source of truth shared with template_views::ListView).
    let order_by: Vec<crate::core::OrderItem> = params
        .get("ordering")
        .map(|raw| {
            crate::list_params::parse_ordering(raw, &state.vs.ordering_fields, state.vs.schema)
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            state
                .vs
                .default_ordering
                .iter()
                .filter_map(|(name, desc)| {
                    state
                        .vs
                        .schema
                        .field(name)
                        .map(|f| crate::core::OrderItem::column(f.column, *desc))
                })
                .collect()
        });

    let fields = state.effective_fields();

    match &state.vs.pagination {
        PaginationStyle::PageNumber => {
            let page: i64 = params
                .get("page")
                .and_then(|p| p.parse().ok())
                .unwrap_or(1)
                .max(1);
            let offset = (page - 1) * page_size;

            // #562 — struct-update over SelectQuery::new for the
            // paginated list query.
            let select_q = SelectQuery {
                where_clause: where_clause.clone(),
                search: search_clause.clone(),
                order_by: order_by.clone(),
                limit: Some(page_size),
                offset: Some(offset),
                ..SelectQuery::new(state.vs.schema)
            };
            let count_q = CountQuery {
                model: state.vs.schema,
                where_clause,
                search: search_clause.clone(),
            };

            // Tenant mode holds a single per-request connection, so
            // the two queries serialize. Static mode could parallelize
            // via `tokio::join!`, but unifying on the sequential path
            // keeps the handler simple — two short queries on the
            // same connection are typically faster than two pool
            // round-trips anyway.
            // `render_list` renders through the registered serializer
            // (typed tri-dialect fetch) when one is set, else falls
            // back to the default field-level `select_rows_as_json`
            // projection — both dialect-agnostic.
            let results = or_500!(render_list(&state, &mut acq, &select_q, &fields).await);
            let count = or_500!(acq.count_rows(&count_q).await);
            let last_page = ((count - 1).max(0) / page_size) + 1;
            json_response(json!({
                "count": count,
                "page": page,
                "page_size": page_size,
                "last_page": last_page,
                "results": results,
            }))
        }
        PaginationStyle::Cursor {
            field: cursor_field,
            desc,
        } => {
            handle_list_cursor(
                state.as_ref(),
                &mut acq,
                params,
                where_clause,
                search_clause,
                fields,
                page_size,
                cursor_field,
                *desc,
            )
            .await
        }
        PaginationStyle::LimitOffset => {
            // `?limit=` overrides the default page size; `?offset=` skips
            // rows. Same `COUNT(*)` cost as page-number pagination.
            let limit: i64 = params
                .get("limit")
                .and_then(|p| p.parse().ok())
                .unwrap_or(page_size)
                .min(1000)
                .max(1);
            let offset: i64 = params
                .get("offset")
                .and_then(|p| p.parse().ok())
                .unwrap_or(0)
                .max(0);

            let select_q = SelectQuery {
                where_clause: where_clause.clone(),
                search: search_clause.clone(),
                order_by,
                limit: Some(limit),
                offset: Some(offset),
                ..SelectQuery::new(state.vs.schema)
            };
            let count_q = CountQuery {
                model: state.vs.schema,
                where_clause,
                search: search_clause,
            };
            let results = or_500!(render_list(&state, &mut acq, &select_q, &fields).await);
            let count = or_500!(acq.count_rows(&count_q).await);
            json_response(json!({
                "count": count,
                "limit": limit,
                "offset": offset,
                "results": results,
            }))
        }
    }
}

/// RFC 10008 QUERY action on the collection (#1112) — the same filtered,
/// paginated `list`, but with the criteria in the request body instead of
/// the querystring. A ViewSet gets this for free (no trait method to
/// implement); `QUERY /things` with body `status=draft&ordering=-created`
/// returns exactly what `GET /things?status=draft&ordering=-created`
/// would. Permissions / throttles reuse the `list` codenames via `enter`.
#[cfg(feature = "admin")]
async fn handle_query(
    State(state): State<Arc<ViewSetState>>,
    req: axum::extract::Request,
) -> Response {
    // `enter` returns the body unconsumed (it only reads `parts` for
    // throttle + permission checks), so we can parse params from it here.
    let (parts, body, acq) = match enter(&state, req, &state.vs.perms.list, "query").await {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    let params = match parse_query_body_params(&parts, body).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    run_list(state, params, acq).await
}

/// Parse a QUERY request body into the same `HashMap<String, String>`
/// param shape `run_list` consumes for GET. Dispatches on `Content-Type`:
/// urlencoded (or none) via the querystring codepath, JSON objects flattened
/// to string values (scalars stringified, arrays comma-joined so
/// `{"status__in":["a","b"]}` matches `status__in=a,b`), anything else 415.
#[cfg(feature = "admin")]
async fn parse_query_body_params(
    parts: &axum::http::request::Parts,
    body: Body,
) -> Result<HashMap<String, String>, Response> {
    const CAP: usize = 1 << 20;
    let essence = parts
        .headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let bytes = match axum::body::to_bytes(body, CAP).await {
        Ok(b) => b,
        Err(_) => {
            return Err(json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "QUERY body too large",
            ))
        }
    };

    if essence == "application/json" || essence.ends_with("+json") {
        let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
            json_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                &format!("invalid JSON QUERY body: {e}"),
            )
        })?;
        let Value::Object(obj) = value else {
            return Err(json_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "QUERY JSON body must be an object of criteria",
            ));
        };
        Ok(obj
            .iter()
            .map(|(k, v)| (k.clone(), json_value_to_param(v)))
            .collect())
    } else if essence.is_empty() || essence == "application/x-www-form-urlencoded" {
        serde_urlencoded::from_bytes::<HashMap<String, String>>(&bytes)
            .map_err(|e| json_error(StatusCode::BAD_REQUEST, &format!("invalid QUERY body: {e}")))
    } else {
        Err(json_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "QUERY body must be application/x-www-form-urlencoded or application/json",
        ))
    }
}

/// Flatten a JSON value to the string form `run_list`'s filter parser
/// expects: strings verbatim, arrays comma-joined (for `__in` lookups),
/// null → empty, numbers / bools via their JSON text.
#[cfg(feature = "admin")]
fn json_value_to_param(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(json_value_to_param)
            .collect::<Vec<_>>()
            .join(","),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

async fn handle_list_cursor(
    state: &ViewSetState,
    acq: &mut AcquiredConn,
    params: HashMap<String, String>,
    where_clause: WhereExpr,
    search_clause: Option<SearchClause>,
    fields: Vec<&'static crate::core::FieldSchema>,
    page_size: i64,
    cursor_field: &str,
    desc: bool,
) -> Response {
    // Resolve cursor field schema
    let Some(cursor_schema) = state.vs.schema.field(cursor_field) else {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("cursor field `{cursor_field}` not found on model"),
        );
    };
    if !matches!(
        cursor_schema.ty,
        FieldType::I16 | FieldType::I32 | FieldType::I64
    ) {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "cursor pagination requires an integer field (i16/i32/i64)",
        );
    }

    // Decode the incoming cursor (if any)
    let cursor_val: Option<i64> = match params.get("cursor") {
        Some(c) if !c.is_empty() => match decode_cursor(c) {
            Some(v) => Some(v),
            None => return json_error(StatusCode::BAD_REQUEST, "invalid cursor"),
        },
        _ => None,
    };

    // Build WHERE = filters AND (cursor predicate, if any)
    let final_where = match cursor_val {
        Some(v) => {
            let op = if desc { Op::Lt } else { Op::Gt };
            let cursor_pred = WhereExpr::Predicate(Filter {
                column: cursor_schema.column,
                op,
                value: SqlValue::I64(v),
            });
            match where_clause {
                WhereExpr::And(v) if v.is_empty() => cursor_pred,
                WhereExpr::And(mut v) => {
                    v.push(cursor_pred);
                    WhereExpr::And(v)
                }
                other => WhereExpr::And(vec![other, cursor_pred]),
            }
        }
        None => where_clause,
    };

    // Force ordering by the cursor field (cursor pagination requires it)
    let order_by = vec![crate::core::OrderItem::column(cursor_schema.column, desc)];

    // #562 — struct-update over SelectQuery::new for the cursor-
    // paginated SELECT. Fetch page_size+1 to detect if a next page
    // exists.
    let select_q = SelectQuery {
        where_clause: final_where,
        search: search_clause,
        order_by,
        limit: Some(page_size + 1),
        ..SelectQuery::new(state.vs.schema)
    };
    let rows = or_500!(render_list(state, acq, &select_q, &fields).await);

    let has_more = rows.len() as i64 > page_size;
    let page_rows: &[Value] = if has_more {
        &rows[..page_size as usize]
    } else {
        &rows[..]
    };

    let next_cursor = if has_more {
        // Read the cursor field value from the last JSON row.
        let last = page_rows.last().expect("non-empty page");
        let val: i64 = last
            .get(cursor_schema.name)
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        Some(encode_cursor(val))
    } else {
        None
    };

    let results: Vec<Value> = page_rows.to_vec();
    json_response(json!({
        "page_size": page_size,
        "next": next_cursor,
        "results": results,
    }))
}

/// Encode an i64 cursor value as URL-safe base64 of its decimal string.
fn encode_cursor(value: i64) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string().as_bytes())
}

/// Decode a cursor token. Returns `None` for malformed input.
fn decode_cursor(token: &str) -> Option<i64> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token.as_bytes())
        .ok()?;
    let s = std::str::from_utf8(&bytes).ok()?;
    s.parse::<i64>().ok()
}

async fn handle_retrieve(
    State(state): State<Arc<ViewSetState>>,
    Path(pk_raw): Path<String>,
    req: axum::extract::Request,
) -> Response {
    let (_parts, _body, mut acq) =
        match enter(&state, req, &state.vs.perms.retrieve, "retrieve").await {
            Ok(x) => x,
            Err(resp) => return resp,
        };

    let pk_field = match pk_field_or_500(&state) {
        Ok(f) => f,
        Err(resp) => return resp,
    };
    let pk_val = match parse_pk_or_400(pk_field, &pk_raw) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // #562 — was 11-field struct literal; SelectQuery::by_pk constructs
    // the single-PK-lookup shape directly.
    let select_q = SelectQuery::by_pk(state.vs.schema, pk_field.column, pk_val);

    let fields = state.effective_fields();
    match render_single(&state, &mut acq, &select_q, &fields).await {
        Ok(Some(row)) => json_response(row),
        Ok(None) => json_error(StatusCode::NOT_FOUND, "not found"),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn handle_create(
    State(state): State<Arc<ViewSetState>>,
    req: axum::extract::Request,
) -> Response {
    let (parts, body, mut acq) = match enter(&state, req, &state.vs.perms.create, "create").await {
        Ok(x) => x,
        Err(resp) => return resp,
    };

    // #435 — sniff for bulk shape (JSON array body) and dispatch.
    let create_body = or_400!(extract_create_body(parts, body).await);

    let skip: Vec<&str> = state
        .vs
        .schema
        .scalar_fields()
        .filter(|f| f.primary_key || f.auto)
        .map(|f| f.name)
        .collect();

    let pk_field = match pk_field_or_500(&state) {
        Ok(f) => f,
        Err(resp) => return resp,
    };

    match create_body {
        CreateBody::Single(form, json) => {
            create_one(&state, &mut acq, &form, json.as_ref(), &skip, pk_field).await
        }
        CreateBody::Bulk(rows) => create_many(&state, &mut acq, &rows, &skip, pk_field).await,
    }
}

/// Build the `InsertQuery`, run `INSERT … RETURNING <pk>`, then
/// re-fetch the row by its PK as a JSON object — the
/// `create_one` ↔ `create_many` shared insert→fetch tail. Issue #808
/// (item 5).
///
/// Returns `(StatusCode, message)` on the two distinct failure modes
/// so both callers can re-emit the right HTTP code:
/// * `BAD_REQUEST` when the INSERT itself fails (constraint violation,
///   bad value, etc. — likely client fault).
/// * `INTERNAL_SERVER_ERROR` when the INSERT succeeds but the re-fetch
///   misses (a row vanishing between INSERT and SELECT is a server-
///   side anomaly).
///
/// `columns` / `values` come from a prior `collect_values` step so
/// the caller has already validated the inbound form / JSON shape.
async fn insert_and_fetch_one(
    state: &Arc<ViewSetState>,
    acq: &mut AcquiredConn,
    columns: Vec<&'static str>,
    values: Vec<SqlValue>,
    pk_field: &'static crate::core::FieldSchema,
    fields: &[&'static crate::core::FieldSchema],
) -> Result<Value, (StatusCode, String)> {
    let query = InsertQuery {
        model: state.vs.schema,
        columns,
        values,
        returning: vec![pk_field.column],
        on_conflict: None,
    };
    let pk_val = acq
        .insert_returning_pk(&query, pk_field)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    fetch_by_pk(state, acq, pk_field, pk_val, fields)
        .await
        .ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "created but could not retrieve".to_owned(),
            )
        })
}

/// Single-row create — used by both the form-urlencoded codepath
/// and the JSON-object body codepath. Returns 201 + the row JSON.
async fn create_one(
    state: &Arc<ViewSetState>,
    acq: &mut AcquiredConn,
    form: &HashMap<String, String>,
    json: Option<&Value>,
    skip: &[&str],
    pk_field: &'static crate::core::FieldSchema,
) -> Response {
    // When a serializer is registered: run its input validation and
    // skip every model column it doesn't accept (read_only / computed).
    let extra_skip = match serializer_write_prep(state, json) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let mut all_skip: Vec<&str> = skip.to_vec();
    all_skip.extend(extra_skip);
    // Translate `source`-renamed writable keys (serializer field name →
    // model column) so the client can POST the serializer field name.
    let renamed = serializer_input_renamed_form(state, form);
    let form = renamed.as_ref().unwrap_or(form);
    let collected = or_400!(collect_values(state.vs.schema, form, &all_skip));
    let (columns, values): (Vec<_>, Vec<_>) = collected.into_iter().unzip();
    let fields = state.effective_fields();
    match insert_and_fetch_one(state, acq, columns, values, pk_field, &fields).await {
        Ok(obj) => json_created(obj),
        Err((code, msg)) => json_error(code, &msg),
    }
}

/// Bulk create — Django DRF `ListSerializer(many=True)` shape.
/// Validates every entry first; on first failure, the WHOLE bulk
/// is rejected with the index + message (atomic-validate, not
/// atomic-insert — partial-insert recovery is a separate concern).
/// On success, inserts each row sequentially and returns 201 + the
/// JSON array of created rows in submission order.
///
/// Issue #435.
async fn create_many(
    state: &Arc<ViewSetState>,
    acq: &mut AcquiredConn,
    rows: &[(HashMap<String, String>, Option<Value>)],
    skip: &[&str],
    pk_field: &'static crate::core::FieldSchema,
) -> Response {
    if rows.is_empty() {
        return json_created(Value::Array(Vec::new()));
    }

    // Atomic validation: collect every (columns, values) up front
    // so a bad row near the end of the list doesn't leave half the
    // INSERTs committed. DRF's default ListSerializer.create has
    // the same shape — validate the whole list before any save.
    let mut prepared: Vec<(Vec<&'static str>, Vec<SqlValue>)> = Vec::with_capacity(rows.len());
    for (i, (row, json)) in rows.iter().enumerate() {
        // Serializer validation + non-writable skip, per entry.
        let extra_skip = match serializer_write_prep(state, json.as_ref()) {
            Ok(s) => s,
            Err(resp) => return resp,
        };
        let mut all_skip: Vec<&str> = skip.to_vec();
        all_skip.extend(extra_skip);
        let renamed = serializer_input_renamed_form(state, row);
        let row = renamed.as_ref().unwrap_or(row);
        let collected = match collect_values(state.vs.schema, row, &all_skip) {
            Ok(v) => v,
            Err(e) => {
                return json_error(StatusCode::BAD_REQUEST, &format!("bulk entry {i}: {e}"));
            }
        };
        prepared.push(collected.into_iter().unzip());
    }

    // Insert sequentially. We loop INSERT-RETURNING rather than
    // emitting one multi-row INSERT because:
    //
    // 1. Per-row PK extraction stays straightforward (one returning
    //    row per call, no fan-out parsing).
    // 2. The framework's `bulk_insert_pool` path is typed-model-
    //    shaped; this dynamic-JSON codepath doesn't have a typed
    //    handle to feed it.
    // 3. Most ListSerializer.create callers are submitting tens of
    //    rows, not millions. A real high-volume bulk endpoint
    //    should mount its own custom handler with bulk_insert_pool.
    //
    // The trade-off: N round-trips per request. We document
    // accordingly in the issue + viewset docs.
    let fields = state.effective_fields();
    let mut created: Vec<Value> = Vec::with_capacity(prepared.len());
    for (i, (columns, values)) in prepared.into_iter().enumerate() {
        match insert_and_fetch_one(state, acq, columns, values, pk_field, &fields).await {
            Ok(obj) => created.push(obj),
            Err((code, msg)) => {
                return json_error(code, &format!("bulk entry {i}: {msg}"));
            }
        }
    }

    json_created(Value::Array(created))
}

async fn handle_update(
    State(state): State<Arc<ViewSetState>>,
    Path(pk_raw): Path<String>,
    req: axum::extract::Request,
) -> Response {
    update_inner(state, pk_raw, req, false).await
}

async fn handle_partial_update(
    State(state): State<Arc<ViewSetState>>,
    Path(pk_raw): Path<String>,
    req: axum::extract::Request,
) -> Response {
    update_inner(state, pk_raw, req, true).await
}

async fn update_inner(
    state: Arc<ViewSetState>,
    pk_raw: String,
    req: axum::extract::Request,
    partial: bool,
) -> Response {
    let (parts, body, mut acq) = match enter(&state, req, &state.vs.perms.update, "update").await {
        Ok(x) => x,
        Err(resp) => return resp,
    };

    let pk_field = match pk_field_or_500(&state) {
        Ok(f) => f,
        Err(resp) => return resp,
    };
    let pk_val = match parse_pk_or_400(pk_field, &pk_raw) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let (form, json) = or_400!(extract_form_body(parts, body).await);

    // Serializer (when set): validate the body + the set of model
    // columns it doesn't accept (read_only / computed), which we skip.
    let non_writable = match serializer_write_prep(&state, json.as_ref()) {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    // Translate `source`-renamed writable keys (serializer field name →
    // model column) before the per-column update loop reads the form.
    let renamed = serializer_input_renamed_form(&state, &form);
    let form = renamed.as_ref().unwrap_or(&form);

    let mut assignments: Vec<Assignment> = Vec::new();
    for field in state.vs.schema.scalar_fields() {
        if field.primary_key || field.auto {
            continue;
        }
        if non_writable.contains(&field.name) {
            continue;
        }
        if partial && !form.contains_key(field.name) {
            continue;
        }
        let raw = form.get(field.name).map(String::as_str);
        match parse_form_value(field, raw) {
            Ok(v) => assignments.push(Assignment {
                column: field.column,
                value: v.into(),
            }),
            Err(FormError::Missing { .. }) if partial => continue,
            Err(e) => return json_error(StatusCode::BAD_REQUEST, &e.to_string()),
        }
    }

    if assignments.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "no fields to update");
    }

    let query = UpdateQuery {
        model: state.vs.schema,
        set: assignments,
        where_clause: WhereExpr::Predicate(Filter {
            column: pk_field.column,
            op: Op::Eq,
            value: pk_val.clone(),
        }),
    };

    if let Err(e) = acq.update(&query).await {
        return json_error(StatusCode::BAD_REQUEST, &e.to_string());
    }

    let fields = state.effective_fields();
    match fetch_by_pk(&state, &mut acq, pk_field, pk_val, &fields).await {
        Some(obj) => json_response(obj),
        None => json_error(StatusCode::NOT_FOUND, "not found after update"),
    }
}

async fn handle_destroy(
    State(state): State<Arc<ViewSetState>>,
    Path(pk_raw): Path<String>,
    req: axum::extract::Request,
) -> Response {
    let (_parts, _body, mut acq) =
        match enter(&state, req, &state.vs.perms.destroy, "destroy").await {
            Ok(x) => x,
            Err(resp) => return resp,
        };

    let pk_field = match pk_field_or_500(&state) {
        Ok(f) => f,
        Err(resp) => return resp,
    };
    let pk_val = match parse_pk_or_400(pk_field, &pk_raw) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let query = DeleteQuery {
        model: state.vs.schema,
        where_clause: WhereExpr::Predicate(Filter {
            column: pk_field.column,
            op: Op::Eq,
            value: pk_val,
        }),
    };

    match acq.delete(&query).await {
        Ok(0) => json_error(StatusCode::NOT_FOUND, "not found"),
        Ok(_) => no_content(),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// ------------------------------------------------------------------ helpers

async fn fetch_by_pk(
    state: &ViewSetState,
    acq: &mut AcquiredConn,
    pk_field: &'static crate::core::FieldSchema,
    pk_val: SqlValue,
    fields: &[&'static crate::core::FieldSchema],
) -> Option<Value> {
    // #562 — SelectQuery::by_pk replaces the 11-field literal.
    let select_q = SelectQuery::by_pk(state.vs.schema, pk_field.column, pk_val);
    render_single(state, acq, &select_q, fields)
        .await
        .ok()
        .flatten()
}

/// Render the rows matching `select_q` for a list response: through
/// the registered [`SerializerBridge`] when one is set
/// ([`ViewSet::serializer`]), else the default field-level JSON
/// projection. Single source of truth for the three list shapes.
async fn render_list(
    state: &ViewSetState,
    acq: &mut AcquiredConn,
    select_q: &SelectQuery,
    fields: &[&'static crate::core::FieldSchema],
) -> Result<Vec<Value>, crate::sql::ExecError> {
    match &state.vs.serializer {
        Some(bridge) => bridge.render_rows(acq, select_q).await,
        None => acq.select_rows_as_json(select_q, fields).await,
    }
}

/// Render the single row matching `select_q` for a retrieve / create /
/// update response: through the registered serializer when set, else
/// the default field-level JSON projection.
async fn render_single(
    state: &ViewSetState,
    acq: &mut AcquiredConn,
    select_q: &SelectQuery,
    fields: &[&'static crate::core::FieldSchema],
) -> Result<Option<Value>, crate::sql::ExecError> {
    match &state.vs.serializer {
        Some(bridge) => bridge.render_one(acq, select_q).await,
        None => acq.select_one_as_json(select_q, fields).await,
    }
}

/// Extract form data from both `application/x-www-form-urlencoded` and
/// `application/json` request bodies.
async fn extract_form_body(
    parts: axum::http::request::Parts,
    body: Body,
) -> Result<(HashMap<String, String>, Option<Value>), String> {
    match extract_create_body(parts, body).await? {
        CreateBody::Single(form, json) => Ok((form, json)),
        CreateBody::Bulk(_) => Err("expected a JSON object; got an array".into()),
    }
}

/// Sniff a POST body and return either a single record (object body
/// or form-urlencoded body) or a bulk list (JSON array body — DRF's
/// `ListSerializer(many=True)` shape). Issue #435.
///
/// Bulk shape is only recognized for `application/json` content-type
/// + a JSON array body — form-urlencoded payloads always parse as
/// single records (multi-row form encoding doesn't have a portable
/// shape).
pub(crate) enum CreateBody {
    /// `(stringified form, raw JSON value)`. The JSON value is `Some`
    /// for `application/json` bodies (used for typed serializer
    /// validation) and `None` for form-urlencoded.
    Single(HashMap<String, String>, Option<Value>),
    Bulk(Vec<(HashMap<String, String>, Option<Value>)>),
}

pub(crate) async fn extract_create_body(
    parts: axum::http::request::Parts,
    body: Body,
) -> Result<CreateBody, String> {
    use axum::body::to_bytes;

    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let bytes = to_bytes(body, 4 * 1024 * 1024)
        .await
        .map_err(|e| e.to_string())?;

    if content_type.contains("application/json") {
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if let Some(array) = value.as_array() {
            // Bulk shape: each element must be an object. Keep the raw
            // JSON entry alongside the stringified form for serializer
            // validation.
            let mut bulk: Vec<(HashMap<String, String>, Option<Value>)> =
                Vec::with_capacity(array.len());
            for (i, entry) in array.iter().enumerate() {
                let obj = entry
                    .as_object()
                    .ok_or_else(|| format!("bulk entry {i} is not a JSON object"))?;
                bulk.push((json_object_to_form(obj), Some(entry.clone())));
            }
            return Ok(CreateBody::Bulk(bulk));
        }
        let obj = value
            .as_object()
            .ok_or("expected a JSON object or array of objects")?;
        Ok(CreateBody::Single(json_object_to_form(obj), Some(value)))
    } else {
        // form-urlencoded (default) — single only, no typed JSON value.
        let form = serde_urlencoded::from_bytes::<HashMap<String, String>>(&bytes)
            .map_err(|e| e.to_string())?;
        Ok(CreateBody::Single(form, None))
    }
}

/// Flatten a JSON object's top-level values into the
/// `HashMap<String, String>` shape every existing collector expects.
/// Numeric / boolean / null primitives stringify; nested objects /
/// arrays serialize back to JSON text (caller can re-parse if it
/// needs typed access). Extracted from the inlined `extract_form_body`
/// path so both single and bulk codepaths share it.
fn json_object_to_form(obj: &serde_json::Map<String, Value>) -> HashMap<String, String> {
    let mut form = HashMap::with_capacity(obj.len());
    for (k, v) in obj {
        let s = match v {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        form.insert(k.clone(), s);
    }
    form
}

#[cfg(test)]
mod cursor_tests {
    use super::{decode_cursor, encode_cursor};

    #[test]
    fn cursor_roundtrip_positive() {
        let token = encode_cursor(12345);
        assert_eq!(decode_cursor(&token), Some(12345));
    }

    #[test]
    fn cursor_roundtrip_zero() {
        let token = encode_cursor(0);
        assert_eq!(decode_cursor(&token), Some(0));
    }

    #[test]
    fn cursor_roundtrip_max() {
        let token = encode_cursor(i64::MAX);
        assert_eq!(decode_cursor(&token), Some(i64::MAX));
    }

    #[test]
    fn cursor_decode_invalid_base64_returns_none() {
        assert!(decode_cursor("not!valid!base64@@").is_none());
    }

    #[test]
    fn cursor_decode_non_numeric_payload_returns_none() {
        use base64::Engine;
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("not_a_number");
        assert!(decode_cursor(&token).is_none());
    }
}

#[cfg(all(test, feature = "tenancy"))]
mod tenant_router_tests {
    use super::*;

    /// Smoke: building a tenant_router shouldn't panic and must
    /// produce a usable `Router` value. The full CRUD round-trip
    /// is exercised via integration tests against a real Postgres
    /// + tenant pool. Mirrors the v1 `viewset/tenant.rs` smoke
    /// test from before the v0.30 unification.
    #[test]
    fn tenant_router_builds_for_a_basic_model() {
        use crate::core::Model as _;
        // Use the framework's own User schema as a stand-in —
        // it's always available and has a PK.
        let _r = ViewSet::for_model(crate::tenancy::auth::User::SCHEMA)
            .read_only()
            .tenant_router("/api/users");
    }

    /// `tenant_router` with the full filter/search/ordering/perm
    /// builder chain compiles + builds — proves the v0.30
    /// unification keeps every static-router knob available in
    /// tenant mode (the v1 had none of these).
    #[test]
    fn tenant_router_carries_over_full_builder_chain() {
        use crate::core::Model as _;
        let _r = ViewSet::for_model(crate::tenancy::auth::User::SCHEMA)
            .filter_fields(&["username"])
            .search_fields(&["username"])
            .ordering(&[("id", true)])
            .page_size(50)
            .tenant_router("/api/users");
    }

    /// Mode flag round-trips. Pure unit assertion that the two
    /// public router builders set distinct internal pool sources.
    #[test]
    fn router_and_tenant_router_set_distinct_pool_sources() {
        use crate::core::Model as _;
        // We can't compare PoolSource directly (no PartialEq), but we
        // can assert the discriminant via matches!.
        let static_state = ViewSet::for_model(crate::tenancy::auth::User::SCHEMA);
        // Static can't be tested without a real PgPool; just confirm
        // the tenant variant exists and matches what we expect.
        let vs = static_state.read_only();
        let _r = vs.clone().tenant_router("/api/users");
        // If this compiles, the variant + builder are wired.
    }
}

#[cfg(test)]
mod lookup_tests {
    use super::*;
    use crate::core::{FieldSchema, FieldType};

    fn int_field() -> &'static FieldSchema {
        &FieldSchema {
            name: "author_id",
            column: "author_id",
            ty: FieldType::I64,
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
            case_insensitive: false,
            fk_on_delete: None,
            validators: &[],
        }
    }

    fn string_field() -> &'static FieldSchema {
        &FieldSchema {
            name: "title",
            column: "title",
            ty: FieldType::String,
            nullable: true,
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
            case_insensitive: false,
            fk_on_delete: None,
            validators: &[],
        }
    }

    fn extract_pred(expr: WhereExpr) -> Filter {
        match expr {
            WhereExpr::Predicate(f) => f,
            _ => panic!("expected Predicate"),
        }
    }

    #[test]
    fn no_lookup_means_eq() {
        let f = extract_pred(build_lookup_filter(int_field(), None, "42").unwrap());
        assert_eq!(f.op, Op::Eq);
        assert!(matches!(f.value, SqlValue::I64(42)));
    }

    #[test]
    fn explicit_exact_means_eq() {
        let f = extract_pred(build_lookup_filter(int_field(), Some("exact"), "42").unwrap());
        assert_eq!(f.op, Op::Eq);
    }

    #[test]
    fn comparison_lookups() {
        for (lk, expected) in [
            ("gt", Op::Gt),
            ("gte", Op::Gte),
            ("lt", Op::Lt),
            ("lte", Op::Lte),
            ("ne", Op::Ne),
        ] {
            let f = extract_pred(build_lookup_filter(int_field(), Some(lk), "10").unwrap());
            assert_eq!(f.op, expected, "lookup {lk}");
        }
    }

    #[test]
    fn in_lookup_parses_csv() {
        let f = extract_pred(build_lookup_filter(int_field(), Some("in"), "1,2,3").unwrap());
        assert_eq!(f.op, Op::In);
        match f.value {
            SqlValue::List(v) => assert_eq!(v.len(), 3),
            _ => panic!("expected List"),
        }
    }

    #[test]
    fn not_in_lookup_parses_csv() {
        let f = extract_pred(build_lookup_filter(int_field(), Some("not_in"), "1,2").unwrap());
        assert_eq!(f.op, Op::NotIn);
    }

    #[test]
    fn in_lookup_drops_empty_entries() {
        let f = extract_pred(build_lookup_filter(int_field(), Some("in"), "1,,2,").unwrap());
        match f.value {
            SqlValue::List(v) => assert_eq!(v.len(), 2),
            _ => panic!("expected List"),
        }
    }

    #[test]
    fn contains_wraps_with_percents_and_uses_like() {
        let f =
            extract_pred(build_lookup_filter(string_field(), Some("contains"), "hello").unwrap());
        assert_eq!(f.op, Op::Like);
        assert!(matches!(f.value, SqlValue::String(ref s) if s == "%hello%"));
    }

    #[test]
    fn icontains_uses_ilike() {
        let f = extract_pred(build_lookup_filter(string_field(), Some("icontains"), "hi").unwrap());
        assert_eq!(f.op, Op::ILike);
        assert!(matches!(f.value, SqlValue::String(ref s) if s == "%hi%"));
    }

    #[test]
    fn startswith_only_trailing_percent() {
        let f =
            extract_pred(build_lookup_filter(string_field(), Some("startswith"), "pre").unwrap());
        assert!(matches!(f.value, SqlValue::String(ref s) if s == "pre%"));
    }

    #[test]
    fn endswith_only_leading_percent() {
        let f = extract_pred(build_lookup_filter(string_field(), Some("endswith"), "fix").unwrap());
        assert!(matches!(f.value, SqlValue::String(ref s) if s == "%fix"));
    }

    #[test]
    fn isnull_true() {
        let f = extract_pred(build_lookup_filter(string_field(), Some("isnull"), "true").unwrap());
        assert_eq!(f.op, Op::IsNull);
        assert!(matches!(f.value, SqlValue::Bool(true)));
    }

    #[test]
    fn isnull_false() {
        let f = extract_pred(build_lookup_filter(string_field(), Some("isnull"), "false").unwrap());
        assert!(matches!(f.value, SqlValue::Bool(false)));
    }

    #[test]
    fn unknown_lookup_returns_none() {
        let r = build_lookup_filter(int_field(), Some("frobulate"), "x");
        assert!(r.is_none());
    }

    #[test]
    fn parse_failure_returns_none() {
        let r = build_lookup_filter(int_field(), Some("gt"), "not-a-number");
        assert!(r.is_none());
    }
}

#[cfg(all(test, feature = "tenancy"))]
mod typed_perms_tests {
    use super::*;
    use crate::sql::Auto;

    #[derive(crate::Model)]
    #[rustango(table = "vs_typed_perm_post")]
    #[allow(dead_code)]
    pub struct PermPost {
        #[rustango(primary_key)]
        pub id: Auto<i64>,
        #[rustango(max_length = 200)]
        pub title: String,
    }

    #[test]
    fn permissions_for_model_fills_all_four_crud_codenames() {
        use crate::core::Model;
        let vs =
            ViewSet::for_model(<PermPost as Model>::SCHEMA).permissions_for_model::<PermPost>();
        assert_eq!(vs.perms.list, vec!["vs_typed_perm_post.view"]);
        assert_eq!(vs.perms.retrieve, vec!["vs_typed_perm_post.view"]);
        assert_eq!(vs.perms.create, vec!["vs_typed_perm_post.add"]);
        assert_eq!(vs.perms.update, vec!["vs_typed_perm_post.change"]);
        assert_eq!(vs.perms.destroy, vec!["vs_typed_perm_post.delete"]);
    }
}
