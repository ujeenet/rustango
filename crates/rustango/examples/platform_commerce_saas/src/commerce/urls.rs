//! Commerce routes, multi-tenant.
//!
//! **Near-twin** of `platform_commerce/src/commerce/urls.rs`. Two
//! differences, and they are the whole difference between a
//! single-tenant and a multi-tenant rustango app:
//!
//! 1. `.tenant_router(prefix)` replaces `.router_pool(prefix, pool)` —
//!    the ViewSet resolves the tenant's pool per request instead of
//!    closing over one.
//! 2. The hand-written handlers take `Tenant<DefaultTenantDb>` and read
//!    `t.pool()` / `t.org.slug` rather than carrying a pool and a fixed
//!    slug in state.
//!
//! Everything else — models, serializers, jobs, the URL map — is byte
//! for byte the same file as the single-tenant app.
//!
//! On a tenant host the framework claims `/admin`, `/login`, `/logout`,
//! `/change-password`, `/_static/*` and `/_brand/*` ahead of this
//! router. `main.rs` calls `RouteConfig::legacy()` to move all of them
//! under `__`, which is what lets a storefront own `/login`.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use axum::Router;
use rustango::core::Model as _;
use rustango::extractors::Tenant;
use rustango::jobs::JobQueue as _;
use rustango::tenancy::DefaultTenantDb;
use rustango::viewset::ViewSet;

use super::jobs::{self, FlakyPaymentCapture, OrderConfirmation};
use super::models::{Customer, InventoryItem, Order, OrderLine, Product};
use super::serializers::{CustomerSerializer, OrderSerializer, ProductSerializer};
use super::views;

/// Shared by the hand-written handlers.
///
/// No pool and no slug here, unlike the single-tenant twin: both come
/// from the request's `Tenant` extractor. The queue map is keyed by
/// slug because the framework gives a job handler no tenant context.
#[derive(Clone)]
pub struct AppState {
    pub queues: Arc<super::supervisor::QueueMap>,
    pub fail_ratio_pct: u8,
}

#[must_use]
pub fn api(queues: Arc<super::supervisor::QueueMap>, fail_ratio_pct: u8) -> Router<()> {
    let state = AppState {
        queues,
        fail_ratio_pct,
    };

    Router::new()
        .merge(products())
        .merge(orders())
        .merge(orders_raw())
        .merge(order_lines())
        .merge(customers())
        .merge(inventory())
        .route("/api/v1/orders/{id}/confirm", post(confirm_order))
        .route("/shop/products", get(views::storefront))
        .route("/_soak/info", get(soak_info))
        .route("/_soak/jobs", get(soak_jobs))
        .with_state(state)
}

fn products() -> Router<AppState> {
    ViewSet::for_model(Product::SCHEMA)
        .serializer::<ProductSerializer>()
        .filter_fields(&["active", "sku"])
        .search_fields(&["sku", "name"])
        .ordering_fields(&["created_at", "price_cents", "sku"])
        .limit_offset_pagination()
        .page_size(25)
        .max_page_size(200)
        .tenant_router("/api/v1/products")
        .with_state(())
}

/// Cursor pagination — deliberately the *other* style, so both
/// paginators are exercised by the same soak.
fn orders() -> Router<AppState> {
    ViewSet::for_model(Order::SCHEMA)
        .serializer::<OrderSerializer>()
        .filter_fields(&["status", "customer_id"])
        .cursor_pagination_desc("id")
        .page_size(25)
        .tenant_router("/api/v1/orders")
        .with_state(())
}

/// The #1450 endpoint: the same model with **no serializer**, because a
/// serializer cannot carry a foreign-key column (#1454). Without one,
/// the ViewSet writes the model's own columns, which is the only route
/// from HTTP to the nullable-`bigint` binder.
fn orders_raw() -> Router<AppState> {
    ViewSet::for_model(Order::SCHEMA)
        .filter_fields(&["status", "customer_id"])
        .limit_offset_pagination()
        .tenant_router("/api/v1/orders-raw")
        .with_state(())
}

/// The bulk-create endpoint (#1403).
fn order_lines() -> Router<AppState> {
    ViewSet::for_model(OrderLine::SCHEMA)
        .filter_fields(&["order_id", "product_id"])
        .page_size(50)
        .tenant_router("/api/v1/order-lines")
        .with_state(())
}

fn customers() -> Router<AppState> {
    ViewSet::for_model(Customer::SCHEMA)
        .serializer::<CustomerSerializer>()
        .filter_fields(&["loyalty_tier"])
        .search_fields(&["email", "full_name"])
        .limit_offset_pagination()
        .tenant_router("/api/v1/customers")
        .with_state(())
}

fn inventory() -> Router<AppState> {
    ViewSet::for_model(InventoryItem::SCHEMA)
        .filter_fields(&["product_id", "location"])
        .limit_offset_pagination()
        .tenant_router("/api/v1/inventory")
        .with_state(())
}

/// Dispatch onto **this tenant's** queue.
///
/// The lookup by slug is the part worth reading: `Job::run(&self)`
/// receives no tenant, so isolation comes from which queue — and
/// therefore which pool — the payload is dispatched to. A tenant with
/// no queue is a tenant provisioned since the last supervisor tick
/// (#1223); answering 503 rather than dispatching into the void is the
/// honest response.
async fn confirm_order(
    State(st): State<AppState>,
    t: Tenant<DefaultTenantDb>,
    Path(id): Path<i64>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    let slug = t.org.slug.clone();
    let queue = st.queues.get(&slug).ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        format!("no worker queue for tenant `{slug}` yet"),
    ))?;

    queue
        .dispatch(&OrderConfirmation {
            tenant: slug.clone(),
            order_id: id,
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    queue
        .dispatch(&FlakyPaymentCapture {
            tenant: slug.clone(),
            order_id: id,
            fail_ratio_pct: st.fail_ratio_pct,
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if id % 50 == 0 {
        queue
            .dispatch(&jobs::FatalProbe {
                tenant: slug.clone(),
                order_id: id,
            })
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "order_id": id, "tenant": slug, "queued": true })),
    ))
}

/// Which tenant did this request actually reach?
///
/// The harness asserts on `tenant` here rather than trusting the `Host`
/// header it sent: the resolver caches negative results for 30s, so
/// "the tenant I meant" and "the tenant I got" can differ for a minute
/// after provisioning.
async fn soak_info(t: Tenant<DefaultTenantDb>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "app": "platform_commerce_saas",
        "tenancy": "multi",
        "tenant": t.org.slug,
        "dialect": t.pool().dialect().name(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn soak_jobs(
    State(st): State<AppState>,
    t: Tenant<DefaultTenantDb>,
) -> Json<serde_json::Value> {
    let slug = t.org.slug.clone();
    let pending = match st.queues.get(&slug) {
        Some(q) => Some(q.pending_count().await),
        None => None,
    };
    Json(serde_json::json!({
        "tenant": slug,
        "pending": pending,
        "registered": jobs::registered_job_names(),
        "pools": jobs::registered_slugs(),
        "queues": st.queues.slugs(),
    }))
}
