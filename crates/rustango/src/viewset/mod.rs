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
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};

use crate::core::{
    CountQuery, DeleteQuery, FieldType, Filter, InsertQuery, ModelSchema, Op, OrderClause,
    SearchClause, SelectQuery, SqlValue, UpdateQuery, WhereExpr, Assignment,
};
use crate::forms::{collect_values, parse_form_value, parse_pk_string, FormError};
use crate::sql::sqlx::{PgPool, Row as _};

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
}

impl PaginationStyle {
    /// Default — 1-based page numbering.
    #[must_use]
    pub const fn page_number() -> Self { Self::PageNumber }

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
    default_page_size: usize,
    default_ordering: Vec<(String, bool)>,
    perms: ViewSetPerms,
    read_only: bool,
    pagination: PaginationStyle,
}

impl ViewSet {
    /// Start a ViewSet for the given model schema.
    pub fn for_model(schema: &'static ModelSchema) -> Self {
        Self {
            schema,
            fields: None,
            filter_fields: Vec::new(),
            search_fields: Vec::new(),
            default_page_size: 20,
            default_ordering: Vec::new(),
            perms: ViewSetPerms::default(),
            read_only: false,
            pagination: PaginationStyle::PageNumber,
        }
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

    /// Set the pagination strategy explicitly.
    #[must_use]
    pub fn pagination(mut self, style: PaginationStyle) -> Self {
        self.pagination = style;
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

    /// Allow GET only — wires list + retrieve, skips create/update/destroy.
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// Build and return an `axum::Router` mounted at `prefix`.
    ///
    /// The prefix may or may not end with `/` — both `/api/posts` and
    /// `/api/posts/` work identically.
    pub fn router(self, prefix: &str, pool: PgPool) -> Router {
        let state = Arc::new(ViewSetState {
            pool,
            vs: self.clone(),
        });
        let prefix = prefix.trim_end_matches('/').to_owned();
        let collection = format!("{prefix}");
        let item = format!("{prefix}/{{pk}}");

        let collection_route = if self.read_only {
            get(handle_list)
        } else {
            get(handle_list).post(handle_create)
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

#[derive(Clone)]
struct ViewSetState {
    pool: PgPool,
    vs: ViewSet,
}

impl ViewSetState {
    fn effective_fields(&self) -> Vec<&'static crate::core::FieldSchema> {
        let schema = self.vs.schema;
        match &self.vs.fields {
            Some(names) => names
                .iter()
                .filter_map(|n| schema.field(n))
                .collect(),
            None => schema.scalar_fields().collect(),
        }
    }

    async fn check_perm(
        &self,
        codenames: &[String],
        parts: &axum::http::request::Parts,
    ) -> bool {
        if codenames.is_empty() {
            return true;
        }
        // Read the injected AuthenticatedUser if present.
        let Some(auth) = parts
            .extensions
            .get::<crate::tenancy::middleware::AuthenticatedUser>()
        else {
            return false; // not authenticated, permission required
        };
        if auth.is_superuser {
            return true;
        }
        for cn in codenames {
            match crate::tenancy::permissions::has_perm(auth.id, cn, &self.pool).await {
                Ok(true) => return true,
                _ => continue,
            }
        }
        false
    }

    fn pk_field(&self) -> Option<&'static crate::core::FieldSchema> {
        self.vs.schema.primary_key()
    }
}

// ------------------------------------------------------------------ Serialization

fn row_to_json(
    row: &crate::sql::sqlx::postgres::PgRow,
    fields: &[&'static crate::core::FieldSchema],
) -> Value {
    let mut map = serde_json::Map::new();
    for field in fields {
        let value = match field.ty {
            FieldType::I32 => row
                .try_get::<i32, _>(field.column)
                .map(|n| json!(n))
                .unwrap_or(Value::Null),
            FieldType::I64 => row
                .try_get::<i64, _>(field.column)
                .map(|n| json!(n))
                .unwrap_or(Value::Null),
            FieldType::F32 => row
                .try_get::<f32, _>(field.column)
                .map(|n| json!(n))
                .unwrap_or(Value::Null),
            FieldType::F64 => row
                .try_get::<f64, _>(field.column)
                .map(|n| json!(n))
                .unwrap_or(Value::Null),
            FieldType::Bool => row
                .try_get::<bool, _>(field.column)
                .map(|b| json!(b))
                .unwrap_or(Value::Null),
            FieldType::String => row
                .try_get::<String, _>(field.column)
                .map(|s| json!(s))
                .unwrap_or(Value::Null),
            FieldType::Date => row
                .try_get::<chrono::NaiveDate, _>(field.column)
                .map(|d| json!(d.to_string()))
                .unwrap_or(Value::Null),
            FieldType::DateTime => row
                .try_get::<chrono::DateTime<chrono::Utc>, _>(field.column)
                .map(|dt| json!(dt.to_rfc3339()))
                .unwrap_or(Value::Null),
            FieldType::Uuid => row
                .try_get::<uuid::Uuid, _>(field.column)
                .map(|u| json!(u.to_string()))
                .unwrap_or(Value::Null),
            FieldType::Json => row
                .try_get::<serde_json::Value, _>(field.column)
                .unwrap_or(Value::Null),
        };
        map.insert(field.name.to_owned(), value);
    }
    Value::Object(map)
}

fn json_response(body: Value) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn json_error(status: StatusCode, msg: &str) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"error": msg}).to_string()))
        .unwrap()
}

fn json_created(body: Value) -> Response {
    Response::builder()
        .status(StatusCode::CREATED)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
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
    let predicate = |op: Op, value: SqlValue| {
        Some(WhereExpr::Predicate(Filter { column, op, value }))
    };
    match lookup.unwrap_or("exact") {
        "exact" => parse_form_value(field, Some(raw)).ok().and_then(|v| predicate(Op::Eq, v)),
        "ne" => parse_form_value(field, Some(raw)).ok().and_then(|v| predicate(Op::Ne, v)),
        "gt" => parse_form_value(field, Some(raw)).ok().and_then(|v| predicate(Op::Gt, v)),
        "gte" => parse_form_value(field, Some(raw)).ok().and_then(|v| predicate(Op::Gte, v)),
        "lt" => parse_form_value(field, Some(raw)).ok().and_then(|v| predicate(Op::Lt, v)),
        "lte" => parse_form_value(field, Some(raw)).ok().and_then(|v| predicate(Op::Lte, v)),
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
            let op = if lookup == Some("not_in") { Op::NotIn } else { Op::In };
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
    let (parts, _) = req.into_parts();
    if !state.check_perm(&state.vs.perms.list, &parts).await {
        return json_error(StatusCode::FORBIDDEN, "permission denied");
    }

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
        // Skip reserved keys
        if matches!(
            param_key.as_str(),
            "page" | "page_size" | "ordering" | "search" | "cursor"
        ) {
            continue;
        }
        let (field_name, lookup) = match param_key.split_once("__") {
            Some((name, lk)) => (name, Some(lk)),
            None => (param_key.as_str(), None),
        };
        if !state.vs.filter_fields.iter().any(|f| f == field_name) {
            continue;
        }
        let Some(field) = state.vs.schema.field(field_name) else { continue };
        if let Some(predicate) = build_lookup_filter(field, lookup, raw_val) {
            filters.push(predicate);
        }
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
        columns: state.vs.search_fields.iter()
            .filter_map(|n| state.vs.schema.field(n).map(|f| f.column))
            .collect(),
    });

    // Ordering
    let ordering_param = params.get("ordering").cloned();
    let order_by: Vec<OrderClause> = ordering_param
        .as_deref()
        .map(|raw| {
            raw.split(',')
                .filter(|s| !s.is_empty())
                .filter_map(|part| {
                    let (field_name, desc) = if let Some(name) = part.strip_prefix('-') {
                        (name, true)
                    } else {
                        (part, false)
                    };
                    state.vs.schema.field(field_name).map(|f| OrderClause {
                        column: f.column,
                        desc,
                    })
                })
                .collect()
        })
        .unwrap_or_else(|| {
            state.vs.default_ordering.iter()
                .filter_map(|(name, desc)| {
                    state.vs.schema.field(name).map(|f| OrderClause {
                        column: f.column,
                        desc: *desc,
                    })
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

            let select_q = SelectQuery {
                model: state.vs.schema,
                where_clause: where_clause.clone(),
                search: search_clause.clone(),
                joins: vec![],
                order_by: order_by.clone(),
                limit: Some(page_size),
                offset: Some(offset),
            };
            let count_q = CountQuery { model: state.vs.schema, where_clause };

            let (rows_result, count_result) = tokio::join!(
                crate::sql::select_rows(&state.pool, &select_q),
                crate::sql::count_rows(&state.pool, &count_q),
            );
            let rows = match rows_result {
                Ok(r) => r,
                Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
            };
            let count = match count_result {
                Ok(c) => c,
                Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
            };

            let results: Vec<Value> = rows.iter().map(|row| row_to_json(row, &fields)).collect();
            let last_page = ((count - 1).max(0) / page_size) + 1;
            json_response(json!({
                "count": count,
                "page": page,
                "page_size": page_size,
                "last_page": last_page,
                "results": results,
            }))
        }
        PaginationStyle::Cursor { field: cursor_field, desc } => {
            handle_list_cursor(
                state.as_ref(),
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
    }
}

async fn handle_list_cursor(
    state: &ViewSetState,
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
    if !matches!(cursor_schema.ty, FieldType::I32 | FieldType::I64) {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "cursor pagination requires an integer field (i32/i64)",
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
    let order_by = vec![OrderClause {
        column: cursor_schema.column,
        desc,
    }];

    // Fetch page_size+1 to detect if a next page exists.
    let select_q = SelectQuery {
        model: state.vs.schema,
        where_clause: final_where,
        search: search_clause,
        joins: vec![],
        order_by,
        limit: Some(page_size + 1),
        offset: None,
    };
    let rows = match crate::sql::select_rows(&state.pool, &select_q).await {
        Ok(r) => r,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let has_more = rows.len() as i64 > page_size;
    let page_rows = if has_more { &rows[..page_size as usize] } else { &rows[..] };

    let next_cursor = if has_more {
        // Read the cursor field value from the last row in this page
        let last = page_rows.last().expect("non-empty page");
        let val: i64 = match cursor_schema.ty {
            FieldType::I32 => last
                .try_get::<i32, _>(cursor_schema.column)
                .map(i64::from)
                .unwrap_or(0),
            FieldType::I64 => last.try_get::<i64, _>(cursor_schema.column).unwrap_or(0),
            _ => 0,
        };
        Some(encode_cursor(val))
    } else {
        None
    };

    let results: Vec<Value> = page_rows.iter().map(|row| row_to_json(row, &fields)).collect();
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
    let (parts, _) = req.into_parts();
    if !state.check_perm(&state.vs.perms.retrieve, &parts).await {
        return json_error(StatusCode::FORBIDDEN, "permission denied");
    }

    let Some(pk_field) = state.pk_field() else {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "model has no primary key");
    };
    let pk_val = match parse_pk_string(pk_field, &pk_raw) {
        Ok(v) => v,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, &e.to_string()),
    };

    let select_q = SelectQuery {
        model: state.vs.schema,
        where_clause: WhereExpr::Predicate(Filter {
            column: pk_field.column,
            op: Op::Eq,
            value: pk_val,
        }),
        search: None,
        joins: vec![],
        order_by: vec![],
        limit: Some(1),
        offset: None,
    };

    let fields = state.effective_fields();
    match crate::sql::select_one_row(&state.pool, &select_q).await {
        Ok(Some(row)) => json_response(row_to_json(&row, &fields)),
        Ok(None) => json_error(StatusCode::NOT_FOUND, "not found"),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn handle_create(
    State(state): State<Arc<ViewSetState>>,
    req: axum::extract::Request,
) -> Response {
    let (parts, body) = req.into_parts();
    if !state.check_perm(&state.vs.perms.create, &parts).await {
        return json_error(StatusCode::FORBIDDEN, "permission denied");
    }

    let form = match extract_form_body(parts, body).await {
        Ok(f) => f,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, &e),
    };

    let skip: Vec<&str> = state
        .vs
        .schema
        .scalar_fields()
        .filter(|f| f.primary_key || f.auto)
        .map(|f| f.name)
        .collect();

    let collected = match collect_values(state.vs.schema, &form, &skip) {
        Ok(v) => v,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let (columns, values): (Vec<_>, Vec<_>) = collected.into_iter().unzip();

    let pk_field = match state.pk_field() {
        Some(f) => f,
        None => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "model has no primary key"),
    };
    let query = InsertQuery {
        model: state.vs.schema,
        columns,
        values,
        returning: vec![pk_field.column],
        on_conflict: None,
    };

    let row = match crate::sql::insert_returning(&state.pool, &query).await {
        Ok(r) => r,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, &e.to_string()),
    };

    // Fetch the full object to return it
    let pk_val = match pk_field.ty {
        FieldType::I64 => SqlValue::I64(row.try_get(pk_field.column).unwrap_or(0)),
        FieldType::I32 => SqlValue::I32(row.try_get(pk_field.column).unwrap_or(0)),
        _ => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "unsupported PK type"),
    };
    let fields = state.effective_fields();
    match fetch_by_pk(&state, pk_field, pk_val, &fields).await {
        Some(obj) => json_created(obj),
        None => json_error(StatusCode::INTERNAL_SERVER_ERROR, "created but could not retrieve"),
    }
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
    let (parts, body) = req.into_parts();
    if !state.check_perm(&state.vs.perms.update, &parts).await {
        return json_error(StatusCode::FORBIDDEN, "permission denied");
    }

    let Some(pk_field) = state.pk_field() else {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "model has no primary key");
    };
    let pk_val = match parse_pk_string(pk_field, &pk_raw) {
        Ok(v) => v,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, &e.to_string()),
    };

    let form = match extract_form_body(parts, body).await {
        Ok(f) => f,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, &e),
    };

    let mut assignments: Vec<Assignment> = Vec::new();
    for field in state.vs.schema.scalar_fields() {
        if field.primary_key || field.auto {
            continue;
        }
        if partial && !form.contains_key(field.name) {
            continue;
        }
        let raw = form.get(field.name).map(String::as_str);
        match parse_form_value(field, raw) {
            Ok(v) => assignments.push(Assignment { column: field.column, value: v }),
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

    if let Err(e) = crate::sql::update(&state.pool, &query).await {
        return json_error(StatusCode::BAD_REQUEST, &e.to_string());
    }

    let fields = state.effective_fields();
    match fetch_by_pk(&state, pk_field, pk_val, &fields).await {
        Some(obj) => json_response(obj),
        None => json_error(StatusCode::NOT_FOUND, "not found after update"),
    }
}

async fn handle_destroy(
    State(state): State<Arc<ViewSetState>>,
    Path(pk_raw): Path<String>,
    req: axum::extract::Request,
) -> Response {
    let (parts, _) = req.into_parts();
    if !state.check_perm(&state.vs.perms.destroy, &parts).await {
        return json_error(StatusCode::FORBIDDEN, "permission denied");
    }

    let Some(pk_field) = state.pk_field() else {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "model has no primary key");
    };
    let pk_val = match parse_pk_string(pk_field, &pk_raw) {
        Ok(v) => v,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, &e.to_string()),
    };

    let query = DeleteQuery {
        model: state.vs.schema,
        where_clause: WhereExpr::Predicate(Filter {
            column: pk_field.column,
            op: Op::Eq,
            value: pk_val,
        }),
    };

    match crate::sql::delete(&state.pool, &query).await {
        Ok(0) => json_error(StatusCode::NOT_FOUND, "not found"),
        Ok(_) => no_content(),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// ------------------------------------------------------------------ helpers

async fn fetch_by_pk(
    state: &ViewSetState,
    pk_field: &'static crate::core::FieldSchema,
    pk_val: SqlValue,
    fields: &[&'static crate::core::FieldSchema],
) -> Option<Value> {
    let select_q = SelectQuery {
        model: state.vs.schema,
        where_clause: WhereExpr::Predicate(Filter {
            column: pk_field.column,
            op: Op::Eq,
            value: pk_val,
        }),
        search: None,
        joins: vec![],
        order_by: vec![],
        limit: Some(1),
        offset: None,
    };
    crate::sql::select_one_row(&state.pool, &select_q)
        .await
        .ok()
        .flatten()
        .map(|row| row_to_json(&row, fields))
}

/// Extract form data from both `application/x-www-form-urlencoded` and
/// `application/json` request bodies.
async fn extract_form_body(
    parts: axum::http::request::Parts,
    body: Body,
) -> Result<HashMap<String, String>, String> {
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
        // JSON body: flatten top-level string/number values to strings
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        let obj = value.as_object().ok_or("expected a JSON object")?;
        let mut form = HashMap::new();
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
        Ok(form)
    } else {
        // form-urlencoded (default)
        serde_urlencoded::from_bytes::<HashMap<String, String>>(&bytes)
            .map_err(|e| e.to_string())
    }
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
            ("gt", Op::Gt), ("gte", Op::Gte),
            ("lt", Op::Lt), ("lte", Op::Lte), ("ne", Op::Ne),
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
        let f = extract_pred(build_lookup_filter(string_field(), Some("contains"), "hello").unwrap());
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
        let f = extract_pred(build_lookup_filter(string_field(), Some("startswith"), "pre").unwrap());
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
