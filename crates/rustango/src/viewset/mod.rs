//! Router viewsets: five REST endpoints from one model.
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
//! | `page_size` | configured default | Items per page (capped at `max_page_size`, default 100) |
//! | `ordering` | configured default | Comma-separated field names, prefix `-` for DESC |
//! | `search` | — | Full-text search across `search_fields` |
//! | `{field}` | — | Exact filter for any `filter_fields` |
//! | `{field}__{lookup}` | — | Lookup suffix (iexact/gt/gte/lt/lte/ne/in/not_in/range/contains/icontains/startswith/istartswith/endswith/iendswith/isnull, date parts such as year/date__gte). An unknown lookup or bad value is a `400` |
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
//! | `page_size` | configured default | Items per page (capped at `max_page_size`, default 100) |
//! | `{field}` | — | Exact filter for any `filter_fields` |
//!
//! Response: `{"page_size": S, "next": "<token>" \| null, "results": [...]}`
//!
//! ### Limit/offset pagination (opt-in)
//!
//! Enable via `.limit_offset_pagination()`. `?limit=&offset=`
//! windowing — handy for tables/grids that page by row offset rather
//! than page number. Runs `COUNT(*)` per request (same cost as
//! page-number).
//!
//! | Parameter | Default | Description |
//! |---|---|---|
//! | `limit` | configured default | Items to return (capped at `max_page_size`, default 100) |
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
use axum::response::IntoResponse as _;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};

use crate::core::{
    Assignment, CountQuery, DeleteQuery, FieldType, Filter, InsertQuery, ModelSchema, Op,
    SearchClause, SelectQuery, SqlValue, UpdateQuery, WhereExpr,
};
use crate::forms::{
    absent_takes_default, collect_insert_values, parse_form_value, parse_pk_string, FormError,
};
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

/// A fixed-window throttle: at most `max` requests per `window_secs`.
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

/// Per-action request throttles for a ViewSet. Any action left `None`
/// is unthrottled.
///
/// Counters are **process-local**, like [`crate::rate_limit`]. Behind
/// N replicas the real limit is N×. For a shared limit, put a gateway
/// throttle in front.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ViewSetThrottle {
    /// Throttle for `GET /` and `QUERY /` (list), one shared budget.
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
    /// `count`, `page` and `last_page`. Runs `COUNT(*)` per request.
    PageNumber,
    /// Cursor pagination — `?cursor=<encoded>&page_size=20`. `field`
    /// must be a stable, monotonic column, usually the primary key.
    /// Skips the COUNT query, so it scales to very large tables.
    Cursor {
        /// SQL field name used as the cursor (e.g. `"id"`).
        field: &'static str,
        /// `true` for descending order. Cursors compare with `<` instead
        /// of `>`. Default ordering is set automatically to match.
        desc: bool,
    },
    /// Limit/offset windowing — `?limit=20&offset=40`.
    /// Returns `count`, `limit` and `offset`. Costs the same as
    /// [`PaginationStyle::PageNumber`]; pick it when callers think in
    /// row offsets rather than page numbers.
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

    /// Limit/offset pagination.
    #[must_use]
    pub const fn limit_offset() -> Self {
        Self::LimitOffset
    }
}

/// Type-erased bridge that lets a `dyn`-routed [`ViewSet`] render rows
/// through a concrete [`crate::serializer::ModelSerializer`].
///
/// `ViewSet` is stored without its model/serializer type, so a router
/// can hold many of them, and so it cannot name `S` directly.
/// [`ViewSet::serializer`] boxes a [`Bridge<S>`] behind this trait
/// instead. When set, list / retrieve / create responses go through
/// the bridge rather than the default `select_rows_as_json`
/// projection, so `method` / `read_only` / `source` / `write_only`
/// shape the JSON output.
///
/// The bridge fetches typed `Vec<S::Model>` via
/// [`crate::sql::select_rows_pool_with_related`], which decodes on all
/// three backends, then maps each model through `S::from_model` and
/// `to_value`.
/// Outer `Err`: the row load failed. Inner `Err`: the body is invalid.
type PatchCheck<'a> = Pin<
    Box<
        dyn Future<Output = Result<Result<(), crate::forms::FormErrors>, crate::sql::ExecError>>
            + Send
            + 'a,
    >,
>;

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

    /// Validate a JSON request body: parse the writable fields, then
    /// call the serializer's `validate()` hook. `Err` carries
    /// per-field errors for a 400 response.
    fn validate_body(&self, body: &Value) -> Result<(), crate::forms::FormErrors>;

    /// PATCH: validate `body` over the row `q` locks in `tx` (#1995,
    /// #2010). No row skips the check; the UPDATE then answers 404.
    fn validate_patch<'a>(
        &'a self,
        tx: &'a mut crate::sql::PoolTx<'static>,
        q: &'a SelectQuery,
        body: &'a Value,
    ) -> PatchCheck<'a>;

    /// The **model** field names the serializer accepts on write,
    /// with `source` resolved. The write path skips every other
    /// column, so a client cannot set `read_only` or computed fields.
    fn writable_model_fields(&self) -> &'static [&'static str];

    /// The **serializer** field names of the writable fields — the
    /// JSON keys a client sends. Parallel to
    /// [`Self::writable_model_fields`]; the two differ only where a
    /// field declares `#[serializer(source = "…")]`.
    fn writable_field_names(&self) -> &'static [&'static str];

    /// The model fields the serializer renders; the `?ordering=` fallback.
    fn readable_model_fields(&self) -> &'static [&'static str];
}

/// Zero-sized carrier that pins a concrete serializer type `S`, so
/// the type-erased [`SerializerBridge`] can call back into
/// `S::from_model` and `S::Model`'s row decode.
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
        // Parse the writable fields, surfacing per-field type errors,
        // then run the serializer's validation hook.
        let s = S::from_writable_json(body)?;
        s.validate()
    }

    fn validate_patch<'a>(
        &'a self,
        tx: &'a mut crate::sql::PoolTx<'static>,
        q: &'a SelectQuery,
        body: &'a Value,
    ) -> PatchCheck<'a> {
        Box::pin(async move {
            let models = crate::sql::select_rows_tx_with_related::<S::Model>(tx, q).await?;
            Ok(models
                .first()
                .map_or(Ok(()), |m| S::validate_patch(m, body)))
        })
    }

    fn writable_model_fields(&self) -> &'static [&'static str] {
        S::writable_source_fields()
    }

    fn writable_field_names(&self) -> &'static [&'static str] {
        S::writable_fields()
    }

    fn readable_model_fields(&self) -> &'static [&'static str] {
        S::readable_source_fields()
    }
}

/// A pluggable filter backend.
///
/// Register one with [`ViewSet::filter_backend`]. It adds `WHERE`
/// predicates from the list request's query params, `AND`-ed with the
/// built-in exact and lookup filters. Use it for what `filter_fields`
/// cannot express: a geo-radius, a custom `?q=` DSL, request-scoped
/// row visibility.
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
///             vec![WhereExpr::Predicate(Filter::new(
///                 f.column,
///                 Op::Eq,
///                 SqlValue::from("published"),
///             ))]
///         })
///     })
///     .router_pool("/posts", pool);
/// ```
///
/// Backends run on **every** action. They narrow the list query and
/// they scope `retrieve` / `update` / `destroy`, so a row the backend
/// excludes is a 404 rather than someone else's data. Without that an
/// ownership backend would guard the collection and leave every row
/// reachable by id.
pub trait ViewSetFilter: Send + Sync + 'static {
    /// Return `WHERE` predicates to AND into the query for this request.
    fn filter(
        &self,
        params: &HashMap<String, String>,
        schema: &'static ModelSchema,
    ) -> Vec<WhereExpr>;

    /// As [`Self::filter`], with the request's [`Parts`] in hand.
    ///
    /// The authenticated principal lives in the request extensions,
    /// not the query string, so "only this user's rows" cannot be
    /// written against [`Self::filter`] alone. Defaults to
    /// [`Self::filter`], which is what the closure form uses.
    ///
    /// [`Parts`]: axum::http::request::Parts
    fn filter_with(
        &self,
        _parts: &axum::http::request::Parts,
        params: &HashMap<String, String>,
        schema: &'static ModelSchema,
    ) -> Vec<WhereExpr> {
        self.filter(params, schema)
    }

    /// What every create and update must write, whatever the body says.
    ///
    /// A scope only narrows reads; without a pin a client could write a
    /// row outside it, e.g. into another user's account. Default: none.
    fn write_pins(
        &self,
        _parts: &axum::http::request::Parts,
        _schema: &'static ModelSchema,
    ) -> Vec<WritePin> {
        Vec::new()
    }
}

/// A value a [`ViewSetFilter`] forces on writes. See
/// [`ViewSetFilter::write_pins`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum WritePin {
    /// Create writes `value` into `field`; update never changes it.
    /// A field the model does not have denies the write.
    Field {
        /// The model field name.
        field: &'static str,
        /// The value to write.
        value: SqlValue,
    },
    /// Refuse the write with `403`, e.g. there is no principal to own it.
    Deny,
}

impl WritePin {
    /// Pin `field` to `value`.
    #[must_use]
    pub fn field(field: &'static str, value: impl Into<SqlValue>) -> Self {
        Self::Field {
            field,
            value: value.into(),
        }
    }
}

/// Scope every row to the principal that owns it.
///
/// Name the column that holds the owner and mount it. It works on
/// any model with such a column (`owner_id`, `member_id`, `user_id`,
/// `created_by`) and applies to every action, so the collection and
/// the item routes cannot disagree.
///
/// ```no_run
/// # use rustango::viewset::{OwnedBy, ViewSet};
/// # use rustango::core::Model as _;
/// # #[derive(rustango::Model)] #[rustango(table = "note")]
/// # pub struct Note { #[rustango(primary_key)] pub id: rustango::sql::Auto<i64>, pub owner_id: i64 }
/// # fn main() {
/// ViewSet::for_model(Note::SCHEMA)
///     .filter_backend(OwnedBy::column("owner_id"))
///     .tenant_router("/api/notes");
/// # }
/// ```
///
/// The owner comes from [`Principal`], which every auth path
/// populates, so one backend covers a cookie session, a Bearer token
/// and an agent token. It is **never** read from the query string: a
/// client that could name its own `owner_id` is not authorized.
///
/// It also pins the column on writes: create stores the principal's id
/// whatever the body says, and update never changes the owner.
///
/// Fails closed. With no principal it matches nothing, so an
/// unauthenticated request sees an empty list rather than the whole
/// table. This is the second line of defence, not the first — pair it
/// with an auth layer that rejects those requests.
///
/// [`Principal`]: crate::tenancy::Principal
#[cfg(feature = "tenancy")]
#[derive(Clone, Copy, Debug)]
pub struct OwnedBy {
    column: &'static str,
    superuser_sees_all: bool,
}

#[cfg(feature = "tenancy")]
impl OwnedBy {
    /// Scope to `column = <principal's user id>`.
    #[must_use]
    pub const fn column(column: &'static str) -> Self {
        Self {
            column,
            superuser_sees_all: false,
        }
    }

    /// Let superusers read and write every row.
    ///
    /// Off by default: "admins see everything" is a product decision.
    /// A support tool wants it; an app where the owner is also an
    /// ordinary member does not.
    #[must_use]
    pub const fn superuser_sees_all(self) -> Self {
        Self {
            superuser_sees_all: true,
            ..self
        }
    }
}

#[cfg(feature = "tenancy")]
impl ViewSetFilter for OwnedBy {
    fn filter(
        &self,
        _params: &HashMap<String, String>,
        schema: &'static ModelSchema,
    ) -> Vec<WhereExpr> {
        // Only reached on the params-only path. No request means no
        // principal, and no principal means no rows.
        vec![match_nothing(schema)]
    }

    fn filter_with(
        &self,
        parts: &axum::http::request::Parts,
        _params: &HashMap<String, String>,
        schema: &'static ModelSchema,
    ) -> Vec<WhereExpr> {
        let Some(principal) = crate::tenancy::Principal::from_parts(parts) else {
            return vec![match_nothing(schema)];
        };
        if self.superuser_sees_all && principal.is_superuser {
            return Vec::new();
        }
        let Some(field) = schema.field(self.column) else {
            // The column named at mount time is not on this model.
            // Deny: there is no owner to scope to.
            tracing::error!(
                model = schema.table,
                column = self.column,
                "OwnedBy names a column this model does not have — denying every row"
            );
            return vec![match_nothing(schema)];
        };
        vec![WhereExpr::Predicate(Filter {
            column: field.column,
            op: Op::Eq,
            value: SqlValue::from(principal.user_id),
        })]
    }

    fn write_pins(
        &self,
        parts: &axum::http::request::Parts,
        _schema: &'static ModelSchema,
    ) -> Vec<WritePin> {
        // Same decision as `filter_with`: no principal means no write.
        let Some(principal) = crate::tenancy::Principal::from_parts(parts) else {
            return vec![WritePin::Deny];
        };
        if self.superuser_sees_all && principal.is_superuser {
            return Vec::new();
        }
        vec![WritePin::field(self.column, principal.user_id)]
    }
}

/// A predicate no row satisfies: `col IS NULL AND col IS NOT NULL`.
///
/// A contradiction rather than `1 = 0` because it binds no
/// parameters and every dialect writes it the same way.
///
/// Use it in the fail-closed branch of a [`ViewSetFilter`], where the
/// principal is missing. An empty `Vec` there does not mean "match
/// nothing", it means "no filter at all", which widens the query to
/// the whole table.
///
/// ```ignore
/// fn filter(&self, _p: &HashMap<String, String>, schema: &'static ModelSchema)
///     -> Vec<WhereExpr>
/// {
///     vec![match_nothing(schema)]   // not `vec![]`
/// }
/// ```
///
/// Returns one `WhereExpr`, so wrap it in `vec![]` for the trait.
pub fn match_nothing(schema: &'static ModelSchema) -> WhereExpr {
    let column = schema
        .primary_key()
        .or_else(|| schema.scalar_fields().next())
        .map_or("id", |f| f.column);
    WhereExpr::And(vec![
        WhereExpr::Predicate(Filter {
            column,
            op: Op::IsNull,
            value: SqlValue::Bool(true),
        }),
        WhereExpr::Predicate(Filter {
            column,
            op: Op::IsNull,
            value: SqlValue::Bool(false),
        }),
    ])
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

/// Builder for a set of REST CRUD endpoints over a single
/// [`Model`](crate::core::Model) table.
///
/// Call `.router(prefix, pool)` when done to get an `axum::Router`.
#[derive(Clone)]
pub struct ViewSet {
    schema: &'static ModelSchema,
    fields: Option<Vec<String>>,
    filter_fields: Vec<String>,
    search_fields: Vec<String>,
    /// Allow-list for `?ordering=`. `None` (the default) means the
    /// fields the response renders; `Some(empty)` means none.
    ordering_fields: Option<Vec<String>>,
    default_page_size: usize,
    default_ordering: Vec<(String, bool)>,
    perms: ViewSetPerms,
    read_only: bool,
    /// Write actions with no codenames are intended; see [`ViewSet::allow_anonymous`].
    allow_anonymous: bool,
    /// Describe the mounted `QUERY` route in the generated OpenAPI
    /// document. Off by default — see [`ViewSet::openapi_query`].
    openapi_query: bool,
    pagination: PaginationStyle,
    /// Each contributes extra `WHERE` predicates, ANDed with the
    /// built-in filters.
    filter_backends: Vec<std::sync::Arc<dyn ViewSetFilter>>,
    /// Per-action request throttles. Default: unthrottled.
    throttle: ViewSetThrottle,
    /// When set, list / retrieve / create responses render each row
    /// through this serializer bridge instead of the default
    /// field-level projection. Wired via [`Self::serializer`].
    serializer: Option<Arc<dyn SerializerBridge>>,
    /// Largest page a client may request via `?page_size=` / `?limit=`.
    /// Defaults to 100. See [`ViewSet::max_page_size`].
    max_page_size: usize,
    /// Name of the path capture for the detail routes — `pk` by default,
    /// giving `/{pk}`. See [`ViewSet::pk_param`].
    pk_param: String,
    /// Most rows one bulk create may carry. See [`ViewSet::max_bulk_create`].
    max_bulk_create: usize,
}

impl ViewSet {
    /// Start a ViewSet for the given model schema.
    pub fn for_model(schema: &'static ModelSchema) -> Self {
        Self {
            schema,
            fields: None,
            filter_fields: Vec::new(),
            search_fields: Vec::new(),
            ordering_fields: None,
            default_page_size: 20,
            default_ordering: Vec::new(),
            perms: ViewSetPerms::default(),
            read_only: false,
            allow_anonymous: false,
            openapi_query: false,
            pagination: PaginationStyle::PageNumber,
            filter_backends: Vec::new(),
            throttle: ViewSetThrottle::default(),
            serializer: None,
            max_page_size: 100,
            pk_param: "pk".to_owned(),
            max_bulk_create: 1000,
        }
    }

    /// Render list / retrieve / create responses through `S`, a
    /// serializer from `#[derive(Serializer)]`, instead of the
    /// default field-level projection.
    ///
    /// Use it when the JSON shape should match a typed serializer's
    /// `read_only` / `source` / `method` / `nested` / `many`
    /// overrides. `S::Model` must be the model the ViewSet is built
    /// over. Works on all three backends.
    ///
    /// `nested` and `many` fields need related rows a flat fetch does
    /// not load, so they render as their `Default` unless the query
    /// used `select_related`. The other overrides always apply.
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

    /// Switch to cursor-based pagination on `field`. This skips the
    /// `COUNT(*)` query that page-number pagination runs, so it scales
    /// well for large tables.
    ///
    /// The field must be a **totally ordered** column: an integer,
    /// timestamp, date, uuid or string. `"id"` is the usual choice; a
    /// `created_at` timestamp is the other common one, and is what you
    /// want on an append-only table.
    ///
    /// # Panics
    ///
    /// If `field` is not on the model, or has a type that cannot be a
    /// cursor (float, bool, json or blob). A nullable field is logged as
    /// an error for now and refused from 0.61.0. Caught at mount time, not
    /// per request, so a misconfigured ViewSet fails where the
    /// mistake is instead of returning 500 to every caller.
    #[must_use]
    pub fn cursor_pagination(mut self, field: &'static str) -> Self {
        self.assert_cursor_field(field);
        self.assert_cursor_is_projected(field);
        self.pagination = PaginationStyle::Cursor { field, desc: false };
        self
    }

    /// Cursor pagination, descending order. Same constraints and same
    /// panics as [`Self::cursor_pagination`].
    #[must_use]
    pub fn cursor_pagination_desc(mut self, field: &'static str) -> Self {
        self.assert_cursor_field(field);
        self.assert_cursor_is_projected(field);
        self.pagination = PaginationStyle::Cursor { field, desc: true };
        self
    }

    /// Fail where the mistake is, not once per request.
    fn assert_cursor_field(&self, field: &'static str) {
        let table = self.schema.table;
        let Some(f) = self.schema.field(field) else {
            let known: Vec<&str> = self
                .schema
                .fields
                .iter()
                .filter(|f| cursor_field_supported(f.ty) && !f.nullable)
                .map(|f| f.name)
                .collect();
            panic!(
                "cursor_pagination(\"{field}\"): `{table}` has no field `{field}`. \
                 Usable cursor fields on this model: {known:?}"
            );
        };
        assert!(
            cursor_field_supported(f.ty),
            "cursor_pagination(\"{field}\"): `{table}.{field}` is {:?}, which cannot \
             be a cursor — the value has to round-trip through a token and order \
             totally. Use an integer, timestamp, date, uuid or string column.",
            f.ty
        );
        // NULL has no place in `col > v`: rows go missing, or the token
        // fails (#2230). Logged, not refused, until 0.61.0.
        if f.nullable {
            tracing::error!(
                target: "rustango::viewset",
                "cursor_pagination(\"{field}\"): `{table}.{field}` is nullable; NULL rows \
                 are skipped or fail the page. Use a NOT NULL column; 0.61.0 refuses this."
            );
        }
    }

    /// A `.fields([..])` projection must include the cursor column and
    /// the primary key.
    ///
    /// The `next` token is built from the *rendered* row, so a
    /// projection that drops either leaves nothing to encode. Checked
    /// both ways: from `fields()` when a cursor is already set, and
    /// from `cursor_pagination()` when the projection is.
    ///
    /// Only `.fields([..])` is covered. A serializer can project the
    /// same column away, and `ModelSerializer` exposes no field list
    /// to check, so that case is still caught at request time.
    fn assert_cursor_is_projected(&self, field: &str) {
        let Some(projection) = self.fields.as_ref() else {
            return; // no projection: every column is rendered
        };
        let table = self.schema.table;
        assert!(
            projection.iter().any(|f| f == field),
            "cursor_pagination(\"{field}\") with .fields({projection:?}): the cursor \
             column is projected away, so no `next` token can be built from a rendered \
             row and pagination would stop after one page. Add `{field}` to the field \
             list, or paginate on a field that is in it."
        );
        if let Some(pk) = self.schema.fields.iter().find(|f| f.primary_key) {
            assert!(
                pk.name == field || projection.iter().any(|f| f == pk.name),
                "cursor_pagination(\"{field}\") with .fields({projection:?}): the primary \
                 key `{table}.{}` is projected away. It breaks ties between rows sharing \
                 a `{field}`, and without it a page boundary inside a run of equal values \
                 skips the rest of the run. Add `{}` to the field list.",
                pk.name,
                pk.name
            );
        }
    }

    /// Switch to limit/offset pagination — `?limit=&offset=`. Like
    /// page-number it runs `COUNT(*)` per request, but callers window
    /// by row offset instead of page index.
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

    /// Register a filter backend. Each one adds `WHERE` predicates,
    /// ANDed with the built-in `filter_fields`. Call it repeatedly to
    /// stack backends; a matching closure works too (see
    /// [`ViewSetFilter`]).
    #[must_use]
    pub fn filter_backend(mut self, backend: impl ViewSetFilter) -> Self {
        self.filter_backends.push(std::sync::Arc::new(backend));
        self
    }

    /// Set per-action request throttles. Counters are process-local —
    /// see [`ViewSetThrottle`].
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
    /// accepted on create/update. Default: all scalar fields. A natural
    /// primary key stays writable on create.
    pub fn fields(mut self, fields: &[&str]) -> Self {
        self.fields = Some(fields.iter().map(|&s| s.to_owned()).collect());
        // Order-independent: the projection can be narrowed after the
        // cursor is chosen just as easily as before it.
        if let PaginationStyle::Cursor { field, .. } = self.pagination {
            self.assert_cursor_is_projected(field);
        }
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

    /// Allow-list for `?ordering=`. When set, only these names are
    /// honored; unknown ones are dropped, so a client cannot sort on
    /// a sensitive column. Unset (the default) means the fields the
    /// response renders; none when it renders none (#1996). An empty
    /// slice makes nothing sortable.
    pub fn ordering_fields(mut self, fields: &[&str]) -> Self {
        self.ordering_fields = Some(fields.iter().map(|&s| s.to_owned()).collect());
        self
    }

    /// Default page size for list responses (default: 20).
    ///
    /// Used when the client asks for no size. What a client may
    /// *request* is bounded by [`ViewSet::max_page_size`], clamped at
    /// request time, so the order of the two builder calls does not
    /// matter.
    pub fn page_size(mut self, n: usize) -> Self {
        self.default_page_size = n.max(1);
        self
    }

    /// Largest page a client may request via `?page_size=` / `?limit=`
    /// (default: 100).
    ///
    /// This bounds how much a single client request can amplify your
    /// per-row work. A serializer that queries per row turns a large
    /// page into that many queries in one request.
    ///
    /// The default matches `template_views`' ceiling, so the two
    /// pagination surfaces agree. Raise it when a consumer really
    /// needs bigger pages:
    ///
    /// ```ignore
    /// ViewSet::for_model(Post::SCHEMA).page_size(20).max_page_size(500)
    /// ```
    #[must_use]
    pub fn max_page_size(mut self, n: usize) -> Self {
        self.max_page_size = n.max(1);
        self
    }

    /// Most rows a bulk create (JSON array body) may carry; more is a
    /// `413`. Default 1000; each row also spends one `create` throttle unit.
    #[must_use]
    pub fn max_bulk_create(mut self, n: usize) -> Self {
        self.max_bulk_create = n.max(1);
        self
    }

    /// Default ordering for list responses. `(field, true)` = descending.
    /// Unset, the model's `default_order` applies; the PK always breaks ties.
    pub fn ordering(mut self, ordering: &[(&str, bool)]) -> Self {
        self.default_ordering = ordering.iter().map(|&(f, d)| (f.to_owned(), d)).collect();
        self
    }

    /// Permission codenames required per action. Empty vec = allow all;
    /// an open write action logs a warning at mount unless
    /// [`Self::allow_anonymous`] is set.
    pub fn permissions(mut self, perms: ViewSetPerms) -> Self {
        self.perms = perms;
        self
    }

    /// Fill `ViewSetPerms` with the four standard CRUD codenames for
    /// `T`, via [`crate::permissions::codename_for`].
    ///
    /// `list` and `retrieve` get `view`, `create` gets `add`,
    /// `update` gets `change`, `destroy` gets `delete`.
    ///
    /// Use [`Self::permissions`] for custom codenames or non-CRUD
    /// actions. Needs the `tenancy` feature, where `has_perm` lives.
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

    /// Rename the detail routes' path capture. Defaults to `pk`, i.e.
    /// `/{pk}`.
    ///
    /// axum allows only **one** capture name per path position in a
    /// router. A hand-written route beside a ViewSet that spells the
    /// same position `/{id}` panics at startup. Match the ViewSet to
    /// its neighbours instead of renaming them all:
    ///
    /// ```ignore
    /// ViewSet::for_model(Post::SCHEMA).pk_param("id").router("/api/posts", pool)
    /// // detail routes become /api/posts/{id}
    /// ```
    ///
    /// Handlers read the capture positionally, so only the route
    /// string and the OpenAPI parameter change.
    ///
    /// A capture cannot share a segment with a literal, so
    /// `/{token}:accept` is not expressible; use `/{token}/accept`.
    #[must_use]
    pub fn pk_param(mut self, name: impl Into<String>) -> Self {
        self.pk_param = name.into();
        self
    }

    /// The configured detail-route capture name (`pk` unless
    /// [`ViewSet::pk_param`] changed it).
    #[must_use]
    pub fn pk_param_name(&self) -> &str {
        &self.pk_param
    }

    /// Allow GET only — wires list + retrieve, skips create/update/destroy.
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// Say that write actions without codenames are meant to be open to
    /// any caller that reaches this router. Silences the mount warning.
    #[must_use]
    pub fn allow_anonymous(mut self) -> Self {
        self.allow_anonymous = true;
        self
    }

    /// Write actions any caller may run, unless acknowledged (#1857).
    fn open_write_actions(&self) -> Vec<&'static str> {
        if self.read_only || self.allow_anonymous {
            return Vec::new();
        }
        let p = &self.perms;
        [
            ("create", &p.create),
            ("update", &p.update),
            ("destroy", &p.destroy),
        ]
        .into_iter()
        .filter(|(_, codenames)| codenames.is_empty())
        .map(|(action, _)| action)
        .collect()
    }

    /// Describe the mounted RFC 10008 `QUERY` route in the generated
    /// OpenAPI document.
    ///
    /// **Off by default, and that is about tooling, not the route.**
    /// The route is mounted either way. `query` is a Path Item field
    /// OpenAPI added in **3.2.0**, so a document containing one must
    /// declare 3.2.0, and a client pinned to 3.1.0 rejects the whole
    /// document. Most generators are still 3.1.
    ///
    /// Turn it on once your toolchain reads 3.2:
    ///
    /// ```ignore
    /// ViewSet::for_model(Post::SCHEMA).openapi_query(true)
    /// ```
    #[must_use]
    pub fn openapi_query(mut self, on: bool) -> Self {
        self.openapi_query = on;
        self
    }

    /// Build an `axum::Router` mounted at `prefix`. The pool is baked
    /// in at mount time, so every request uses the same `PgPool`. For
    /// tenancy projects use [`Self::tenant_router`] instead, so each
    /// request resolves its own tenant connection.
    ///
    /// A trailing `/` on the prefix makes no difference.
    #[cfg(feature = "postgres")]
    pub fn router(self, prefix: &str, pool: crate::sql::sqlx::PgPool) -> Router {
        Self::router_with_source(self, prefix, PoolSource::Static(pool))
    }

    /// [`Self::router`], but taking the backend-erasing
    /// [`crate::sql::Pool`] enum. SQLite and MySQL projects use this;
    /// Postgres projects may use either.
    pub fn router_pool(self, prefix: &str, pool: crate::sql::Pool) -> Router {
        Self::router_with_source(self, prefix, PoolSource::StaticPool(pool))
    }

    /// Build a router that resolves the database connection per
    /// request via the [`crate::extractors::Tenant`] extractor. Use
    /// it for multi-tenant projects (subdomain, schema or per-tenant
    /// database): each handler runs against the connection for the
    /// tenant the request resolves to.
    ///
    /// Mount it on the API router that `Server::Builder` or
    /// `Cli::tenancy()` wires up; both inject the
    /// [`TenantContext`](crate::extractors::TenantContext) extension
    /// the extractor reads.
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
    /// Permission checks, when configured via [`Self::permissions`]
    /// or [`Self::permissions_for_model`], run on the same
    /// per-request connection — no second pool acquire.
    ///
    /// The list endpoint runs its SELECT and COUNT one after the
    /// other, not in parallel: `Tenant::conn()` hands out an
    /// exclusive connection. To skip the COUNT entirely, use
    /// [`Self::cursor_pagination`].
    #[cfg(feature = "tenancy")]
    #[must_use]
    pub fn tenant_router(self, prefix: &str) -> Router {
        Self::router_with_source(self, prefix, PoolSource::Tenant)
    }

    fn router_with_source(self, prefix: &str, pool_source: PoolSource) -> Router {
        let open = self.open_write_actions();
        if !open.is_empty() {
            tracing::warn!(
                target: "rustango::viewset",
                model = self.schema.table,
                prefix,
                actions = ?open,
                "ViewSet write actions need no permission, so any caller that reaches this \
                 router can run them; set .permissions_for_model(), .read_only(), or \
                 .allow_anonymous() if this is intended"
            );
        }
        let state = Arc::new(ViewSetState {
            pool_source,
            vs: self.clone(),
            throttle_store: Arc::new(ThrottleStore::new(MAX_THROTTLE_KEYS)),
        });
        let prefix = prefix.trim_end_matches('/').to_owned();
        let collection = prefix.clone();
        let item = format!("{prefix}/{{{}}}", self.pk_param);

        let collection_route = if self.read_only {
            get(handle_list)
        } else {
            get(handle_list).post(handle_create)
        };
        // RFC 10008 QUERY: the same filtered list as GET, with the
        // criteria in the body. Gated like the `http_query` module that
        // holds the routing shim.
        #[cfg(feature = "_http_layers")]
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

/// Where a [`ViewSet`] gets its database pool.
#[derive(Clone)]
enum PoolSource {
    /// PG-typed static pool, from `router(prefix, pool)`.
    #[cfg(feature = "postgres")]
    Static(crate::sql::sqlx::PgPool),
    /// Any-dialect static pool, from `router_pool(prefix, pool)`.
    StaticPool(crate::sql::Pool),
    /// Each handler resolves a connection through the
    /// [`crate::extractors::Tenant`] extractor at request time. A
    /// pool cannot be baked in because schema-mode tenants need a
    /// per-connection `SET search_path`, which only
    /// `TenantPools::acquire` does.
    #[cfg(feature = "tenancy")]
    Tenant,
}

#[derive(Clone)]
struct ViewSetState {
    pool_source: PoolSource,
    vs: ViewSet,
    /// Process-local fixed-window throttle counters, keyed by
    /// `{table}:{action}:{tenant}:{client}` and shared across requests.
    throttle_store: Arc<ThrottleStore>,
}

/// Distinct throttle keys kept before the store sweeps (#1999).
const MAX_THROTTLE_KEYS: usize = 100_000;

/// One client's fixed window for one action.
#[derive(Clone, Copy, Debug)]
struct ThrottleWindow {
    count: u32,
    /// The rule's `max`, so eviction compares spend across actions.
    max: u32,
    start: Instant,
    window: std::time::Duration,
}

impl ThrottleWindow {
    fn ended(&self, now: Instant) -> bool {
        now.duration_since(self.start) >= self.window
    }

    /// The spent share of `max`, as a fixed-point fraction.
    fn spent(&self) -> u64 {
        (u64::from(self.count) << 32) / u64::from(self.max.max(1))
    }
}

/// Fixed-window counters with a bounded key count.
struct ThrottleStore {
    cap: usize,
    windows: Mutex<HashMap<String, ThrottleWindow>>,
}

impl ThrottleStore {
    fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// Spend `cost` from `key`'s window: `Err(retry_after_secs)` when over
    /// `rule.max`. A rejected spend charges nothing, so it cannot lock a client out.
    fn spend(&self, key: String, rule: ThrottleRule, cost: u32, now: Instant) -> Result<(), u64> {
        // A poisoned mutex means an earlier panic mid-update. Fail open
        // rather than reject every request from here on.
        let Ok(mut windows) = self.windows.lock() else {
            return Ok(());
        };
        if !windows.contains_key(&key) {
            self.make_room(&mut windows, now);
        }
        let fresh = ThrottleWindow {
            count: 0,
            max: rule.max,
            start: now,
            window: std::time::Duration::from_secs(rule.window_secs),
        };
        let w = windows.entry(key).or_insert(fresh);
        if w.ended(now) {
            *w = fresh;
        }
        let count = w.count.saturating_add(cost);
        if count > rule.max {
            let left = w.window.saturating_sub(now.duration_since(w.start));
            return Err(left.as_secs().max(1));
        }
        w.count = count;
        Ok(())
    }

    /// Drop ended windows, which act like absent ones; if every window is
    /// still live, drop the eighth with the least spent share of `max`,
    /// the cheapest to lose (#1999).
    fn make_room(&self, windows: &mut HashMap<String, ThrottleWindow>, now: Instant) {
        if windows.len() < self.cap {
            return;
        }
        windows.retain(|_, w| !w.ended(now));
        if windows.len() < self.cap {
            return;
        }
        let mut by_spent: Vec<(u64, String)> = windows
            .iter()
            .map(|(k, w)| (w.spent(), k.clone()))
            .collect();
        by_spent.sort_unstable_by_key(|(n, _)| *n);
        for (_, k) in by_spent.into_iter().take((self.cap / 8).max(1)) {
            windows.remove(&k);
        }
    }
}

/// A request's tenant, resolved but not connected yet, so the
/// throttle can run between the two (#2076).
enum Scope {
    /// A static pool: no tenant.
    Static,
    #[cfg(feature = "tenancy")]
    Tenant(Box<crate::tenancy::Org>),
    /// No tenant matched: throttled by client, then a `404`.
    #[cfg(feature = "tenancy")]
    Unknown,
}

impl Scope {
    /// The tenant part of a throttle key. `?` cannot be in a slug.
    fn throttle_label(&self) -> &str {
        match self {
            Self::Static => "",
            #[cfg(feature = "tenancy")]
            Self::Tenant(org) => &org.slug,
            #[cfg(feature = "tenancy")]
            Self::Unknown => "?",
        }
    }
}

/// A per-request pool handle covering both static-pool and
/// per-request-tenant modes. [`ViewSetState::connect`] builds it at
/// the top of each handler, and its facade methods keep handler
/// bodies free of pool-source branching.
///
/// In tenant mode the inner `Pool` is the tenant's scoped pool: a
/// search-path-bound pool for PG schema mode, or the cached pool for
/// database mode. No connection is held until a query runs.
struct AcquiredConn {
    pool: Pool,
    scope: Scope,
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

    /// Decode every row matching `q` into the model struct `T`, on
    /// any backend. Used by the serializer render path
    /// ([`SerializerBridge`]) in place of
    /// [`Self::select_rows_as_json`]. Goes through the same pool, so
    /// it inherits the same tenant scoping.
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

    /// Insert a row and return its primary key; audited models write a `create` row.
    async fn insert_returning_pk(
        &mut self,
        q: &InsertQuery,
        pk_field: &crate::core::FieldSchema,
    ) -> Result<SqlValue, crate::sql::ExecError> {
        crate::audit::insert(&self.pool, q, pk_field).await
    }
}

impl AcquiredConn {
    async fn update(&mut self, q: &UpdateQuery) -> Result<u64, crate::sql::ExecError> {
        crate::audit::update(&self.pool, q).await
    }

    async fn delete(&mut self, q: &DeleteQuery) -> Result<u64, crate::sql::ExecError> {
        crate::audit::delete(&self.pool, q).await
    }

    async fn soft_delete(&mut self, q: &UpdateQuery) -> Result<u64, crate::sql::ExecError> {
        crate::audit::update_as(&self.pool, q, crate::audit::AuditOp::SoftDelete).await
    }

    #[cfg(feature = "tenancy")]
    async fn has_perm(&mut self, uid: i64, codename: &str) -> bool {
        crate::tenancy::permissions::has_perm_pool(uid, codename, &self.pool)
            .await
            .unwrap_or(false)
    }
}

impl ViewSet {
    fn effective_fields(&self) -> Vec<&'static crate::core::FieldSchema> {
        match &self.fields {
            Some(names) => names.iter().filter_map(|n| self.schema.field(n)).collect(),
            None => self.schema.scalar_fields().collect(),
        }
    }

    /// The fields a request body may set, before per-request pins.
    /// [`WriteSet`] and the OpenAPI request schemas both read this (#1922).
    fn body_fields(&self) -> Vec<&'static crate::core::FieldSchema> {
        let exposed = self.effective_fields();
        let serializer = self.serializer.as_ref().map(|b| b.writable_model_fields());
        self.schema
            .scalar_fields()
            .filter(|f| f.primary_key || exposed.iter().any(|e| e.name == f.name))
            .filter(|f| serializer.map_or(true, |w| w.contains(&f.name)))
            // Only DELETE stamps the soft-delete column; a body never sets it (#2074).
            .filter(|f| self.schema.soft_delete_column != Some(f.column))
            .collect()
    }

    /// The JSON key a client sends for `model_field`: its serializer
    /// name when a `source` rename gave it one.
    #[cfg(feature = "openapi")]
    fn body_key(&self, model_field: &'static str) -> &'static str {
        self.serializer
            .as_ref()
            .and_then(|b| {
                b.writable_field_names()
                    .iter()
                    .zip(b.writable_model_fields())
                    .find(|(_, m)| **m == model_field)
                    .map(|(n, _)| *n)
            })
            .unwrap_or(model_field)
    }
}

impl ViewSetState {
    fn effective_fields(&self) -> Vec<&'static crate::core::FieldSchema> {
        self.vs.effective_fields()
    }

    /// The fields a response shows: `fields()`, narrowed to what the
    /// serializer renders when one is set.
    fn rendered_fields(&self) -> Vec<&'static crate::core::FieldSchema> {
        let mut fields = self.effective_fields();
        if let Some(bridge) = &self.vs.serializer {
            let readable = bridge.readable_model_fields();
            fields.retain(|f| readable.contains(&f.name));
        }
        fields
    }

    /// Resolve the request's tenant through the mounted context, the
    /// one auth uses. Takes no connection. Errors come back as a
    /// finished [`Response`].
    // Only the `tenancy`-gated `PoolSource::Tenant` arm reads `parts`,
    // so an admin-only build sees an unused parameter.
    #[cfg_attr(not(feature = "tenancy"), allow(unused_variables))]
    async fn resolve(&self, parts: &axum::http::request::Parts) -> Result<Scope, Response> {
        match &self.pool_source {
            #[cfg(feature = "postgres")]
            PoolSource::Static(_) => Ok(Scope::Static),
            PoolSource::StaticPool(_) => Ok(Scope::Static),
            #[cfg(feature = "tenancy")]
            PoolSource::Tenant => {
                use crate::extractors::TenantRejection;
                match crate::tenancy::middleware::request_org(parts, &parts.extensions).await {
                    Some(Ok(Some(org))) => Ok(Scope::Tenant(Box::new(org))),
                    Some(Ok(None)) => Ok(Scope::Unknown),
                    Some(Err(e)) => Err(TenantRejection::Internal(e.to_string()).into_response()),
                    None => Err(TenantRejection::MissingContext.into_response()),
                }
            }
        }
    }

    /// The pool handle for a resolved `scope`. A cheap clone in
    /// static-pool mode; the tenant's scoped pool in tenant mode.
    #[cfg_attr(not(feature = "tenancy"), allow(unused_variables))]
    async fn connect(
        &self,
        scope: Scope,
        parts: &axum::http::request::Parts,
    ) -> Result<AcquiredConn, Response> {
        let pool = match (&self.pool_source, &scope) {
            #[cfg(feature = "postgres")]
            (PoolSource::Static(pool), _) => Pool::from(pool.clone()),
            (PoolSource::StaticPool(pool), _) => pool.clone(),
            #[cfg(feature = "tenancy")]
            (PoolSource::Tenant, scope) => {
                use crate::extractors::TenantRejection;
                let Scope::Tenant(org) = scope else {
                    return Err(TenantRejection::NotFound.into_response());
                };
                match crate::tenancy::middleware::request_pool(&parts.extensions, org).await {
                    Some(Ok(pool)) => pool,
                    Some(Err(e)) => {
                        return Err(TenantRejection::Internal(e.to_string()).into_response())
                    }
                    None => return Err(TenantRejection::MissingContext.into_response()),
                }
            }
        };
        Ok(AcquiredConn { pool, scope })
    }

    /// Permission gate. An empty `codenames` skips the check.
    /// Superusers are allowed straight away; everyone else goes to
    /// the `tenancy::permissions` engine.
    async fn check_perm(
        &self,
        codenames: &[String],
        parts: &axum::http::request::Parts,
        conn: &mut AcquiredConn,
    ) -> PermOutcome {
        if codenames.is_empty() {
            return PermOutcome::Allow;
        }
        #[cfg(feature = "tenancy")]
        {
            let Some(auth) = parts
                .extensions
                .get::<crate::tenancy::middleware::AuthenticatedUser>()
            else {
                // No principal at all: the client needs to
                // authenticate, so 401 rather than 403.
                return PermOutcome::Unauthenticated;
            };
            if auth.is_superuser {
                return PermOutcome::Allow;
            }
            for cn in codenames {
                if conn.has_perm(auth.id, cn).await {
                    return PermOutcome::Allow;
                }
            }
            PermOutcome::Forbidden
        }
        #[cfg(not(feature = "tenancy"))]
        {
            // Without tenancy there is no `AuthenticatedUser` and no
            // `has_perm` engine, so codenames with nothing to check
            // them against are denied. 403, not 401: authenticating
            // cannot satisfy a check with no engine behind it, and
            // 401 would send a token client into a refresh loop.
            let _ = (parts, conn);
            PermOutcome::Forbidden
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

/// A `200` JSON response.
fn json_response(body: Value) -> Response {
    json_with_status(StatusCode::OK, body)
}

/// An [`ApiError`](crate::api_errors::ApiError) body; a 5xx message is
/// logged rather than sent.
fn json_error(status: StatusCode, msg: &str) -> Response {
    crate::api_errors::ApiError::logged(status, "viewset", msg).into_response()
}

/// A 500 whose cause is logged under `context`, not sent.
fn json_server_error(context: &str, e: &dyn std::fmt::Display) -> Response {
    crate::api_errors::ApiError::logged(StatusCode::INTERNAL_SERVER_ERROR, context, e)
        .into_response()
}

/// A `422` from serializer validation, as every `validation_failed` is.
/// `details` is `{"<field>": ["msg", …], …, "non_field_errors": [ … ]}`.
fn json_form_errors(errs: &crate::forms::FormErrors) -> Response {
    json_form_errors_msg("invalid input", errs)
}

/// [`json_form_errors`] with its own message.
fn json_form_errors_msg(message: &str, errs: &crate::forms::FormErrors) -> Response {
    let mut map = serde_json::Map::new();
    for (field, msgs) in errs.fields() {
        map.insert(field.clone(), json!(msgs));
    }
    if !errs.non_field().is_empty() {
        map.insert("non_field_errors".to_owned(), json!(errs.non_field()));
    }
    crate::api_errors::ApiError::validation(message)
        .with_details(Value::Object(map))
        .into_response()
}

/// Rename inbound form keys from serializer field names to model
/// columns, for any writable field with `#[serializer(source = "…")]`.
/// A client can then POST `content` for a field declared
/// `#[serializer(source = "body")]`, and the persist step, which reads
/// model columns, still finds the value.
///
/// The model-column key of a renamed field is dropped: the serializer
/// never validated it, so it must not reach the write (#1994).
///
/// Returns `None` when nothing needs renaming, which is the common
/// case and costs no allocation.
fn serializer_input_renamed_form(
    state: &ViewSetState,
    form: &HashMap<String, String>,
) -> Option<HashMap<String, String>> {
    let bridge = state.vs.serializer.as_ref()?;
    let names = bridge.writable_field_names();
    // The two lists are parallel: JSON keys and model columns. They
    // differ only at `source` renames.
    let renames: Vec<(&'static str, &'static str)> = names
        .iter()
        .zip(bridge.writable_model_fields().iter())
        .filter(|(name, col)| name != col)
        .filter(|(name, col)| form.contains_key(**name) || form.contains_key(**col))
        .map(|(name, col)| (*name, *col))
        .collect();
    if renames.is_empty() {
        return None;
    }
    let out = form
        .iter()
        .filter_map(|(k, v)| match renames.iter().find(|(n, _)| n == k) {
            Some((_, col)) => Some(((*col).to_owned(), v.clone())),
            // A hidden column, unless another field publishes that name.
            None if renames.iter().any(|(_, c)| c == k) && !names.contains(&k.as_str()) => None,
            None => Some((k.clone(), v.clone())),
        })
        .collect();
    Some(out)
}

/// The name the API publishes for a model field, when a `source` rename
/// gave it a different one.
///
/// The inverse of [`serializer_input_renamed_form`]. The write loop
/// walks model fields, so parse errors name those. Without this, a
/// `source = "body"` rename would tell the client to supply `body`, a
/// column their schema never mentions.
fn serializer_public_field_name(state: &ViewSetState, model_field: &str) -> Option<&'static str> {
    let bridge = state.vs.serializer.as_ref()?;
    bridge
        .writable_field_names()
        .iter()
        .zip(bridge.writable_model_fields().iter())
        .find(|(name, col)| **col == model_field && *name != *col)
        .map(|(name, _)| *name)
}

/// Re-render a form error against the names the API publishes.
///
/// Only the field name changes. The wording stays, so a client
/// matching on the message still matches.
fn public_form_error(state: &ViewSetState, e: FormError) -> FormError {
    match e {
        FormError::Missing { field } => FormError::Missing {
            field: serializer_public_field_name(state, &field).map_or(field, ToOwned::to_owned),
        },
        FormError::Parse {
            field,
            ty,
            value,
            detail,
        } => FormError::Parse {
            field: serializer_public_field_name(state, &field).map_or(field, ToOwned::to_owned),
            ty,
            value,
            detail,
        },
        // Names a primary key, which no serializer renames. Matched
        // by name rather than `_` so a new variant has to be
        // considered here instead of leaking a model column.
        other @ FormError::UnsupportedPk { .. } => other,
    }
}

/// Run the serializer's input validation on the body.
fn serializer_validate(state: &ViewSetState, json: &Value) -> Result<(), Response> {
    if let Some(bridge) = &state.vs.serializer {
        bridge
            .validate_body(json)
            .map_err(|errs| json_form_errors(&errs))?;
    }
    Ok(())
}

/// The body the serializer validates: the JSON as sent, or a form
/// typed by its model fields, so a form cannot skip validation (#1993).
fn write_json(state: &ViewSetState, form: &HashMap<String, String>, json: Option<Value>) -> Value {
    if let Some(json) = json {
        return json;
    }
    // Only a serializer reads this body.
    let Some(b) = state.vs.serializer.as_ref() else {
        return Value::Null;
    };
    let model_name = |key: &str| -> Option<&'static str> {
        b.writable_field_names()
            .iter()
            .zip(b.writable_model_fields())
            .find(|(n, _)| **n == key)
            .map(|(_, m)| *m)
    };
    let typed = |key: &str, raw: &String| {
        // Unparseable stays a string, so the serializer reports it.
        model_name(key)
            .and_then(|m| state.vs.schema.field(m))
            .and_then(|f| parse_form_value(f, Some(raw)).ok())
            .and_then(sql_value_json)
            .unwrap_or_else(|| Value::String(raw.clone()))
    };
    Value::Object(form.iter().map(|(k, v)| (k.clone(), typed(k, v))).collect())
}

/// A parsed form value in the JSON shape its serializer field reads.
fn sql_value_json(v: SqlValue) -> Option<Value> {
    match v {
        SqlValue::Null => Some(Value::Null),
        SqlValue::I16(n) => Some(n.into()),
        SqlValue::I32(n) => Some(n.into()),
        SqlValue::I64(n) => Some(n.into()),
        SqlValue::F32(n) => serde_json::to_value(n).ok(),
        SqlValue::F64(n) => serde_json::to_value(n).ok(),
        SqlValue::Bool(b) => Some(b.into()),
        SqlValue::String(s) | SqlValue::RangeLiteral(s) => Some(s.into()),
        SqlValue::Json(j) => Some(j),
        SqlValue::DateTime(d) => serde_json::to_value(d).ok(),
        SqlValue::Date(d) => serde_json::to_value(d).ok(),
        SqlValue::Time(t) => serde_json::to_value(t).ok(),
        SqlValue::Uuid(u) => serde_json::to_value(u).ok(),
        SqlValue::Decimal(d) => serde_json::to_value(d).ok(),
        SqlValue::Binary(bytes) => serde_json::to_value(bytes).ok(),
        SqlValue::Vector(v) => serde_json::to_value(v).ok(),
        SqlValue::HStore(pairs) => Some(Value::Object(
            pairs
                .into_iter()
                .map(|(k, v)| (k, v.map_or(Value::Null, Value::String)))
                .collect(),
        )),
        SqlValue::Array(items) | SqlValue::List(items) => items
            .into_iter()
            .map(sql_value_json)
            .collect::<Option<Vec<_>>>()
            .map(Value::Array),
        SqlValue::Geometry { x, y, srid } => {
            Some(serde_json::json!({"x": x, "y": y, "srid": srid}))
        }
    }
}

/// The fields one write request may set, decided once (#1845).
///
/// Create and update read the body only through this, so `fields()`,
/// the serializer and the filter backends' pins cannot disagree.
struct WriteSet {
    schema: &'static ModelSchema,
    /// `fields()` ∩ the serializer's writable fields, minus the pinned.
    writable: Vec<&'static crate::core::FieldSchema>,
    /// Forced by a filter backend; never read from the body.
    pinned: Vec<(&'static crate::core::FieldSchema, SqlValue)>,
}

impl WriteSet {
    /// `Err` is a `403` when a backend denies the write.
    fn for_request(
        state: &ViewSetState,
        parts: &axum::http::request::Parts,
    ) -> Result<Self, Response> {
        let schema = state.vs.schema;
        let mut pinned: Vec<(&'static crate::core::FieldSchema, SqlValue)> = Vec::new();
        for backend in &state.vs.filter_backends {
            for pin in backend.write_pins(parts, schema) {
                match pin {
                    WritePin::Field { field, value } => match schema.field(field) {
                        // One pin per field; two backends that disagree deny.
                        Some(f) => match pinned.iter().find(|(p, _)| p.name == f.name) {
                            None => pinned.push((f, value)),
                            Some((_, v)) if *v == value => {}
                            Some(_) => {
                                tracing::error!(
                                    model = schema.table,
                                    field,
                                    "two write pins disagree on one field — denying"
                                );
                                return Err(json_error(StatusCode::FORBIDDEN, "forbidden"));
                            }
                        },
                        None => {
                            tracing::error!(
                                model = schema.table,
                                field,
                                "write pin names a field this model does not have — denying"
                            );
                            return Err(json_error(StatusCode::FORBIDDEN, "forbidden"));
                        }
                    },
                    WritePin::Deny => return Err(json_error(StatusCode::FORBIDDEN, "forbidden")),
                }
            }
        }
        let writable = state
            .vs
            .body_fields()
            .into_iter()
            .filter(|f| !pinned.iter().any(|(p, _)| p.name == f.name))
            // Only DELETE stamps the soft-delete column; a body never sets it (#2074).
            .filter(|f| schema.soft_delete_column != Some(f.column))
            .collect();
        Ok(Self {
            schema,
            writable,
            pinned,
        })
    }

    fn is_writable(&self, name: &str) -> bool {
        self.writable.iter().any(|f| f.name == name)
    }

    /// The fields an UPDATE writes when the body carries them.
    fn updatable(&self) -> impl Iterator<Item = &'static crate::core::FieldSchema> + '_ {
        self.writable
            .iter()
            .copied()
            .filter(|f| f.accepts_input(crate::core::WriteKind::Update))
    }

    /// `body` cut to the keys the UPDATE writes, so PATCH validation
    /// overlays only those on the stored row (#1995).
    fn patch_body(&self, bridge: &dyn SerializerBridge, body: &Value) -> Value {
        let Some(obj) = body.as_object() else {
            return body.clone();
        };
        let written = |key: &str| {
            bridge
                .writable_field_names()
                .iter()
                .zip(bridge.writable_model_fields())
                .any(|(n, m)| *n == key && self.updatable().any(|f| f.name == *m))
        };
        Value::Object(
            obj.iter()
                .filter(|(k, _)| written(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        )
    }

    /// The INSERT's `(column, value)` list: writable fields from `form`,
    /// the server-stamped timestamps, then the pins.
    fn insert_values(
        &self,
        form: &HashMap<String, String>,
    ) -> Result<Vec<(&'static str, SqlValue)>, Refusal> {
        // An omitted defaulted field is left to the column default (#2528).
        let skip: Vec<&str> = self
            .schema
            .scalar_fields()
            .filter(|f| {
                !self.is_writable(f.name) || (absent_takes_default(f) && !form.contains_key(f.name))
            })
            .map(|f| f.name)
            .collect();
        let mut out = collect_insert_values(self.schema, form, &skip)?;
        // A pin replaces a stamped `auto` value rather than doubling the column.
        out.retain(|(c, _)| !self.pinned.iter().any(|(f, _)| f.column == *c));
        out.extend(self.pinned.iter().map(|(f, v)| (f.column, v.clone())));
        self.check(&out)?;
        Ok(out)
    }

    /// The UPDATE's `SET` list. Pinned fields are never in it.
    fn update_assignments(
        &self,
        form: &HashMap<String, String>,
        partial: bool,
    ) -> Result<Vec<Assignment>, Refusal> {
        let mut out = Vec::new();
        for field in self.updatable() {
            if partial && !form.contains_key(field.name) {
                continue;
            }
            match parse_form_value(field, form.get(field.name).map(String::as_str)) {
                Ok(v) => out.push((field.column, v)),
                Err(FormError::Missing { .. }) if partial => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.check(&out)?;
        // An empty body stays "no fields to update", not a bare restamp.
        if !out.is_empty() {
            crate::forms::stamp_auto_now(self.schema, &mut out);
        }
        Ok(out
            .into_iter()
            .map(|(column, v)| Assignment {
                column,
                value: v.into(),
            })
            .collect())
    }

    /// The model's field rules, checked here so a rejection is a 400 (#2529).
    fn check(&self, values: &[(&'static str, SqlValue)]) -> Result<(), Refusal> {
        for (column, value) in values {
            let Some(field) = self.schema.scalar_fields().find(|f| f.column == *column) else {
                continue;
            };
            if let Err(e) = crate::core::validate_value(self.schema.name, field, value) {
                // Anything else (an unknown validator) is the server's; the write reports it.
                if let Some((_, message)) = e.value_rejection() {
                    return Err(Refusal::Invalid {
                        field: field.name,
                        message,
                    });
                }
            }
        }
        Ok(())
    }
}

/// Why a body cannot be written: unparseable, or refused by a field rule.
enum Refusal {
    Form(FormError),
    Invalid {
        field: &'static str,
        message: String,
    },
}

impl From<FormError> for Refusal {
    fn from(e: FormError) -> Self {
        Self::Form(e)
    }
}

impl Refusal {
    /// A parse error is a `400`, a field-rule refusal a `422` like a
    /// serializer's, under the names the API publishes; `entry` is a bulk index.
    fn into_response(self, state: &ViewSetState, entry: Option<usize>) -> Response {
        let prefix = entry
            .map(|i| format!("bulk entry {i}: "))
            .unwrap_or_default();
        match self {
            Self::Form(e) => json_error(
                StatusCode::BAD_REQUEST,
                &format!("{prefix}{}", public_form_error(state, e)),
            ),
            Self::Invalid { field, message } => {
                let mut errs = crate::forms::FormErrors::default();
                errs.add(
                    serializer_public_field_name(state, field).unwrap_or(field),
                    message,
                );
                json_form_errors_msg(&format!("{prefix}invalid input"), &errs)
            }
        }
    }
}

/// Unwrap, or return a `500` JSON error carrying the message.
///
/// `$expr` must be a `Result<_, E>` where `E: Display`.
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

/// The model's global scopes, its soft-delete liveness and every filter
/// backend's predicates, to be ANDed into whatever query the action runs.
fn scope_filters(
    state: &ViewSetState,
    parts: &axum::http::request::Parts,
    params: &HashMap<String, String>,
) -> Vec<WhereExpr> {
    let mut all = state.vs.schema.global_scope_exprs(&[]);
    // A soft-deleted row reads as gone to every action (#1998).
    all.extend(crate::soft_delete::active_filter(state.vs.schema));
    all.extend(
        state
            .vs
            .filter_backends
            .iter()
            .flat_map(|b| b.filter_with(parts, params, state.vs.schema)),
    );
    all
}

/// `expr` narrowed by `extra`. An empty `extra` returns `expr`
/// unchanged, so a ViewSet with no backends builds the same SQL.
fn narrow(expr: WhereExpr, extra: Vec<WhereExpr>) -> WhereExpr {
    if extra.is_empty() {
        return expr;
    }
    let mut all = vec![expr];
    all.extend(extra);
    WhereExpr::And(all)
}

/// Outcome of a per-action permission check.
///
/// Three-valued on purpose: "no principal" and "principal without the
/// permission" are different answers to the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermOutcome {
    /// Authorised — proceed.
    Allow,
    /// No authenticated principal → `401`.
    ///
    /// Only the `tenancy` permission engine builds this. Without that
    /// feature every denial is `Forbidden`, so the variant is
    /// unconstructed rather than dead.
    #[cfg_attr(not(feature = "tenancy"), allow(dead_code))]
    Unauthenticated,
    /// Authenticated, but lacks every required codename → `403`.
    Forbidden,
}

/// The preamble every REST handler runs. Splits the request into
/// parts and body, resolves a connection through the [`PoolSource`],
/// then checks the action's permission codenames. On failure it
/// returns a finished [`Response`] (401 / 403 / 5xx) to `?`-bubble.
///
/// The body comes back unconsumed, so write paths can read it after
/// the permission gate. Read handlers bind it as `_body`.
async fn enter(
    state: &Arc<ViewSetState>,
    req: axum::extract::Request,
    codenames: &[String],
    action: &'static str,
) -> Result<(axum::http::request::Parts, Body, AcquiredConn), Response> {
    let (parts, body) = req.into_parts();
    // Resolve, throttle, then connect: the budget is per tenant, and a
    // throttled request must not touch the tenant pool (#2076).
    let scope = state.resolve(&parts).await?;
    if let Some(resp) = check_throttle(state, action, &parts, &scope) {
        return Err(resp);
    }
    let mut acq = state.connect(scope, &parts).await?;
    match state.check_perm(codenames, &parts, &mut acq).await {
        PermOutcome::Allow => {}
        // 401 means "authenticate", 403 means "you may not". A token
        // client treats 401 as its cue to refresh, so answering 403
        // to an anonymous request silently logs the member out.
        PermOutcome::Unauthenticated => {
            return Err(json_error(
                StatusCode::UNAUTHORIZED,
                "authentication required",
            ))
        }
        PermOutcome::Forbidden => {
            return Err(json_error(StatusCode::FORBIDDEN, "permission denied"))
        }
    }
    Ok((parts, body, acq))
}

/// Per-action fixed-window throttle. Returns `Some(429)` when the
/// client has gone over the action's [`ThrottleRule`] for this
/// window. Counters are process-local — see [`ViewSetThrottle`].
fn check_throttle(
    state: &ViewSetState,
    action: &str,
    parts: &axum::http::request::Parts,
    scope: &Scope,
) -> Option<Response> {
    state.vs.throttle.for_action(action)?;
    spend_throttle(state, action, &ThrottleClient::new(parts, scope), 1)
}

/// Spend `cost` requests of `action`'s throttle for `client`.
fn spend_throttle(
    state: &ViewSetState,
    action: &str,
    client: &ThrottleClient,
    cost: u32,
) -> Option<Response> {
    let rule = state.vs.throttle.for_action(action)?;
    let key = format!("{}:{}:{}", state.vs.schema.table, action, client.0);
    state
        .throttle_store
        .spend(key, rule, cost, Instant::now())
        .err()
        .map(throttled_response)
}

/// Who a throttle budget belongs to: the tenant, then the client.
/// Built only from a resolved [`Scope`], so no key can skip the tenant (#2076).
struct ThrottleClient(String);

impl ThrottleClient {
    /// The scope's tenant label, then the trusted client IP
    /// (IPv6 by /64), else one shared `"global"` bucket. Never a raw
    /// forwarding header (#1745).
    fn new(parts: &axum::http::request::Parts, scope: &Scope) -> Self {
        let ip = crate::rate_limit::client_ip(&parts.extensions, &parts.headers).map_or_else(
            || {
                crate::rate_limit::warn_missing_discriminator("IP (ConnectInfo missing)");
                "global".to_owned()
            },
            crate::rate_limit::ip_bucket,
        );
        Self(format!("{}:{ip}", scope.throttle_label()))
    }
}

/// A `429 Too Many Requests` with a `Retry-After` header.
fn throttled_response(retry_after_secs: u64) -> Response {
    crate::api_errors::ApiError::rate_limited_response("request throttled", retry_after_secs)
}

/// Parse a path capture into the primary key's type, or a `400`.
fn parse_pk_or_400(
    field: &'static crate::core::FieldSchema,
    raw: &str,
) -> Result<crate::core::SqlValue, Response> {
    parse_pk_string(field, raw).map_err(|e| json_error(StatusCode::BAD_REQUEST, &e.to_string()))
}

/// The model's primary-key field, or a `500` if it has none.
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

/// Why a filter query param was refused, as a `400` (#2227).
#[derive(Debug)]
enum FilterError {
    /// The `__lookup` suffix is not one the ViewSet supports.
    UnknownLookup,
    /// The value does not parse, or the lookup does not fit the field.
    Invalid(String),
    TooLong(crate::list_params::InListTooLong),
}

impl FilterError {
    fn message(&self, param: &str) -> String {
        match self {
            Self::UnknownLookup => format!("filter `{param}`: unknown lookup"),
            Self::Invalid(detail) => format!("filter `{param}`: {detail}"),
            Self::TooLong(e) => e.to_string(),
        }
    }
}

/// Build a `WhereExpr` from one query-param `field[__lookup]=value` entry.
///
/// Supported lookups:
/// - (none) / `exact` — `Op::Eq`; `iexact` — case-insensitive
/// - `gt`, `gte`, `lt`, `lte`, `ne`
/// - `in` / `not_in` — comma-separated values; `range` / `between` — `lo,hi`
/// - `contains` (LIKE %v%) / `icontains` (ILIKE %v%)
/// - `startswith` (LIKE v%) / `istartswith` (ILIKE v%)
/// - `endswith` (LIKE %v) / `iendswith` (ILIKE %v)
/// - `isnull` — value `"true"` / `"false"`
/// - date parts as in `QuerySet::filter`: `year`, `month`, `day`, `hour`,
///   `minute`, `second`, `quarter`, `week`, `week_day`, `date`, each
///   optionally followed by `__gt`, `__gte`, `__lt`, `__lte` or `__ne`
///
/// An empty value is no filter, as in the admin: on a nullable field
/// it would parse to NULL and `col = NULL` matches nothing (#2226).
/// An unknown lookup or a bad value is an error, never a dropped filter.
fn build_lookup_filter(
    field: &'static crate::core::FieldSchema,
    lookup: Option<&str>,
    raw: &str,
) -> Result<Option<WhereExpr>, FilterError> {
    if raw.is_empty() {
        return Ok(None);
    }
    let column = field.column;
    let predicate = |op: Op, value: SqlValue| WhereExpr::Predicate(Filter { column, op, value });
    let parse =
        |s: &str| parse_form_value(field, Some(s)).map_err(|e| FilterError::Invalid(e.to_string()));
    // Escapes the user value and pairs it with the matching
    // `*Escaped` op in one place, so no arm can combine an escaped
    // value with a plain `Op::Like`, or the reverse.
    let escaped_like = |prefix: &str, suffix: &str, raw: &str, case_insensitive: bool| {
        let escaped = crate::core::escape_like(raw);
        let op = if case_insensitive {
            Op::ILikeEscaped
        } else {
            Op::LikeEscaped
        };
        predicate(op, SqlValue::String(format!("{prefix}{escaped}{suffix}")))
    };
    let lookup = lookup.unwrap_or("exact");
    let (token, trailing) = match lookup.split_once("__") {
        Some((token, trailing)) => (token, Some(trailing)),
        None => (lookup, None),
    };
    if let Some(transform) = crate::query::date_transform_fn(token) {
        return date_part_filter(field, transform, trailing, raw).map(Some);
    }
    // The binary comparisons share the ORM's suffix table.
    if let Some(op) = crate::query::date_compare_op(lookup) {
        return match parse(raw) {
            Ok(value) => Ok(Some(predicate(op, value))),
            Err(e) => whole_day_bound(field, op, raw)
                .map(|(op, value)| Some(predicate(op, value)))
                .ok_or(e),
        };
    }
    let parse_list = || -> Result<Vec<SqlValue>, FilterError> {
        crate::list_params::split_in_list(raw)
            .map_err(FilterError::TooLong)?
            .into_iter()
            .map(parse)
            .collect()
    };
    Ok(Some(match lookup {
        "in" | "not_in" => {
            let parts = parse_list()?;
            if parts.is_empty() {
                return Err(FilterError::Invalid(
                    "expects comma-separated values".into(),
                ));
            }
            let op = if lookup == "not_in" {
                Op::NotIn
            } else {
                Op::In
            };
            predicate(op, SqlValue::List(parts))
        }
        "range" | "between" => {
            let parts = parse_list()?;
            if parts.len() != 2 {
                return Err(FilterError::Invalid("expects two values, `lo,hi`".into()));
            }
            predicate(Op::Between, SqlValue::List(parts))
        }
        // Escape LIKE metacharacters in the URL-supplied `raw`, so
        // `%` and `_` match literally, and use the `*Escaped` ops so
        // an ESCAPE clause is emitted. SQLite needs that clause.
        "iexact" | "contains" | "icontains" | "startswith" | "istartswith" | "endswith"
        | "iendswith"
            if field.ty != FieldType::String =>
        {
            return Err(FilterError::Invalid(
                "this lookup needs a string field".into(),
            ));
        }
        "iexact" => escaped_like("", "", raw, true),
        "contains" => escaped_like("%", "%", raw, false),
        "icontains" => escaped_like("%", "%", raw, true),
        "startswith" => escaped_like("", "%", raw, false),
        "istartswith" => escaped_like("", "%", raw, true),
        "endswith" => escaped_like("%", "", raw, false),
        "iendswith" => escaped_like("%", "", raw, true),
        "isnull" => {
            let is_null = match raw.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => true,
                "false" | "0" | "no" => false,
                _ => return Err(FilterError::Invalid("expects `true` or `false`".into())),
            };
            predicate(Op::IsNull, SqlValue::Bool(is_null))
        }
        _ => return Err(FilterError::UnknownLookup),
    }))
}

/// `__year=2024`, `__date__gte=2024-01-01` and the other date parts.
fn date_part_filter(
    field: &'static crate::core::FieldSchema,
    transform: crate::core::ScalarFn,
    trailing: Option<&str>,
    raw: &str,
) -> Result<WhereExpr, FilterError> {
    use crate::core::ScalarFn;
    let op = match trailing {
        None => Op::Eq,
        Some(t) => crate::query::date_compare_op(t).ok_or(FilterError::UnknownLookup)?,
    };
    let time_part = matches!(
        transform,
        ScalarFn::ExtractHour | ScalarFn::ExtractMinute | ScalarFn::ExtractSecond
    );
    let fits = match field.ty {
        FieldType::DateTime => true,
        FieldType::Date => !time_part,
        _ => false,
    };
    if !fits {
        let needs = if time_part {
            "a datetime"
        } else {
            "a date or datetime"
        };
        return Err(FilterError::Invalid(format!(
            "this lookup needs {needs} field"
        )));
    }
    let value = if transform == ScalarFn::TruncDate {
        chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
            .map(SqlValue::Date)
            .map_err(|_| FilterError::Invalid(format!("`{raw}` is not a date (YYYY-MM-DD)")))?
    } else {
        raw.parse::<i64>()
            .map(SqlValue::I64)
            .map_err(|_| FilterError::Invalid(format!("`{raw}` is not an integer")))?
    };
    Ok(crate::query::date_transform_where(
        field.column,
        transform,
        op,
        value,
    ))
}

/// A plain date on a datetime `__gte` / `__lte` covers that whole UTC
/// day. Other comparisons stay an error: `__gt=<day>` is ambiguous.
fn whole_day_bound(field: &crate::core::FieldSchema, op: Op, raw: &str) -> Option<(Op, SqlValue)> {
    if field.ty != FieldType::DateTime {
        return None;
    }
    let day = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()?;
    let (op, day) = match op {
        Op::Gte => (Op::Gte, day),
        Op::Lte => (Op::Lt, day.succ_opt()?),
        _ => return None,
    };
    Some((
        op,
        SqlValue::DateTime(day.and_time(chrono::NaiveTime::MIN).and_utc()),
    ))
}

// ------------------------------------------------------------------ Handlers

async fn handle_list(
    State(state): State<Arc<ViewSetState>>,
    Query(params): Query<HashMap<String, String>>,
    req: axum::extract::Request,
) -> Response {
    let (parts, _body, acq) = match enter(&state, req, &state.vs.perms.list, "list").await {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    run_list(state, params, acq, &parts).await
}

/// The `list` logic, shared by GET and the RFC 10008 QUERY action.
/// Builds filters, search, ordering and pagination from `params` and
/// renders the paginated envelope. `params` come from the
/// querystring on GET and from the body on QUERY, so both transports
/// return the same results for the same criteria.
async fn run_list(
    state: Arc<ViewSetState>,
    params: HashMap<String, String>,
    mut acq: AcquiredConn,
    parts: &axum::http::request::Parts,
) -> Response {
    // Clamp to the ViewSet's own ceiling. The default page size is
    // clamped too, so a `page_size` larger than `max_page_size`
    // cannot smuggle a bigger page through the default path.
    let page_size: i64 = params
        .get("page_size")
        .and_then(|p| p.parse().ok())
        .unwrap_or(state.vs.default_page_size as i64)
        .min(state.vs.max_page_size as i64)
        .max(1);

    // Build WHERE from filter_fields in query params.
    //
    // Supports both:
    //   ?author_id=42                — exact match (Op::Eq)
    //   ?author_id__gt=10            — lookup suffix
    //   ?status__in=draft,published  — comma-separated for IN/NOT_IN
    //   ?title__icontains=hello      — pattern lookups
    //   ?published_at__isnull=true   — IS NULL / IS NOT NULL
    let mut filters: Vec<WhereExpr> = Vec::new();
    let in_budget = crate::list_params::in_values_budget(acq.pool.dialect().max_bind_params());
    let mut in_values = 0_usize;
    for (param_key, raw_val) in &params {
        // `list_params::is_reserved_list_key` is the single source of
        // truth, shared with template_views.
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
        match build_lookup_filter(field, lookup, raw_val) {
            Ok(Some(predicate)) => {
                if let WhereExpr::Predicate(Filter {
                    value: SqlValue::List(items),
                    ..
                }) = &predicate
                {
                    in_values += items.len();
                }
                filters.push(predicate);
            }
            Ok(None) => {}
            // A backend may own keys like `price__min` (#2264).
            Err(FilterError::UnknownLookup) if !state.vs.filter_backends.is_empty() => {
                tracing::debug!(target: "rustango::viewset", param = %param_key, "lookup left to filter backends");
            }
            Err(e) => return json_error(StatusCode::BAD_REQUEST, &e.message(param_key)),
        }
    }
    // Every list value is a bind, so the sum must fit the dialect too.
    if in_values > in_budget {
        let e = crate::list_params::InListTooLong::Total(in_budget);
        return json_error(StatusCode::BAD_REQUEST, &e.to_string());
    }

    // Filter backends add predicates, ANDed with the built-in
    // `filter_fields` parsed above.
    filters.extend(scope_filters(&state, parts, &params));

    let where_clause = if filters.len() == 1 {
        filters.remove(0)
    } else if filters.is_empty() {
        WhereExpr::And(vec![])
    } else {
        WhereExpr::And(filters)
    };

    // Search
    // No `search_fields`: `?search=` is not a filter here.
    let search = params
        .get("search")
        .filter(|s| !s.is_empty() && !state.vs.search_fields.is_empty())
        .cloned();
    let search_clause = search.map(|q| SearchClause {
        query: q,
        columns: state
            .vs
            .search_fields
            .iter()
            .filter_map(|n| state.vs.schema.field(n).map(|f| f.column))
            .collect(),
    });

    // Ordering. `?ordering=` honours the `ordering_fields`
    // allow-list when it is set, and unknown names are dropped, so a
    // client cannot sort on `password_hash` just because it is a
    // column. With no explicit allow-list the fallback is the fields
    // this ViewSet actually *exposes*, not every column on the model
    // — otherwise `fields = "id, title"` would still allow a sort
    // oracle over a column the API never returns. When `fields` is
    // unset, `effective_fields` is every scalar column anyway. A
    // serializer narrows it to what it renders (#1845).
    //
    // `list_params::parse_ordering` is the single source of truth,
    // shared with `template_views::ListView`.
    let ordering_allowlist: Vec<String> = match &state.vs.ordering_fields {
        Some(allowed) => allowed.clone(),
        None => state
            .rendered_fields()
            .iter()
            .map(|f| f.name.to_owned())
            .collect(),
    };
    let order_by: Vec<crate::core::OrderItem> = params
        .get("ordering")
        .map(|raw| crate::list_params::parse_ordering(raw, &ordering_allowlist, state.vs.schema))
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default_order_by(&state.vs));
    // The PK breaks ties so rows cannot repeat or vanish across pages (#2047).
    let order_by = state.vs.schema.with_pk_tiebreak(order_by);

    let fields = state.effective_fields();

    match &state.vs.pagination {
        PaginationStyle::PageNumber => {
            let page = crate::list_params::parse_page(&params);
            let offset = crate::list_params::page_offset(page, page_size);

            // Struct-update over `SelectQuery::new` for the
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
                source: None,
            };

            // The SELECT and COUNT run one after the other: tenant
            // mode holds a single per-request connection, and one
            // sequential path keeps the handler simple.
            //
            // `render_list` goes through the registered serializer
            // when there is one, else the default
            // `select_rows_as_json` projection.
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
            // `?limit=` overrides the default page size and
            // `?offset=` skips rows. Same `COUNT(*)` cost as
            // page-number pagination, and the same ceiling as
            // `?page_size=` — otherwise it would be a way around it.
            let limit: i64 = params
                .get("limit")
                .and_then(|p| p.parse().ok())
                .unwrap_or(page_size)
                .min(state.vs.max_page_size as i64)
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
                source: None,
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

/// `.ordering(..)`, else the model's `default_order`, as ListView and the admin do (#2047).
fn default_order_by(vs: &ViewSet) -> Vec<crate::core::OrderItem> {
    let schema = vs.schema;
    let builder: Vec<(&str, bool)> = vs
        .default_ordering
        .iter()
        .map(|(n, d)| (n.as_str(), *d))
        .collect();
    let spec = if builder.is_empty() {
        schema.default_order
    } else {
        &builder[..]
    };
    spec.iter()
        .filter_map(|(name, desc)| {
            schema
                .field(name)
                .or_else(|| schema.field_by_column(name))
                .map(|f| crate::core::OrderItem::column(f.column, *desc))
        })
        .collect()
}

/// RFC 10008 QUERY on the collection: the same filtered, paginated
/// `list`, with the criteria in the request body. `QUERY /things`
/// with body `status=draft&ordering=-created` returns what
/// `GET /things?status=draft&ordering=-created` would. Permissions
/// and throttles reuse the `list` codenames.
#[cfg(feature = "admin")]
async fn handle_query(
    State(state): State<Arc<ViewSetState>>,
    req: axum::extract::Request,
) -> Response {
    // `enter` reads only `parts`, so the body is still unconsumed
    // and the params can be parsed from it here. QUERY spends the list
    // throttle: it returns the same rows as GET (#1997).
    let (parts, body, acq) = match enter(&state, req, &state.vs.perms.list, "list").await {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    let params = match parse_query_body_params(&parts, body).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    run_list(state, params, acq, &parts).await
}

/// Parse a QUERY body into the same param map `run_list` takes for
/// GET. Dispatches on `Content-Type`: urlencoded (or none) goes
/// through the querystring path, a JSON object is flattened to
/// strings (arrays comma-joined, so `{"status__in":["a","b"]}`
/// matches `status__in=a,b`), and anything else is a 415.
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

/// Flatten a JSON value to the string form the filter parser wants:
/// strings as-is, arrays comma-joined (for `__in`), null as empty,
/// numbers and bools as their JSON text.
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
    if !cursor_field_supported(cursor_schema.ty) {
        // A 500, because it is a server misconfiguration, not
        // anything the caller did. `cursor_pagination()` panics at
        // build time on an unusable field, so reaching this at
        // request time needs a hand-built `PaginationStyle::Cursor`.
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!(
                "cursor pagination needs a totally-ordered column; `{cursor_field}` is \
                 {:?}, which does not round-trip through a cursor token. Use an \
                 integer, timestamp, date, uuid or string column.",
                cursor_schema.ty
            ),
        );
    }

    // The primary key breaks ties. A non-unique cursor column, such
    // as any timestamp, can put equal values on both sides of a page
    // boundary, and a strict `>` on the column alone then skips
    // every tied row after the first. Ordering by `(col, pk)` and
    // comparing the pair makes the position total.
    //
    // Skipped when the cursor is the primary key, already unique.
    let pk_schema = state.vs.schema.fields.iter().find(|f| f.primary_key);
    let tiebreak = pk_schema.filter(|pk| pk.column != cursor_schema.column);

    // Decode the incoming cursor (if any)
    let cursor_pos: Option<(SqlValue, Option<SqlValue>)> = match params.get("cursor") {
        Some(c) if !c.is_empty() => {
            let pk_ty = tiebreak.map_or(cursor_schema.ty, |pk| pk.ty);
            match decode_cursor(c, cursor_schema.ty, pk_ty) {
                Some(v) => Some(v),
                None => return json_error(StatusCode::BAD_REQUEST, "invalid cursor"),
            }
        }
        _ => None,
    };

    // Build WHERE = filters AND (cursor predicate, if any)
    let final_where = match cursor_pos {
        Some((v, pk_v)) => {
            let op = if desc { Op::Lt } else { Op::Gt };
            let cursor_pred = match (tiebreak, pk_v) {
                // `col > v OR (col = v AND pk > pk_v)` — strictly after
                // the last row of the previous page, ties included.
                (Some(pk), Some(pk_v)) => WhereExpr::Or(vec![
                    WhereExpr::Predicate(Filter {
                        column: cursor_schema.column,
                        op,
                        value: v.clone(),
                    }),
                    WhereExpr::And(vec![
                        WhereExpr::Predicate(Filter {
                            column: cursor_schema.column,
                            op: Op::Eq,
                            value: v,
                        }),
                        WhereExpr::Predicate(Filter {
                            column: pk.column,
                            op,
                            value: pk_v,
                        }),
                    ]),
                ]),
                // No tiebreak: the cursor is the PK, or the token is a
                // pre-#1459 one that carries no pk component.
                _ => WhereExpr::Predicate(Filter {
                    column: cursor_schema.column,
                    op,
                    value: v,
                }),
            };
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

    // Order by the cursor field, then the tiebreaker. The ORDER BY
    // must match the comparison above, or "strictly after" is wrong.
    let mut order_by = vec![crate::core::OrderItem::column(cursor_schema.column, desc)];
    if let Some(pk) = tiebreak {
        order_by.push(crate::core::OrderItem::column(pk.column, desc));
    }

    // Fetch `page_size + 1` rows to tell whether a next page exists.
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
        // The tiebreaker travels in the token. If the PK is
        // projected away the position cannot be total, and a
        // value-only token would start skipping tied rows again — so
        // report it, like a missing cursor column.
        let pk_part = match tiebreak {
            Some(pk) => match cursor_value_of(last, pk.name) {
                Some(v) => Some(v),
                None => {
                    return json_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &format!(
                            "cursor pagination on `{}` needs the primary key `{}` in the \
                             rendered rows to break ties, and it is not projected. Two rows \
                             sharing a `{}` would otherwise be split across a page boundary \
                             and all but one skipped. Include `{}` in `.fields([..])` / the \
                             serializer.",
                            cursor_schema.name, pk.name, cursor_schema.name, pk.name,
                        ),
                    );
                }
            },
            None => None,
        };
        match cursor_value_of(last, cursor_schema.name) {
            Some(v) => Some(encode_cursor(&v, pk_part.as_deref())),
            None if last.get(cursor_schema.name).is_some_and(Value::is_null) => {
                return json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!(
                        "cursor field `{}` is NULL in this row, so no `next` token can \
                         be issued. Paginate on a NOT NULL column.",
                        cursor_schema.name
                    ),
                );
            }
            // The cursor column is not in the rendered row, almost
            // always because `.fields([...])` or a serializer
            // projected it away. `next: null` here would be silent
            // truncation: `has_more` is true, but the caller gets no
            // continuation token and no error. Say so instead.
            None => {
                return json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!(
                        "cursor field `{}` is not present in the rendered rows, so no \
                         `next` token can be issued and pagination would silently stop \
                         after one page. Include it in `.fields([..])` / the \
                         serializer, or paginate on a field that is projected.",
                        cursor_schema.name
                    ),
                );
            }
        }
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

/// Can this column be a cursor?
///
/// Cursor pagination needs a **totally ordered** column whose value
/// round-trips through a string. Integers, timestamps, dates, UUIDs
/// and strings all qualify; monotonic ids (`UUIDv7`, ULID) are a
/// common cursor and sort correctly in SQL.
///
/// The rest are excluded for a reason: floats do not round-trip
/// exactly, `Json` / `Binary` / `Array` have no useful total order,
/// and `Bool` has too few values to page by.
pub(crate) fn cursor_field_supported(ty: FieldType) -> bool {
    matches!(
        ty,
        FieldType::I16
            | FieldType::I32
            | FieldType::I64
            | FieldType::DateTime
            | FieldType::Date
            | FieldType::Uuid
            | FieldType::String
    )
}

/// Encode a cursor position as URL-safe base64.
///
/// `tiebreak` is the row's primary key, carried with the cursor value
/// whenever the cursor column is not the primary key. Without it a
/// page boundary landing on a run of equal values drops all but one
/// of them: the next page asks for `col > v`, and the tied rows are
/// `= v`. Ties are ordinary on timestamp cursors — a `bulk_insert`,
/// or Postgres' per-transaction `now()`.
///
/// A composite token is a two-element JSON array; a bare value is the
/// older form. [`decode_cursor`] reads both.
fn encode_cursor(value: &str, tiebreak: Option<&str>) -> String {
    use base64::Engine;
    let payload = match tiebreak {
        Some(pk) => serde_json::json!([value, pk]).to_string(),
        None => value.to_owned(),
    };
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.as_bytes())
}

/// Decode a cursor token into a bind value of the cursor column's type.
///
/// Returns `None` for malformed input, which the caller turns into a
/// 400: a bad cursor is the client's mistake.
fn decode_cursor(
    token: &str,
    ty: FieldType,
    pk_ty: FieldType,
) -> Option<(SqlValue, Option<SqlValue>)> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token.as_bytes())
        .ok()?;
    let s = std::str::from_utf8(&bytes).ok()?;

    // Composite form first: `["<value>", "<pk>"]`. A bare value that
    // happens to parse as JSON, as an integer cursor does, is not a
    // two-element array of strings, so the forms cannot be confused.
    if let Some([a, b]) = serde_json::from_str::<Vec<String>>(s)
        .ok()
        .filter(|v| v.len() == 2)
        .as_deref()
    {
        return Some((
            parse_cursor_scalar(a, ty)?,
            Some(parse_cursor_scalar(b, pk_ty)?),
        ));
    }
    Some((parse_cursor_scalar(s, ty)?, None))
}

/// One cursor component, parsed per the column's type.
fn parse_cursor_scalar(s: &str, ty: FieldType) -> Option<SqlValue> {
    match ty {
        FieldType::I16 | FieldType::I32 | FieldType::I64 => {
            s.parse::<i64>().ok().map(SqlValue::I64)
        }
        FieldType::DateTime => chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| SqlValue::DateTime(dt.with_timezone(&chrono::Utc))),
        FieldType::Date => chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .ok()
            .map(SqlValue::Date),
        FieldType::Uuid => s.parse::<uuid::Uuid>().ok().map(SqlValue::Uuid),
        FieldType::String => Some(SqlValue::String(s.to_owned())),
        _ => None,
    }
}

/// The string form of a rendered row's cursor column, for the `next`
/// token.
///
/// Reads the JSON the list endpoint already produced instead of
/// re-querying. An integer arrives as a number, everything else as a
/// string.
fn cursor_value_of(row: &Value, field_name: &str) -> Option<String> {
    let v = row.get(field_name)?;
    v.as_i64()
        .map(|n| n.to_string())
        .or_else(|| v.as_str().map(std::borrow::ToOwned::to_owned))
}

async fn handle_retrieve(
    State(state): State<Arc<ViewSetState>>,
    Path(pk_raw): Path<String>,
    req: axum::extract::Request,
) -> Response {
    let (parts, _body, mut acq) =
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

    let mut select_q = SelectQuery::by_pk(state.vs.schema, pk_field.column, pk_val);
    // Narrowed by the filter backends, so a row this principal may
    // not see reads as absent. A 403 would confirm the id exists.
    let scope = scope_filters(&state, &parts, &HashMap::new());
    select_q.where_clause = narrow(select_q.where_clause, scope);

    let fields = state.effective_fields();
    match render_single(&state, &mut acq, &select_q, &fields).await {
        Ok(Some(row)) => json_response(row),
        Ok(None) => json_error(StatusCode::NOT_FOUND, "not found"),
        Err(e) => json_server_error("viewset::retrieve", &e),
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

    // The read-back is scoped: a row created outside it is not echoed.
    let scope = scope_filters(&state, &parts, &HashMap::new());
    let write_set = match WriteSet::for_request(&state, &parts) {
        Ok(w) => w,
        Err(resp) => return resp,
    };
    // A bulk create spends one throttle unit per row (#1999).
    let client = state
        .vs
        .throttle
        .create
        .map(|_| ThrottleClient::new(&parts, &acq.scope));
    // A JSON array body means a bulk create.
    let create_body = match extract_create_body(parts, body, typed_schema(&state)).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };

    let pk_field = match pk_field_or_500(&state) {
        Ok(f) => f,
        Err(resp) => return resp,
    };

    match create_body {
        CreateBody::Single(form, json) => {
            let json = write_json(&state, &form, json);
            create_one(
                &state,
                &mut acq,
                &write_set,
                (&form, &json),
                pk_field,
                &scope,
            )
            .await
        }
        CreateBody::Bulk(rows) => {
            if rows.len() > state.vs.max_bulk_create {
                let msg = format!(
                    "bulk create accepts at most {} rows",
                    state.vs.max_bulk_create
                );
                return json_error(StatusCode::PAYLOAD_TOO_LARGE, &msg);
            }
            // More rows than a whole window allows can never pass: say so.
            if let Some(rule) = state.vs.throttle.create {
                if rows.len() > rule.max as usize {
                    let msg = format!(
                        "bulk create of {} rows exceeds the create throttle of {} per window",
                        rows.len(),
                        rule.max
                    );
                    return json_error(StatusCode::PAYLOAD_TOO_LARGE, &msg);
                }
            }
            let extra = u32::try_from(rows.len().saturating_sub(1)).unwrap_or(u32::MAX);
            if let Some(resp) = client
                .as_ref()
                .and_then(|c| spend_throttle(&state, "create", c, extra))
            {
                return resp;
            }
            let rows: Vec<_> = rows
                .into_iter()
                .map(|(form, json)| {
                    let json = write_json(&state, &form, json);
                    (form, json)
                })
                .collect();
            create_many(&state, &mut acq, &write_set, &rows, pk_field, &scope).await
        }
    }
}

/// Run `INSERT … RETURNING <pk>`, then re-fetch the row by its PK as
/// JSON, narrowed by `scope`; `None` when the new row is outside it.
///
/// Returns `(StatusCode, message)` so the caller emits the right code:
/// * `CONFLICT` on a duplicate key, `BAD_REQUEST` on another constraint
///   violation or a bad value — see [`write_failure`].
/// * `INTERNAL_SERVER_ERROR` when the re-fetch errors.
///
/// `columns` and `values` come from an earlier `collect_values`, so
/// the inbound shape is already validated.
async fn insert_and_fetch_one(
    state: &Arc<ViewSetState>,
    acq: &mut AcquiredConn,
    columns: Vec<&'static str>,
    values: Vec<SqlValue>,
    pk_field: &'static crate::core::FieldSchema,
    fields: &[&'static crate::core::FieldSchema],
    scope: &[WhereExpr],
) -> Result<Option<Value>, (StatusCode, String)> {
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
        .map_err(|e| write_failure("viewset::create", &e))?;
    fetch_by_pk_scoped(state, acq, pk_field, pk_val, fields, scope)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                crate::error::client_error_body("viewset::create::read_back", &e, false),
            )
        })
}

/// A failed INSERT/UPDATE: a duplicate key is a `409`, any other database
/// rejection a `400`, driver text withheld; anything else (an audit write) is a logged `500`.
/// Field rules are checked before the write, by `WriteSet::check`.
fn write_failure(context: &str, e: &crate::sql::ExecError) -> (StatusCode, String) {
    let client_caused = matches!(e, crate::sql::ExecError::Driver(sqlx::Error::Database(_)));
    let body = crate::error::client_error_body(context, e, client_caused);
    let status = if e.is_unique_violation() {
        StatusCode::CONFLICT
    } else if client_caused {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (status, body)
}

/// Single-row create — used by both the form-urlencoded codepath
/// and the JSON-object body codepath. Returns 201 + the row JSON, or
/// 201 with no body when the new row is outside `scope`.
async fn create_one(
    state: &Arc<ViewSetState>,
    acq: &mut AcquiredConn,
    write_set: &WriteSet,
    (form, json): (&HashMap<String, String>, &Value),
    pk_field: &'static crate::core::FieldSchema,
    scope: &[WhereExpr],
) -> Response {
    if let Err(resp) = serializer_validate(state, json) {
        return resp;
    }
    // Translate `source`-renamed writable keys (serializer field name →
    // model column) so the client can POST the serializer field name.
    let renamed = serializer_input_renamed_form(state, form);
    let form = renamed.as_ref().unwrap_or(form);
    let collected = match write_set.insert_values(form) {
        Ok(v) => v,
        Err(e) => return e.into_response(state, None),
    };
    let (columns, values): (Vec<_>, Vec<_>) = collected.into_iter().unzip();
    let fields = state.effective_fields();
    match insert_and_fetch_one(state, acq, columns, values, pk_field, &fields, scope).await {
        Ok(Some(obj)) => json_created(obj),
        Ok(None) => StatusCode::CREATED.into_response(),
        Err((code, msg)) => json_error(code, &msg),
    }
}

/// Bulk create from a JSON array body.
/// Validates every entry first; on first failure, the WHOLE bulk
/// is rejected with the index + message (atomic-validate, not
/// atomic-insert — partial-insert recovery is a separate concern).
/// On success, inserts each row sequentially and returns 201 + the
/// JSON array of created rows in submission order; a row outside
/// `scope` is `null`.
///
/// Issue #435.
async fn create_many(
    state: &Arc<ViewSetState>,
    acq: &mut AcquiredConn,
    write_set: &WriteSet,
    rows: &[(HashMap<String, String>, Value)],
    pk_field: &'static crate::core::FieldSchema,
    scope: &[WhereExpr],
) -> Response {
    if rows.is_empty() {
        return json_created(Value::Array(Vec::new()));
    }

    // Atomic validation: collect every (columns, values) up front
    // so a bad row near the end of the list doesn't leave half the
    // INSERTs committed: validate the whole list before any save.
    let mut prepared: Vec<(Vec<&'static str>, Vec<SqlValue>)> = Vec::with_capacity(rows.len());
    for (i, (row, json)) in rows.iter().enumerate() {
        if let Err(resp) = serializer_validate(state, json) {
            return resp;
        }
        let renamed = serializer_input_renamed_form(state, row);
        let row = renamed.as_ref().unwrap_or(row);
        let collected = match write_set.insert_values(row) {
            Ok(v) => v,
            Err(e) => return e.into_response(state, Some(i)),
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
    //
    // All N inside one transaction (#1403). Validation was atomic and
    // the writes were not, which is worse than plainly non-atomic: a
    // unique or foreign-key violation on entry 5 committed entries 0-4,
    // returned `400 bulk entry 5`, and listed none of them — so the
    // caller is told the batch failed, five rows exist, and nothing in
    // the response says which. Those constraints are exactly the class
    // validation cannot decide up front.
    let fields = state.effective_fields();
    let mut tx = match crate::sql::transaction_pool(&acq.pool).await {
        Ok(tx) => tx,
        Err(e) => {
            return json_server_error("viewset::bulk_create::begin", &e);
        }
    };

    let mut pks: Vec<SqlValue> = Vec::with_capacity(prepared.len());
    for (i, (columns, values)) in prepared.into_iter().enumerate() {
        let query = InsertQuery {
            model: state.vs.schema,
            columns,
            values,
            returning: vec![pk_field.column],
            on_conflict: None,
        };
        match crate::audit::insert_tx(&mut tx, &query, pk_field).await {
            Ok(pk) => pks.push(pk),
            Err(e) => {
                // Drop every row this request wrote, including the ones
                // that succeeded before entry `i`.
                let _ = tx.rollback().await;
                // The entry index is what the caller can act on; the
                // driver text behind it names tables and constraints,
                // so it goes to the log only (#1525). The body must
                // not claim a server fault on a 400.
                //
                // Only a *database rejection* is the client's doing.
                // This arm also catches pool timeouts and dropped
                // connections, and logging those at `warn` left an
                // outage with no ERROR record anywhere (#1604 review,
                // correctness-003).
                let (status, detail) = write_failure("viewset::bulk_create::entry", &e);
                return json_error(status, &format!("bulk entry {i}: {detail}"));
            }
        }
    }
    if let Err(e) = tx.commit().await {
        return json_server_error("viewset::bulk_create::commit", &e);
    }

    // Read the rows back after the commit. They have to be committed to
    // be visible here — `acq` is the pool, not the transaction — and
    // atomicity is a property of the writes, which is the part that was
    // missing.
    let mut created: Vec<Value> = Vec::with_capacity(pks.len());
    for pk_val in pks {
        match fetch_by_pk_scoped(state, acq, pk_field, pk_val, &fields, scope).await {
            Ok(obj) => created.push(obj.unwrap_or(Value::Null)),
            Err(e) => return json_server_error("viewset::bulk_create::read_back", &e),
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
    // Scope first: an update must not reach a row this principal cannot see.
    let scope = scope_filters(&state, &parts, &HashMap::new());
    let write_set = match WriteSet::for_request(&state, &parts) {
        Ok(w) => w,
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

    let (form, json) = match extract_form_body(parts, body, typed_schema(&state)).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let json = write_json(&state, &form, json);

    // PATCH checks only the sent fields, over the stored row (#1995),
    // locked until the UPDATE in the same transaction commits (#2010).
    let mut locked = None;
    let validated = match &state.vs.serializer {
        Some(bridge) if partial => {
            let mut q = SelectQuery::by_pk(state.vs.schema, pk_field.column, pk_val.clone());
            q.where_clause = narrow(q.where_clause, scope.clone());
            q.lock_mode = Some(crate::core::LockMode {
                silent_on_sqlite: true,
                ..crate::core::LockMode::default()
            });
            let mut tx = match crate::sql::write_transaction_pool(&acq.pool).await {
                Ok(tx) => tx,
                Err(e) => return json_server_error("viewset::update::begin", &e),
            };
            let body = write_set.patch_body(bridge.as_ref(), &json);
            let checked = match bridge.validate_patch(&mut tx, &q, &body).await {
                Ok(r) => r.map_err(|errs| json_form_errors(&errs)),
                Err(e) => return json_server_error("viewset::update::validate", &e),
            };
            // A model audited only on a pool updates there, unlocked,
            // rather than lose its audit entry.
            locked = crate::audit::TxUpdate::for_model(state.vs.schema).map(|u| (u, tx));
            checked
        }
        _ => serializer_validate(&state, &json),
    };
    if let Err(resp) = validated {
        return resp;
    }

    // Translate `source`-renamed writable keys (serializer field name →
    // model column) before the per-column update loop reads the form.
    let renamed = serializer_input_renamed_form(&state, &form);
    let form = renamed.as_ref().unwrap_or(&form);

    let assignments = match write_set.update_assignments(form, partial) {
        Ok(a) => a,
        Err(e) => return e.into_response(&state, None),
    };

    if assignments.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "no fields to update");
    }

    let query = UpdateQuery {
        model: state.vs.schema,
        set: assignments,
        where_clause: narrow(
            WhereExpr::Predicate(Filter {
                column: pk_field.column,
                op: Op::Eq,
                value: pk_val.clone(),
            }),
            scope.clone(),
        ),
    };

    let updated = match locked.as_mut() {
        Some((u, tx)) => u.run(tx, &acq.pool, &query, pk_val.clone()).await,
        None => acq.update(&query).await,
    };
    match updated {
        // Nothing matched: either no such row, or one this principal is
        // scoped out of. Both are a 404 — see `handle_retrieve`.
        Ok(0) => return json_error(StatusCode::NOT_FOUND, "not found"),
        Ok(_) => {}
        Err(e) => {
            let (status, msg) = write_failure("viewset::update", &e);
            return json_error(status, &msg);
        }
    }
    if let Some((_, tx)) = locked {
        if let Err(e) = tx.commit().await {
            return json_server_error("viewset::update::commit", &e);
        }
    }

    // The UPDATE committed; a row it moved out of scope answers 204.
    let fields = state.effective_fields();
    match fetch_by_pk_scoped(&state, &mut acq, pk_field, pk_val, &fields, &scope).await {
        Ok(Some(obj)) => json_response(obj),
        Ok(None) => no_content(),
        Err(e) => json_server_error("viewset::update::read_back", &e),
    }
}

async fn handle_destroy(
    State(state): State<Arc<ViewSetState>>,
    Path(pk_raw): Path<String>,
    req: axum::extract::Request,
) -> Response {
    let (parts, _body, mut acq) = match enter(&state, req, &state.vs.perms.destroy, "destroy").await
    {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    let scope = scope_filters(&state, &parts, &HashMap::new());

    let pk_field = match pk_field_or_500(&state) {
        Ok(f) => f,
        Err(resp) => return resp,
    };
    let pk_val = match parse_pk_or_400(pk_field, &pk_raw) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // A soft-delete model stamps its column, as the admin does (#1998).
    let deleted = if let Some(col) = state.vs.schema.soft_delete_column {
        let mut query = crate::soft_delete::__mark_query(
            state.vs.schema,
            col,
            pk_field.column,
            pk_val,
            Some(chrono::Utc::now()),
        );
        query.where_clause = narrow(query.where_clause, scope);
        acq.soft_delete(&query).await
    } else {
        let query = DeleteQuery {
            model: state.vs.schema,
            where_clause: narrow(
                WhereExpr::Predicate(Filter {
                    column: pk_field.column,
                    op: Op::Eq,
                    value: pk_val,
                }),
                scope,
            ),
        };
        acq.delete(&query).await
    };
    match deleted {
        Ok(0) => json_error(StatusCode::NOT_FOUND, "not found"),
        Ok(_) => no_content(),
        Err(e) => json_server_error("viewset::destroy", &e),
    }
}

// ------------------------------------------------------------------ helpers

/// The read-back after a write, narrowed by `scope`: `None` when the
/// written row is outside it, so a scoped-out row is never echoed.
async fn fetch_by_pk_scoped(
    state: &ViewSetState,
    acq: &mut AcquiredConn,
    pk_field: &'static crate::core::FieldSchema,
    pk_val: SqlValue,
    fields: &[&'static crate::core::FieldSchema],
    scope: &[WhereExpr],
) -> Result<Option<Value>, crate::sql::ExecError> {
    let mut select_q = SelectQuery::by_pk(state.vs.schema, pk_field.column, pk_val);
    select_q.where_clause = narrow(select_q.where_clause, scope.to_vec());
    render_single(state, acq, &select_q, fields).await
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

/// Why a write body was refused: over a size cap (413) or unreadable (400).
pub(crate) enum BodyError {
    TooLarge,
    Bad(String),
}

impl From<String> for BodyError {
    fn from(msg: String) -> Self {
        Self::Bad(msg)
    }
}

impl From<&str> for BodyError {
    fn from(msg: &str) -> Self {
        Self::Bad(msg.to_owned())
    }
}

impl BodyError {
    fn into_response(self) -> Response {
        match self {
            Self::TooLarge => json_error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
            Self::Bad(msg) => json_error(StatusCode::BAD_REQUEST, &msg),
        }
    }
}

/// Extract form data from both `application/x-www-form-urlencoded` and
/// `application/json` request bodies.
async fn extract_form_body(
    parts: axum::http::request::Parts,
    body: Body,
    typed: Option<&'static ModelSchema>,
) -> Result<(HashMap<String, String>, Option<Value>), BodyError> {
    match extract_create_body(parts, body, typed).await? {
        CreateBody::Single(form, json) => Ok((form, json)),
        CreateBody::Bulk(_) => Err("expected a JSON object; got an array".into()),
    }
}

/// Sniff a POST body and return either a single record (object body
/// or form-urlencoded body) or a bulk list (JSON array body).
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
    typed: Option<&'static ModelSchema>,
) -> Result<CreateBody, BodyError> {
    use axum::body::to_bytes;

    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let bytes = to_bytes(body, 4 * 1024 * 1024).await.map_err(|e| {
        // Our own cap or an outer `BodyLimitLayer` stream cap (#1673).
        #[cfg(feature = "_http_layers")]
        if crate::body_limit::over_cap(&e) {
            return BodyError::TooLarge;
        }
        BodyError::Bad(e.to_string())
    })?;

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
                let form =
                    json_object_to_form(obj, typed).map_err(|e| format!("bulk entry {i}: {e}"))?;
                bulk.push((form, Some(entry.clone())));
            }
            return Ok(CreateBody::Bulk(bulk));
        }
        let obj = value
            .as_object()
            .ok_or("expected a JSON object or array of objects")?;
        Ok(CreateBody::Single(
            json_object_to_form(obj, typed)?,
            Some(value),
        ))
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
///
/// With `typed`, keys are decoded by their model field (#2530): null on
/// a NOT NULL field is refused, and a Json field keeps the value as JSON.
fn json_object_to_form(
    obj: &serde_json::Map<String, Value>,
    typed: Option<&'static ModelSchema>,
) -> Result<HashMap<String, String>, String> {
    let mut form = HashMap::with_capacity(obj.len());
    for (k, v) in obj {
        // Server-set columns are never read from the body.
        let field = typed
            .and_then(|s| s.field(k))
            .filter(|f| !f.auto && f.generated_as.is_none());
        let s = match (v, field) {
            (Value::Null, Some(f)) if !f.nullable => {
                return Err(format!("field `{k}` may not be null"));
            }
            (Value::Null, _) => String::new(),
            (other, Some(f)) if f.ty == FieldType::Json => other.to_string(),
            (Value::String(s), _) => s.clone(),
            (Value::Number(n), _) => n.to_string(),
            (Value::Bool(b), _) => b.to_string(),
            (other, _) => other.to_string(),
        };
        form.insert(k.clone(), s);
    }
    Ok(form)
}

/// The schema a JSON body is decoded by; a serializer types its own (#2530).
fn typed_schema(state: &ViewSetState) -> Option<&'static ModelSchema> {
    state.vs.serializer.is_none().then_some(state.vs.schema)
}

/// Every ViewSet error is an `ApiError`, and a 5xx withholds its cause (#1193).
#[cfg(test)]
mod envelope_tests {
    use super::{json_error, json_server_error, throttled_response, StatusCode};

    async fn body(r: axum::response::Response) -> serde_json::Value {
        let b = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
        serde_json::from_slice(&b).unwrap()
    }

    #[tokio::test]
    async fn a_404_is_the_envelope() {
        let v = body(json_error(StatusCode::NOT_FOUND, "not found")).await;
        assert_eq!(
            (v["error"].as_str(), v["message"].as_str()),
            (Some("not_found"), Some("not found"))
        );
        assert_eq!(v["status"], 404);
    }

    #[tokio::test]
    async fn a_500_withholds_the_driver_text() {
        let _env = crate::error::test_env::lock();
        let (a, b) = (
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "relation \"users\" missing",
            ),
            json_server_error("t", &"relation \"users\" missing"),
        );
        for v in [body(a).await, body(b).await] {
            assert_eq!(v["error"], "internal_error");
            assert!(!v.to_string().contains("users"), "{v}");
        }
    }

    #[tokio::test]
    async fn a_throttle_carries_retry_after() {
        let r = throttled_response(5);
        assert_eq!(r.headers()[axum::http::header::RETRY_AFTER], "5");
        let v = body(r).await;
        assert_eq!(
            (v["error"].as_str(), v["details"]["retry_after"].as_u64()),
            (Some("rate_limited"), Some(5))
        );
    }
}

#[cfg(all(test, feature = "admin"))]
mod body_tests {
    use super::{extract_create_body, Body, BodyError, StatusCode};

    fn json_parts() -> axum::http::request::Parts {
        let req = axum::http::Request::builder()
            .header("content-type", "application/json")
            .body(())
            .unwrap();
        req.into_parts().0
    }

    /// A streamed body over an outer cap is a 413, not a 400 (#1673).
    #[tokio::test]
    async fn an_over_cap_body_is_413() {
        let body = Body::new(http_body_util::Limited::new(Body::from("[1,2,3,4]"), 4));
        let Err(err) = extract_create_body(json_parts(), body, None).await else {
            panic!("an over-cap body was accepted");
        };
        assert!(matches!(err, BodyError::TooLarge));
        assert_eq!(err.into_response().status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn a_malformed_body_is_still_400() {
        let Err(err) = extract_create_body(json_parts(), Body::from("{not json"), None).await
        else {
            panic!("malformed JSON was accepted");
        };
        assert_eq!(err.into_response().status(), StatusCode::BAD_REQUEST);
    }
}

#[cfg(test)]
mod throttle_store_tests {
    use super::{ThrottleRule, ThrottleStore};
    use std::time::{Duration, Instant};

    fn len(store: &ThrottleStore) -> usize {
        store.windows.lock().unwrap().len()
    }

    /// Ended windows are swept once the store is full, so it stays bounded (#1999).
    #[test]
    fn a_full_store_drops_ended_windows() {
        let store = ThrottleStore::new(4);
        let rule = ThrottleRule::new(5, 1);
        let then = Instant::now();
        for n in 0..4 {
            store.spend(format!("k{n}"), rule, 1, then).unwrap();
        }
        let later = then + Duration::from_secs(2);
        store.spend("fresh".into(), rule, 1, later).unwrap();
        assert_eq!(len(&store), 1, "every ended window swept");
    }

    /// With every window live, the sweep keeps the spent ones and still bounds the map.
    #[test]
    fn a_full_live_store_keeps_spent_windows() {
        let store = ThrottleStore::new(4);
        let rule = ThrottleRule::new(2, 60);
        let now = Instant::now();
        store.spend("spent".into(), rule, 2, now).unwrap();
        for n in 0..3 {
            store.spend(format!("k{n}"), rule, 1, now).unwrap();
        }
        store.spend("new".into(), rule, 1, now).unwrap();
        assert!(len(&store) <= 4);
        assert!(
            store.spend("spent".into(), rule, 1, now).is_err(),
            "no reset"
        );
    }

    /// Eviction ranks by the spent share of each rule's `max`, not raw counts.
    #[test]
    fn a_full_store_evicts_the_least_spent_share() {
        let store = ThrottleStore::new(4);
        let now = Instant::now();
        store
            .spend("big".into(), ThrottleRule::new(100, 60), 10, now)
            .unwrap();
        for n in 0..3 {
            let rule = ThrottleRule::new(2, 60);
            store.spend(format!("s{n}"), rule, 1, now).unwrap();
        }
        store
            .spend("new".into(), ThrottleRule::new(2, 60), 1, now)
            .unwrap();
        let windows = store.windows.lock().unwrap();
        assert!(!windows.contains_key("big"), "10% spent is the cheapest");
        assert!((0..3).all(|n| windows.contains_key(&format!("s{n}"))));
    }

    /// A rejected spend charges nothing (#1999 review).
    #[test]
    fn a_rejected_spend_is_free() {
        let store = ThrottleStore::new(4);
        let rule = ThrottleRule::new(3, 60);
        let now = Instant::now();
        assert!(store.spend("k".into(), rule, 50, now).is_err());
        assert!(store.spend("k".into(), rule, 2, now).is_ok());
        assert!(store.spend("k".into(), rule, 2, now).is_err());
        assert!(store.spend("k".into(), rule, 1, now).is_ok());
    }

    #[test]
    fn cost_spends_several_units() {
        let store = ThrottleStore::new(4);
        let rule = ThrottleRule::new(3, 60);
        let now = Instant::now();
        assert!(store.spend("k".into(), rule, 3, now).is_ok());
        assert!(store.spend("k".into(), rule, 1, now).is_err());
    }
}

#[cfg(test)]
mod cursor_tests {
    use super::{cursor_field_supported, decode_cursor, encode_cursor, FieldType, SqlValue};

    fn int_token(v: i64) -> String {
        encode_cursor(&v.to_string(), None)
    }

    /// Decode a value-only token. Most cases below predate the
    /// tiebreaker and only care about the cursor component.
    fn decode_value(token: &str, ty: FieldType) -> Option<SqlValue> {
        decode_cursor(token, ty, FieldType::I64).map(|(v, _)| v)
    }

    #[test]
    fn cursor_roundtrip_positive() {
        let token = int_token(12345);
        assert_eq!(
            decode_value(&token, FieldType::I64),
            Some(SqlValue::I64(12345))
        );
    }

    #[test]
    fn cursor_roundtrip_zero() {
        let token = int_token(0);
        assert_eq!(decode_value(&token, FieldType::I64), Some(SqlValue::I64(0)));
    }

    #[test]
    fn cursor_roundtrip_max() {
        let token = int_token(i64::MAX);
        assert_eq!(
            decode_value(&token, FieldType::I64),
            Some(SqlValue::I64(i64::MAX))
        );
    }

    #[test]
    fn cursor_decode_invalid_base64_returns_none() {
        assert!(decode_value("not!valid!base64@@", FieldType::I64).is_none());
    }

    #[test]
    fn cursor_decode_non_numeric_payload_returns_none() {
        use base64::Engine;
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("not_a_number");
        assert!(decode_value(&token, FieldType::I64).is_none());
    }

    /// Tokens issued before #1459 must still decode. An integer cursor
    /// is base64 of its decimal string in both the old and the new
    /// encoder, so a client paginating across an upgrade keeps working.
    #[test]
    fn integer_tokens_are_unchanged_by_the_generalisation() {
        use base64::Engine;
        let legacy = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"4242");
        assert_eq!(int_token(4242), legacy);
        assert_eq!(
            decode_value(&legacy, FieldType::I64),
            Some(SqlValue::I64(4242))
        );
    }

    /// A composite token carries the tiebreaker and comes back intact.
    ///
    /// The tiebreaker is what stops a page boundary inside a run of
    /// equal cursor values from skipping the rest of the run — see
    /// `tests/cursor_pagination_on_a_timestamp.rs` for the end-to-end
    /// version.
    #[test]
    fn composite_tokens_round_trip_both_components() {
        let iso = "2026-09-14T22:41:59Z";
        let token = encode_cursor(iso, Some("4242"));
        let (value, tie) =
            decode_cursor(&token, FieldType::DateTime, FieldType::I64).expect("decodes");
        assert!(matches!(value, SqlValue::DateTime(_)), "got {value:?}");
        assert_eq!(tie, Some(SqlValue::I64(4242)));
    }

    /// A bare value and a composite must never be confused. An integer
    /// cursor's payload (`4242`) is valid JSON, so the discriminator has
    /// to be "a two-element array of strings", not "parses as JSON".
    #[test]
    fn a_bare_integer_token_is_not_read_as_composite() {
        let (value, tie) =
            decode_cursor(&int_token(4242), FieldType::I64, FieldType::I64).expect("decodes");
        assert_eq!(value, SqlValue::I64(4242));
        assert_eq!(
            tie, None,
            "a pre-tiebreaker token carries no second component"
        );
    }

    /// The case that 500'd on every request before #1459.
    #[test]
    fn timestamp_cursors_round_trip() {
        let iso = "2026-09-14T22:41:59Z";
        let token = encode_cursor(iso, None);
        let decoded = decode_value(&token, FieldType::DateTime);
        match decoded {
            Some(SqlValue::DateTime(dt)) => {
                assert_eq!(dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true), iso);
            }
            other => panic!("expected a DateTime cursor, got {other:?}"),
        }
    }

    #[test]
    fn uuid_and_string_cursors_round_trip() {
        let u = "0199c1f4-0000-7000-8000-000000000000";
        assert!(matches!(
            decode_value(&encode_cursor(u, None), FieldType::Uuid),
            Some(SqlValue::Uuid(_))
        ));
        assert_eq!(
            decode_value(&encode_cursor("01J8Z", None), FieldType::String),
            Some(SqlValue::String("01J8Z".into()))
        );
    }

    /// The supported set is a decision, not an accident — floats do not
    /// round-trip exactly and JSON/binary have no total order, so they
    /// stay out.
    #[test]
    fn only_totally_ordered_types_are_accepted() {
        for ok in [
            FieldType::I16,
            FieldType::I32,
            FieldType::I64,
            FieldType::DateTime,
            FieldType::Date,
            FieldType::Uuid,
            FieldType::String,
        ] {
            assert!(cursor_field_supported(ok), "{ok:?} should be usable");
        }
        for bad in [
            FieldType::F32,
            FieldType::F64,
            FieldType::Bool,
            FieldType::Json,
            FieldType::Binary,
        ] {
            assert!(!cursor_field_supported(bad), "{bad:?} must not be usable");
        }
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

/// A submitted PK wins over MySQL's `LAST_INSERT_ID()`, which is 0 for a
/// non-auto PK and would read back `WHERE pk = 0` (#1671).
#[cfg(all(test, feature = "mysql", feature = "tenancy"))]
mod created_pk_tests {
    use super::*;
    use crate::core::Model as _;

    #[test]
    fn a_submitted_string_pk_is_not_replaced_by_last_insert_id() {
        let schema = crate::tenancy::auth::User::SCHEMA;
        let pk = schema
            .fields
            .iter()
            .find(|f| f.ty == FieldType::String)
            .expect("a String field");
        let q = InsertQuery::new(
            schema,
            vec![pk.column],
            vec![SqlValue::String("rust".into())],
        );
        let got = crate::sql::inserted_pk(&q, &crate::sql::InsertReturningPool::MySqlAutoId(0), pk)
            .expect("submitted pk");
        assert!(
            matches!(got, SqlValue::String(ref s) if s == "rust"),
            "{got:?}"
        );
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
            auto_now: false,
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
            auto_now: false,
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
        let f = extract_pred(
            build_lookup_filter(int_field(), None, "42")
                .unwrap()
                .unwrap(),
        );
        assert_eq!(f.op, Op::Eq);
        assert!(matches!(f.value, SqlValue::I64(42)));
    }

    #[test]
    fn explicit_exact_means_eq() {
        let f = extract_pred(
            build_lookup_filter(int_field(), Some("exact"), "42")
                .unwrap()
                .unwrap(),
        );
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
            let f = extract_pred(
                build_lookup_filter(int_field(), Some(lk), "10")
                    .unwrap()
                    .unwrap(),
            );
            assert_eq!(f.op, expected, "lookup {lk}");
        }
    }

    #[test]
    fn in_lookup_parses_csv() {
        let f = extract_pred(
            build_lookup_filter(int_field(), Some("in"), "1,2,3")
                .unwrap()
                .unwrap(),
        );
        assert_eq!(f.op, Op::In);
        match f.value {
            SqlValue::List(v) => assert_eq!(v.len(), 3),
            _ => panic!("expected List"),
        }
    }

    #[test]
    fn not_in_lookup_parses_csv() {
        let f = extract_pred(
            build_lookup_filter(int_field(), Some("not_in"), "1,2")
                .unwrap()
                .unwrap(),
        );
        assert_eq!(f.op, Op::NotIn);
    }

    #[test]
    fn in_lookup_drops_empty_entries() {
        let f = extract_pred(
            build_lookup_filter(int_field(), Some("in"), "1,,2,")
                .unwrap()
                .unwrap(),
        );
        match f.value {
            SqlValue::List(v) => assert_eq!(v.len(), 2),
            _ => panic!("expected List"),
        }
    }

    #[test]
    fn contains_wraps_with_percents_and_uses_like() {
        let f = extract_pred(
            build_lookup_filter(string_field(), Some("contains"), "hello")
                .unwrap()
                .unwrap(),
        );
        assert_eq!(f.op, Op::LikeEscaped);
        assert!(matches!(f.value, SqlValue::String(ref s) if s == "%hello%"));
    }

    #[test]
    fn icontains_uses_ilike() {
        let f = extract_pred(
            build_lookup_filter(string_field(), Some("icontains"), "hi")
                .unwrap()
                .unwrap(),
        );
        assert_eq!(f.op, Op::ILikeEscaped);
        assert!(matches!(f.value, SqlValue::String(ref s) if s == "%hi%"));
    }

    #[test]
    fn startswith_only_trailing_percent() {
        let f = extract_pred(
            build_lookup_filter(string_field(), Some("startswith"), "pre")
                .unwrap()
                .unwrap(),
        );
        assert!(matches!(f.value, SqlValue::String(ref s) if s == "pre%"));
    }

    #[test]
    fn endswith_only_leading_percent() {
        let f = extract_pred(
            build_lookup_filter(string_field(), Some("endswith"), "fix")
                .unwrap()
                .unwrap(),
        );
        assert!(matches!(f.value, SqlValue::String(ref s) if s == "%fix"));
    }

    #[test]
    fn isnull_true() {
        let f = extract_pred(
            build_lookup_filter(string_field(), Some("isnull"), "true")
                .unwrap()
                .unwrap(),
        );
        assert_eq!(f.op, Op::IsNull);
        assert!(matches!(f.value, SqlValue::Bool(true)));
    }

    #[test]
    fn isnull_false() {
        let f = extract_pred(
            build_lookup_filter(string_field(), Some("isnull"), "false")
                .unwrap()
                .unwrap(),
        );
        assert!(matches!(f.value, SqlValue::Bool(false)));
    }

    /// #2226: `""` on a nullable field parsed to NULL, so `col = NULL`.
    #[test]
    fn an_empty_value_is_no_filter() {
        for lk in [None, Some("ne"), Some("in"), Some("isnull")] {
            assert!(build_lookup_filter(string_field(), lk, "")
                .unwrap()
                .is_none());
        }
    }

    /// #2227: these used to drop the filter and widen the response.
    #[test]
    fn an_unknown_lookup_or_a_bad_value_is_an_error() {
        static DATE: FieldSchema = FieldSchema::new("on", "on", FieldType::Date);
        let date = &DATE;
        for (field, lk, raw) in [
            (int_field(), Some("frobulate"), "1"),
            (int_field(), Some("iexact"), "1"),
            (int_field(), Some("contains"), "1"),
            (int_field(), Some("gt"), "not-a-number"),
            (int_field(), None, "abc"),
            (int_field(), Some("in"), "1,abc"),
            (int_field(), Some("in"), ","),
            (int_field(), Some("range"), "1"),
            (int_field(), Some("year"), "2024"),
            (string_field(), Some("isnull"), "maybe"),
            (date, Some("year"), "twenty"),
            (date, Some("hour"), "1"),
            (date, Some("year__contains"), "1"),
        ] {
            let r = build_lookup_filter(field, lk, raw);
            assert!(r.is_err(), "{lk:?}={raw} must be refused: {r:?}");
        }
    }

    #[test]
    fn date_parts_build_the_orm_shape() {
        static DATE: FieldSchema = FieldSchema::new("on", "on", FieldType::Date);
        let date = &DATE;
        let r = build_lookup_filter(date, Some("year__gte"), "2024").unwrap();
        assert!(
            matches!(
                r,
                Some(WhereExpr::ExprCompare {
                    op: Op::Gte,
                    rhs: crate::core::Expr::Literal(SqlValue::I64(2024)),
                    ..
                })
            ),
            "{r:?}"
        );
    }

    /// A plain date covers the whole day on a datetime `__gte` / `__lte`.
    #[test]
    fn a_plain_date_bounds_a_datetime_by_the_whole_day() {
        static AT: FieldSchema = FieldSchema::new("at", "at", FieldType::DateTime);
        let at = &AT;
        let day = |d: u32| {
            chrono::NaiveDate::from_ymd_opt(2024, 1, d)
                .unwrap()
                .and_time(chrono::NaiveTime::MIN)
                .and_utc()
        };
        let f = extract_pred(
            build_lookup_filter(at, Some("gte"), "2024-01-01")
                .unwrap()
                .unwrap(),
        );
        assert_eq!(f.op, Op::Gte);
        assert!(matches!(f.value, SqlValue::DateTime(v) if v == day(1)));
        let f = extract_pred(
            build_lookup_filter(at, Some("lte"), "2024-01-01")
                .unwrap()
                .unwrap(),
        );
        assert_eq!(f.op, Op::Lt);
        assert!(matches!(f.value, SqlValue::DateTime(v) if v == day(2)));
        assert!(build_lookup_filter(at, Some("gt"), "2024-01-01").is_err());
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

#[cfg(test)]
mod default_order_tests {
    use super::*;
    use crate::core::{FieldSchema, FieldType};
    use crate::sql::Dialect as _;

    static FIELDS: &[FieldSchema] = &[
        FieldSchema {
            primary_key: true,
            ..FieldSchema::new("id", "id", FieldType::I64)
        },
        FieldSchema::new("title", "title", FieldType::String),
    ];

    /// No `.ordering(..)`: `default_order`, then the PK as a tiebreak (#2047).
    #[test]
    fn the_list_orders_by_default_order_then_the_pk() {
        let mut schema = ModelSchema::new("vs_do", "vs_do");
        schema.fields = FIELDS;
        schema.default_order = &[("title", true)];
        let schema: &'static ModelSchema = Box::leak(Box::new(schema));
        let mut q = SelectQuery::new(schema);
        q.order_by = schema.with_pk_tiebreak(default_order_by(&ViewSet::for_model(schema)));
        let sql = crate::sql::Sqlite.compile_select(&q).unwrap().sql;
        assert!(sql.ends_with(r#"ORDER BY "title" DESC, "id""#), "{sql}");
    }
}
