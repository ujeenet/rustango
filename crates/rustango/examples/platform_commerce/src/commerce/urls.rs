//! Commerce routes.
//!
//! **Near-twin.** `platform_commerce_saas/src/commerce/urls.rs` is this
//! file with `.router_pool(prefix, pool)` replaced by
//! `.tenant_router(prefix)` and the pool argument dropped. That one
//! substitution is the entire difference between a single-tenant and a
//! multi-tenant rustango app, and keeping it the *only* difference is
//! why these two examples exist side by side.
//!
//! Everything is namespaced under `/api/v1` and `/shop`. On a tenant
//! host the framework claims `/admin`, `/login`, `/logout`,
//! `/change-password`, `/_static/*` and `/_brand/*` before your router
//! sees them, so a storefront that wants its own `/login` has to either
//! move the console (the SaaS app calls `RouteConfig::legacy()`) or
//! stay out of those paths. This app does both.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use axum::Router;
use rustango::core::Model as _;
use rustango::jobs::{DatabaseJobQueue, JobQueue as _};
use rustango::sql::Pool;
use rustango::viewset::ViewSet;

use super::jobs::{self, FlakyPaymentCapture, OrderConfirmation};
use super::models::{Customer, InventoryItem, Order, OrderLine, Product};
use super::serializers::{CustomerSerializer, OrderSerializer, ProductSerializer};
use super::views;

/// Shared by the hand-written handlers.
#[derive(Clone)]
pub struct AppState {
    pub pool: Pool,
    pub queue: Arc<DatabaseJobQueue>,
    /// `__single__` here; the resolved tenant slug in the SaaS twin.
    pub slug: String,
    /// Percentage of payment captures that fail every attempt. Driven
    /// from `SOAK_FAIL_RATIO_PCT` so the harness can predict the
    /// dead-letter count exactly.
    pub fail_ratio_pct: u8,
}

#[must_use]
pub fn api(pool: Pool, queue: Arc<DatabaseJobQueue>, fail_ratio_pct: u8) -> Router<()> {
    let state = AppState {
        pool: pool.clone(),
        queue,
        slug: jobs::SINGLE.to_owned(),
        fail_ratio_pct,
    };

    Router::new()
        .merge(products(&pool))
        .merge(orders(&pool))
        .merge(orders_raw(&pool))
        .merge(order_lines(&pool))
        .merge(customers(&pool))
        .merge(inventory(&pool))
        .route("/api/v1/orders/{id}/confirm", post(confirm_order))
        .route("/shop/products", get(views::storefront))
        .route("/_soak/info", get(soak_info))
        .route("/_soak/jobs", get(soak_jobs))
        .with_state(state)
}

/// Limit/offset pagination, filtering and search.
fn products(pool: &Pool) -> Router<AppState> {
    ViewSet::for_model(Product::SCHEMA)
        .serializer::<ProductSerializer>()
        .filter_fields(&["active", "sku"])
        .search_fields(&["sku", "name"])
        .ordering_fields(&["created_at", "price_cents", "sku"])
        .limit_offset_pagination()
        .page_size(25)
        .max_page_size(200)
        .router_pool("/api/v1/products", pool.clone())
        .with_state(())
}

/// Cursor pagination — deliberately the *other* style, so both
/// paginators are exercised by the same soak.
///
/// Keyed on `placed_at`, which is the natural cursor for an
/// append-only table. That column 500'd on every request until #1459 —
/// cursor pagination accepted any field name at build time and then
/// rejected non-integers per request — and this endpoint was dead on
/// all six instances until the soak's first run found it.
fn orders(pool: &Pool) -> Router<AppState> {
    ViewSet::for_model(Order::SCHEMA)
        .serializer::<OrderSerializer>()
        .filter_fields(&["status", "customer_id"])
        .cursor_pagination_desc("placed_at")
        .page_size(25)
        .router_pool("/api/v1/orders", pool.clone())
        .with_state(())
}

/// The #1450 endpoint: the same model with **no serializer**.
///
/// It exists because a serializer cannot carry a foreign-key column —
/// `ForeignKey<T>` implements neither `Deserialize` nor `Default` nor
/// `OpenApiSchema`, so no serializer field can name `assigned_picker_id`
/// (see `serializers.rs`). Without a serializer the ViewSet writes the
/// model's own columns, which is the only route from HTTP to the
/// nullable-`bigint` binder.
///
/// `POST` an order with `assigned_picker_id` absent, and `PATCH` one
/// with it explicitly `null`: two separate binder call sites, both of
/// which failed on PostgreSQL before 0.57.5 with *"column
/// assigned_picker_id is of type bigint but expression is of type
/// text"*.
fn orders_raw(pool: &Pool) -> Router<AppState> {
    ViewSet::for_model(Order::SCHEMA)
        .filter_fields(&["status", "customer_id"])
        .limit_offset_pagination()
        .router_pool("/api/v1/orders-raw", pool.clone())
        .with_state(())
}

/// The bulk-create endpoint. A JSON **array** body inserts many rows in
/// one transaction; one constraint violation must roll the whole batch
/// back and leave zero rows (#1403). `unique_together(order_id,
/// product_id)` on the model is what makes that provokable from a test.
fn order_lines(pool: &Pool) -> Router<AppState> {
    ViewSet::for_model(OrderLine::SCHEMA)
        .filter_fields(&["order_id", "product_id"])
        .page_size(50)
        .router_pool("/api/v1/order-lines", pool.clone())
        .with_state(())
}

fn customers(pool: &Pool) -> Router<AppState> {
    ViewSet::for_model(Customer::SCHEMA)
        .serializer::<CustomerSerializer>()
        .filter_fields(&["loyalty_tier"])
        .search_fields(&["email", "full_name"])
        .limit_offset_pagination()
        .router_pool("/api/v1/customers", pool.clone())
        .with_state(())
}

/// Writable on purpose: the reconciliation job writes the same rows, so
/// the soak generates real contention rather than simulating it.
fn inventory(pool: &Pool) -> Router<AppState> {
    ViewSet::for_model(InventoryItem::SCHEMA)
        .filter_fields(&["product_id", "location"])
        .limit_offset_pagination()
        .router_pool("/api/v1/inventory", pool.clone())
        .with_state(())
}

/// Dispatch the per-order jobs and return 202.
///
/// Three jobs per confirmed order: the happy path, the deterministic
/// flaky one, and — for one order in fifty — the fatal probe, so the
/// soak sees `JobError::Fatal` bypass retry without drowning in it.
async fn confirm_order(
    State(st): State<AppState>,
    Path(id): Path<i64>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    st.queue
        .dispatch(&OrderConfirmation {
            tenant: st.slug.clone(),
            order_id: id,
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    st.queue
        .dispatch(&FlakyPaymentCapture {
            tenant: st.slug.clone(),
            order_id: id,
            fail_ratio_pct: st.fail_ratio_pct,
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if id % 50 == 0 {
        st.queue
            .dispatch(&jobs::FatalProbe {
                tenant: st.slug.clone(),
                order_id: id,
            })
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    tracing::info!(order = id, tenant = %st.slug, "order queued for fulfilment");
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "order_id": id, "queued": true })),
    ))
}

/// What this instance is, so the harness can assert it reached the
/// instance it meant to.
async fn soak_info(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "app": "platform_commerce",
        "tenancy": "single",
        "tenant": st.slug,
        "dialect": st.pool.dialect().name(),
        "version": env!("CARGO_PKG_VERSION"),
        "fail_ratio_pct": st.fail_ratio_pct,
    }))
}

/// Queue depth. Note `pending_count()` on the database queue counts
/// only rows with `locked_at IS NULL`, so a job being worked on right
/// now is invisible here — which is exactly why the harness also counts
/// `ShipmentEvent` rows rather than trusting this number alone.
async fn soak_jobs(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "pending": st.queue.pending_count().await,
        "registered": jobs::registered_job_names(),
        "pools": jobs::registered_slugs(),
    }))
}
