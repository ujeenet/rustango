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
use rustango::sql::UpdaterPool as _;
use rustango::tenancy::{DefaultTenantDb, Org};
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
    /// The **registry**, not a tenant's database — `Org` lives there and
    /// the `Tenant` extractor does not expose it. The only handler that
    /// uses it is the gated `_soak` one below.
    pub registry: rustango::sql::Pool,
    /// Reported on `/_soak/info` so the driver can assert the process
    /// actually applied the deployment's sizing rather than silently
    /// falling back to the defaults (#1456).
    pub pool_cfg: rustango::tenancy::TenantPoolsConfig,
}

#[must_use]
pub fn api(
    queues: Arc<super::supervisor::QueueMap>,
    fail_ratio_pct: u8,
    pool_cfg: rustango::tenancy::TenantPoolsConfig,
    cache: rustango::cache::BoxedCache,
    registry: rustango::sql::Pool,
) -> Router<()> {
    let state = AppState {
        queues,
        fail_ratio_pct,
        pool_cfg,
        registry,
    };

    Router::new()
        .merge(products())
        .merge(orders())
        .merge(orders_raw())
        .merge(order_lines())
        .merge(customers())
        .merge(inventory())
        .route("/api/v1/orders/{id}/confirm", post(confirm_order))
        .merge(storefront(cache))
        .route("/_soak/info", get(soak_info))
        .route("/_soak/jobs", get(soak_jobs))
        .route("/_soak/tenants/{slug}/active", post(soak_set_tenant_active))
        .with_state(state)
}

/// The one cached route, and the one place tenancy makes page caching
/// dangerous.
///
/// `CachePageLayer` keys on `(method, path, vary-on header values)`.
/// Every tenant's storefront is the *same path* — `/shop/products` —
/// so without `vary_on(["host"])` the first tenant to warm the cache
/// serves its catalogue to every other tenant. That is a cross-tenant
/// leak produced by a caching layer doing exactly what it says.
///
/// `cache_authenticated` is deliberately left off: a request carrying
/// `Cookie` or `Authorization` then bypasses the cache entirely, which
/// is right for a storefront that renders a signed-in user's name.
fn storefront(cache: rustango::cache::BoxedCache) -> Router<AppState> {
    Router::new().route("/shop/products", get(views::storefront)).layer(
        rustango::cache_page::CachePageLayer::new(cache)
            .timeout(std::time::Duration::from_secs(30))
            .key_prefix(&cache_namespace())
            .vary_on(["host"]),
    )
}

/// The page cache's key prefix, namespaced per deployment.
///
/// `vary_on(["host"])` separates tenants. It does **not** separate
/// *deployments*, and a shared Redis needs both: the soak fleet runs six
/// app instances — two apps across three dialects, each with its own
/// database — against one Redis, and `/shop/products` is the same path
/// on every one of them. With a bare literal prefix they all shared a
/// key per Host, and a single-tenant instance's catalogue was served to
/// the multi-tenant one under the same tenant hostname. Found by running
/// the fleet and reading the page.
///
/// That is not a soak artifact. Any two deployments pointed at one cache
/// — blue/green, a staging tier sharing prod's Redis, two services
/// behind one hostname — collide the same way, and the symptom is the
/// wrong page rather than an error.
///
/// The crate name separates the two apps; `RUSTANGO_CACHE_NAMESPACE`
/// separates instances of the same app.
fn cache_namespace() -> String {
    let instance = std::env::var("RUSTANGO_CACHE_NAMESPACE").unwrap_or_else(|_| "default".into());
    format!("{}.{instance}.storefront", env!("CARGO_PKG_NAME"))
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
        .cursor_pagination_desc("placed_at")
        .page_size(25)
        .tenant_router("/api/v1/orders")
        .with_state(())
}

/// The same model with **no serializer** — the control for `/orders`.
///
/// This route existed because it had to: until #1454 a serializer could
/// not declare a foreign-key field at all, so a serializer-less ViewSet
/// was the only way to reach the nullable-`bigint` binder (#1450) from
/// HTTP. It is kept now that `OrderSerializer` carries the FK, because
/// the two together are what distinguish "the binder works" from "the
/// serializer happens to hide the column".
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
    tracing::info!(order = id, tenant = %slug, "order queued for fulfilment");
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
async fn soak_info(
    State(st): State<AppState>,
    t: Tenant<DefaultTenantDb>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "app": "platform_commerce_saas",
        "tenancy": "multi",
        "tenant": t.org.slug,
        "dialect": t.pool().dialect().name(),
        "version": env!("CARGO_PKG_VERSION"),
        // #1456. A default here means the sizing never reached the
        // pools, which is a connection-exhaustion outage waiting for
        // the twentieth tenant rather than a cosmetic mismatch.
        "pool": {
            "max_connections": st.pool_cfg.database_pool_max_connections,
            "min_connections": st.pool_cfg.database_pool_min_connections,
            "cache_max": st.pool_cfg.max_cached_database_pools,
        },
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

/// Flip a tenant's `active` flag, so the soak can observe the
/// supervisor's *other* direction.
///
/// The refresh loop learns about a deactivated tenant by its absence
/// from `Org::objects().filter("active", true)` — there is no event to
/// subscribe to. Without something that can produce that absence, the
/// retirement path is code no test ever runs, which is exactly how it
/// came to be missing in the first place.
///
/// Gated on `SOAK_ALLOW_TENANT_MUTATION`, off by default. An endpoint
/// that deactivates tenants is not something an example should mount
/// just because it is convenient for a harness.
async fn soak_set_tenant_active(
    State(st): State<AppState>,
    Path(slug): Path<String>,
    body: String,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if std::env::var("SOAK_ALLOW_TENANT_MUTATION").as_deref() != Ok("1") {
        return Err((
            StatusCode::FORBIDDEN,
            "set SOAK_ALLOW_TENANT_MUTATION=1 to enable this endpoint".to_owned(),
        ));
    }
    let active = body.trim() != "false";

    // The registry, not the calling tenant's own database: `Org` lives
    // there.
    let updated = Org::objects()
        .filter("slug", slug.clone())
        .update()
        .set("active", active)
        .execute_pool(&st.registry)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if updated == 0 {
        return Err((StatusCode::NOT_FOUND, format!("no tenant `{slug}`")));
    }
    tracing::info!(tenant = %slug, active, "tenant active flag changed by _soak endpoint");
    Ok(Json(
        serde_json::json!({ "tenant": slug, "active": active, "updated": updated }),
    ))
}
