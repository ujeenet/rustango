//! Tenant-aware variant of [`ViewSet`] (#80).
//!
//! [`ViewSet::router`] bakes a single `PgPool` at mount time, which
//! is incompatible with multi-tenant routing — schema-mode tenants
//! share the registry pool but rely on a per-checkout `SET
//! search_path`, and database-mode tenants live in entirely
//! separate Postgres databases. Mounting a normal ViewSet against
//! `&pool` from inside a tenant project hits the wrong schema /
//! database on every request.
//!
//! [`ViewSet::tenant_router`] returns a `Router<()>` whose handlers
//! pull a per-request connection from
//! [`crate::extractors::Tenant`] instead. Each handler runs against
//! the connection scoped to whichever tenant the resolver chose
//! for that request.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::viewset::ViewSet;
//! use rustango::core::Model as _;
//!
//! let region_router = ViewSet::for_model(Region::SCHEMA)
//!     .tenant_router("/api/regions");
//!
//! Router::new()
//!     .merge(region_router)
//!     .merge(country_router)
//!     // ... and so on
//! ```
//!
//! ## v1 scope (versus [`ViewSet::router`])
//!
//! - **List** is a full-table fetch: no `filter_fields`, `search_fields`,
//!   `ordering`, or pagination params. The query string is ignored.
//! - **Permission gating** is not built-in. Wrap the merged router
//!   with [`crate::tenancy::middleware::RouterAuthExt::require_auth`]
//!   for authentication, or extract `SessionUser` / `ApiAuth`-shape
//!   inside hand-rolled handlers for fine-grained checks.
//!
//! Anything beyond this surface should still use hand-rolled
//! axum handlers — see the tango regions viewsets for the
//! reference template. A v2 of this module (filters, pagination,
//! perms) is in the backlog.

use std::sync::Arc;

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::Value;

use super::{json_error, json_response, parse_pk_string, row_to_json, ViewSet};
use crate::core::{Filter, Op, SelectQuery, WhereExpr};
use crate::extractors::Tenant;

#[derive(Clone)]
struct TenantState {
    vs: Arc<ViewSet>,
}

impl TenantState {
    fn effective_fields(&self) -> Vec<&'static crate::core::FieldSchema> {
        let schema = self.vs.schema;
        match &self.vs.fields {
            Some(names) => names.iter().filter_map(|n| schema.field(n)).collect(),
            None => schema.scalar_fields().collect(),
        }
    }
}

impl ViewSet {
    /// Build a `Router<()>` whose handlers pull a per-request
    /// connection from [`crate::extractors::Tenant`] instead of a
    /// pool baked at mount time. Use in any project that calls
    /// [`crate::manage::Cli::tenancy`] or wires
    /// [`crate::server::Builder`] — i.e. multi-tenant deployments
    /// where each request resolves to a different tenant's
    /// schema/database.
    ///
    /// See module docs for the v1 scope (no built-in filtering /
    /// pagination / perm checks; hand-roll handlers for those).
    pub fn tenant_router(self, prefix: &str) -> Router<()> {
        let state = TenantState {
            vs: Arc::new(self.clone()),
        };
        let prefix = prefix.trim_end_matches('/').to_owned();
        let collection = prefix.clone();
        let item = format!("{prefix}/{{pk}}");

        let collection_route = if self.read_only {
            get(handle_list_tenant)
        } else {
            get(handle_list_tenant).post(handle_create_tenant)
        };

        let item_route = if self.read_only {
            axum::routing::MethodRouter::new().get(handle_retrieve_tenant)
        } else {
            axum::routing::MethodRouter::new()
                .get(handle_retrieve_tenant)
                .put(handle_update_tenant)
                .patch(handle_partial_update_tenant)
                .delete(handle_destroy_tenant)
        };

        Router::new()
            .route(&collection, collection_route)
            .route(&item, item_route)
            .with_state(state)
    }
}

// ---------- handlers (v1: no filter/search/pagination/perms) ----------

async fn handle_list_tenant(
    axum::extract::State(state): axum::extract::State<TenantState>,
    mut t: Tenant,
) -> Response {
    let select_q = SelectQuery {
        model: state.vs.schema,
        where_clause: WhereExpr::And(vec![]),
        search: None,
        joins: vec![],
        order_by: vec![],
        limit: None,
        offset: None,
    };
    let fields = state.effective_fields();
    match crate::sql::select_rows_on(t.conn(), &select_q).await {
        Ok(rows) => {
            let results: Vec<Value> = match &state.vs.row_render {
                Some(render) => rows.iter().map(|r| (render)(r)).collect(),
                None => rows.iter().map(|row| row_to_json(row, &fields)).collect(),
            };
            Json(serde_json::json!({
                "count": results.len(),
                "results": results,
            }))
            .into_response()
        }
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn handle_retrieve_tenant(
    axum::extract::State(state): axum::extract::State<TenantState>,
    mut t: Tenant,
    Path(pk_raw): Path<String>,
) -> Response {
    let Some(pk_field) = state.vs.schema.primary_key() else {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "model has no primary key",
        );
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
    match crate::sql::select_one_row_on(t.conn(), &select_q).await {
        Ok(Some(row)) => match &state.vs.row_render {
            Some(render) => json_response((render)(&row)),
            None => json_response(row_to_json(&row, &fields)),
        },
        Ok(None) => json_error(StatusCode::NOT_FOUND, "not found"),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn handle_create_tenant(
    axum::extract::State(state): axum::extract::State<TenantState>,
    mut t: Tenant,
    Json(body): Json<serde_json::Map<String, Value>>,
) -> Response {
    use crate::core::InsertQuery;

    let form = json_object_to_form(&body);

    let skip: Vec<&str> = state
        .vs
        .schema
        .scalar_fields()
        .filter(|f| f.primary_key || f.auto)
        .map(|f| f.name)
        .collect();

    let collected = match super::collect_values(state.vs.schema, &form, &skip) {
        Ok(v) => v,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let (columns, values): (Vec<_>, Vec<_>) = collected.into_iter().unzip();

    let pk_field = match state.vs.schema.primary_key() {
        Some(f) => f,
        None => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "model has no primary key",
            )
        }
    };
    let query = InsertQuery {
        model: state.vs.schema,
        columns,
        values,
        returning: vec![pk_field.column],
        on_conflict: None,
    };

    let row = match crate::sql::insert_returning_on(t.conn(), &query).await {
        Ok(r) => r,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, &e.to_string()),
    };

    // Read back the freshly assigned PK.
    use crate::core::{FieldType, SqlValue};
    use sqlx::Row as _;
    let pk_val = match pk_field.ty {
        FieldType::I64 => SqlValue::I64(row.try_get(pk_field.column).unwrap_or(0)),
        FieldType::I32 => SqlValue::I32(row.try_get(pk_field.column).unwrap_or(0)),
        FieldType::I16 => SqlValue::I16(row.try_get(pk_field.column).unwrap_or(0)),
        _ => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "unsupported PK type"),
    };
    let fields = state.effective_fields();
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
    match crate::sql::select_one_row_on(t.conn(), &select_q).await {
        Ok(Some(row)) => {
            let body = match &state.vs.row_render {
                Some(render) => (render)(&row),
                None => row_to_json(&row, &fields),
            };
            (StatusCode::CREATED, Json(body)).into_response()
        }
        Ok(None) | Err(_) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "created but could not retrieve",
        ),
    }
}

async fn handle_update_tenant(
    axum::extract::State(state): axum::extract::State<TenantState>,
    t: Tenant,
    Path(pk_raw): Path<String>,
    Json(body): Json<serde_json::Map<String, Value>>,
) -> Response {
    update_inner(state, t, pk_raw, body, false).await
}

async fn handle_partial_update_tenant(
    axum::extract::State(state): axum::extract::State<TenantState>,
    t: Tenant,
    Path(pk_raw): Path<String>,
    Json(body): Json<serde_json::Map<String, Value>>,
) -> Response {
    update_inner(state, t, pk_raw, body, true).await
}

async fn update_inner(
    state: TenantState,
    mut t: Tenant,
    pk_raw: String,
    body: serde_json::Map<String, Value>,
    partial: bool,
) -> Response {
    use crate::core::{Assignment, UpdateQuery};

    let Some(pk_field) = state.vs.schema.primary_key() else {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "model has no primary key",
        );
    };
    let pk_val = match parse_pk_string(pk_field, &pk_raw) {
        Ok(v) => v,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, &e.to_string()),
    };

    let form = json_object_to_form(&body);
    let mut assignments: Vec<Assignment> = Vec::new();
    for field in state.vs.schema.scalar_fields() {
        if field.primary_key || field.auto {
            continue;
        }
        if partial && !form.contains_key(field.name) {
            continue;
        }
        let raw = form.get(field.name).map(String::as_str);
        match super::parse_form_value(field, raw) {
            Ok(v) => assignments.push(Assignment {
                column: field.column,
                value: v,
            }),
            Err(super::FormError::Missing { .. }) if partial => continue,
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

    if let Err(e) = crate::sql::update_on(t.conn(), &query).await {
        return json_error(StatusCode::BAD_REQUEST, &e.to_string());
    }

    let fields = state.effective_fields();
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
    match crate::sql::select_one_row_on(t.conn(), &select_q).await {
        Ok(Some(row)) => match &state.vs.row_render {
            Some(render) => json_response((render)(&row)),
            None => json_response(row_to_json(&row, &fields)),
        },
        _ => json_error(StatusCode::NOT_FOUND, "not found after update"),
    }
}

async fn handle_destroy_tenant(
    axum::extract::State(state): axum::extract::State<TenantState>,
    mut t: Tenant,
    Path(pk_raw): Path<String>,
) -> Response {
    use crate::core::DeleteQuery;

    let Some(pk_field) = state.vs.schema.primary_key() else {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "model has no primary key",
        );
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
    match crate::sql::delete_on(t.conn(), &query).await {
        Ok(0) => json_error(StatusCode::NOT_FOUND, "not found"),
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// ---------- helpers ----------

/// Flatten a JSON object into the same `HashMap<String, String>` shape
/// the regular ViewSet handlers' `extract_form_body` produces, so we
/// can reuse [`super::collect_values`] and [`super::parse_form_value`]
/// without duplicating their per-field-type parsing logic.
fn json_object_to_form(
    body: &serde_json::Map<String, Value>,
) -> std::collections::HashMap<String, String> {
    body.iter()
        .map(|(k, v)| {
            let s = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            (k.clone(), s)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: building a tenant_router shouldn't panic and
    /// should produce a usable `Router<()>` value. The full
    /// CRUD round-trip is exercised via integration tests
    /// against a real Postgres + tenant pool.
    #[test]
    fn tenant_router_builds_for_a_basic_model() {
        use crate::core::Model as _;
        // Use the framework's own User schema as a stand-in —
        // it's always available and has a PK.
        let _r = ViewSet::for_model(crate::tenancy::auth::User::SCHEMA)
            .read_only()
            .tenant_router("/api/users");
    }
}
