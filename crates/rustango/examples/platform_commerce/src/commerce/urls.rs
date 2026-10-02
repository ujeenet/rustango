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
use rustango::idempotency::{IdempotencyLayer, IdempotencyRouterExt as _};
use rustango::ip_filter::{IpFilterLayer, IpFilterRouterExt as _};
use rustango::jobs::{DatabaseJobQueue, JobQueue as _};
use rustango::sql::Pool;
use rustango::template_views::{CreateView, DeleteView, DetailView, ListView, UpdateView};
use rustango::viewset::ViewSet;

use super::jobs::{self, FlakyPaymentCapture, OrderConfirmation};
use super::models::{Customer, GiftCard, InventoryItem, Order, OrderLine, Product, Promotion};
use super::probes::{self, ProbeResult};
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
pub fn api(
    pool: Pool,
    queue: Arc<DatabaseJobQueue>,
    fail_ratio_pct: u8,
    cache: rustango::cache::BoxedCache,
) -> Router<()> {
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
        .merge(promotions(&pool))
        .merge(gift_cards(&pool))
        .merge(promotion_pages(&pool))
        .route("/api/v1/orders/{id}/confirm", post(confirm_order))
        .merge(payments(cache.clone()))
        .merge(storefront(cache))
        .route("/_soak/info", get(soak_info))
        .route("/_soak/jobs", get(soak_jobs))
        .merge(probe_routes())
        .with_state(state)
}

/// Promotions over the API. The global scope hides `visible = false`
/// rows from list, detail, update and delete (#1746).
fn promotions(pool: &Pool) -> Router<AppState> {
    ViewSet::for_model(Promotion::SCHEMA)
        .filter_fields(&["code", "visible"])
        .search_fields(&["code"])
        .limit_offset_pagination()
        .router_pool("/api/v1/promotions", pool.clone())
        .with_state(())
}

/// The card code is the primary key, and a create keeps it (#1671).
fn gift_cards(pool: &Pool) -> Router<AppState> {
    ViewSet::for_model(GiftCard::SCHEMA)
        .router_pool("/api/v1/gift-cards", pool.clone())
        .with_state(())
}

/// Server-rendered promotion pages. They apply the global scope like
/// the API (#1746), and every POST needs the CSRF token (#1669).
fn promotion_pages(pool: &Pool) -> Router<AppState> {
    let tera = views::promotion_templates();
    let s = Promotion::SCHEMA;
    Router::new()
        .merge(ListView::for_model(s).order_by("id", true).router(
            "/promos",
            tera.clone(),
            pool.clone(),
        ))
        .merge(CreateView::for_model(s).success_url("/promos").router(
            "/promos",
            tera.clone(),
            pool.clone(),
        ))
        .merge(DetailView::for_model(s).router("/promos", tera.clone(), pool.clone()))
        .merge(UpdateView::for_model(s).success_url("/promos").router(
            "/promos",
            tera.clone(),
            pool.clone(),
        ))
        .merge(DeleteView::for_model(s).success_url("/promos").router(
            "/promos",
            tera,
            pool.clone(),
        ))
        .with_state(())
}

/// A retried payment POST must not charge twice, and must not replay
/// another caller's answer (#1668).
fn payments(cache: rustango::cache::BoxedCache) -> Router<AppState> {
    Router::new()
        .route("/api/v1/payments", post(probes::payment))
        .route("/api/v1/refunds", post(probes::payment))
        .idempotency(IdempotencyLayer::new(cache).scope(format!("{}.payments", cache_namespace())))
}

fn probe_routes() -> Router<AppState> {
    Router::new()
        .route("/_soak/ip", get(probes::client_ip))
        // Refuses every IPv4 client. On a dual-stack listener those
        // arrive as `::ffff:a.b.c.d` and must still match (#1673).
        .merge(
            Router::new()
                .route("/_soak/v4-blocked", get(|| async { "reached" }))
                .ip_filter(IpFilterLayer::block(["0.0.0.0/0"]).expect("valid CIDR")),
        )
        .route("/_soak/service-token", get(probes::service_token))
        .route("/_soak/webhooks/probe", post(|Json(b): Json<serde_json::Value>| probes::webhook_probe(b)))
        .route(
            "/_soak/hook-sink/{nonce}",
            get(|Path(n): Path<String>| probes::hook_sink_count(n))
                .post(|Path(n): Path<String>| probes::hook_sink_hit(n)),
        )
        .route("/_soak/scopes/seed", post(|State(st): State<AppState>| async move { probes::scopes_seed(&st.pool).await }))
        .route("/_soak/scopes/row/{pk}", get(scopes_row))
        .route("/_soak/scopes/shortcuts", post(|State(st): State<AppState>| async move { probes::scopes_shortcuts(&st.pool).await }))
        .route("/_soak/audit/probe", post(|State(st): State<AppState>| async move { probes::audit_probe(&st.pool).await }))
        .route("/_soak/dml/bounded", post(|State(st): State<AppState>| async move { probes::dml_bounded(&st.pool).await }))
        .route("/_soak/dml/atomic", post(|State(st): State<AppState>| async move { probes::dml_atomic(&st.pool).await }))
        .route("/_soak/dbcache", post(dbcache))
}

async fn scopes_row(State(st): State<AppState>, Path(pk): Path<i64>) -> ProbeResult {
    probes::scopes_row(&st.pool, pk).await
}

async fn dbcache(State(st): State<AppState>, Json(b): Json<serde_json::Value>) -> ProbeResult {
    probes::dbcache(&st.pool, b).await
}

/// The proxies whose `X-Forwarded-For` this deployment believes, from
/// `TRUSTED_PROXIES` (comma-separated CIDRs). Unset trusts none.
#[must_use]
pub fn real_ip_layer() -> rustango::real_ip::RealIpLayer {
    let nets: Vec<String> = std::env::var("TRUSTED_PROXIES")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    let layer = rustango::real_ip::RealIpLayer::default();
    if nets.is_empty() {
        return layer;
    }
    layer
        .trust_proxies(nets)
        .expect("TRUSTED_PROXIES holds valid CIDRs")
}

/// The one cached route.
///
/// `vary_on(["host"])` is not needed here — one tenant, one catalogue —
/// but it is kept identical to the SaaS twin on purpose. In the
/// multi-tenant app every tenant's storefront is the *same path*, so
/// dropping the vary there lets the first tenant to warm the cache
/// serve its catalogue to all the others: a cross-tenant leak produced
/// by a caching layer doing exactly what it says on the tin. Leaving it
/// here costs one header lookup and keeps the two files comparable.
fn storefront(cache: rustango::cache::BoxedCache) -> Router<AppState> {
    Router::new()
        .route("/shop/products", get(views::storefront))
        .layer(
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

/// The same model with **no serializer** — the control for `/orders`.
///
/// This route existed because it had to: until #1454 a serializer could
/// not declare a foreign-key field at all (`ForeignKey<T>` implemented
/// neither `Deserialize` nor `Default` nor `OpenApiSchema`), so a
/// serializer-less ViewSet was the only route from HTTP to the
/// nullable-`bigint` binder. It is kept now that `OrderSerializer`
/// carries the FK, because the two together distinguish "the binder
/// works" from "the serializer happens to hide the column".
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
