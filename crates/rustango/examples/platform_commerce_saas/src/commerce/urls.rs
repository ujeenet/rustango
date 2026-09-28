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
use rustango::extractors::{SessionUser, Tenant};
use rustango::idempotency::{IdempotencyLayer, IdempotencyRouterExt as _};
use rustango::ip_filter::{IpFilterLayer, IpFilterRouterExt as _};
use rustango::jobs::JobQueue as _;
use rustango::real_ip::RealIpRouterExt as _;
use rustango::sql::UpdaterPool as _;
use rustango::template_views::{CreateView, DeleteView, DetailView, ListView, UpdateView};
use rustango::tenancy::auth_routes::{Config as JwtConfig, JwtAuth};
use rustango::tenancy::{DefaultTenantDb, Org};
use rustango::viewset::ViewSet;

use super::jobs::{self, FlakyPaymentCapture, OrderConfirmation};
use super::models::{Customer, GiftCard, InventoryItem, Order, OrderLine, Product, Promotion};
use super::probes::{self, ProbeResult};
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
        .merge(promotions())
        .merge(gift_cards())
        .merge(promotion_pages())
        .route("/api/v1/orders/{id}/confirm", post(confirm_order))
        .merge(payments(cache.clone()))
        .merge(storefront(cache))
        .route("/_soak/info", get(soak_info))
        .route("/_soak/jobs", get(soak_jobs))
        .route("/_soak/tenants/{slug}/active", post(soak_set_tenant_active))
        .merge(probe_routes())
        .with_state(state)
        // Login, refresh, logout and `me` over JWT, on every tenant.
        .merge(jwt_auth().router())
        .real_ip(real_ip_layer())
}

/// One `JwtAuth` for the process: a second one would keep its own
/// revocation list, and a logout through it would not stick (#1190).
fn jwt_auth() -> JwtAuth {
    JwtAuth::new(JwtConfig::default())
}

/// Promotions over the API. The global scope hides `visible = false`
/// rows from list, detail, update and delete (#1746).
fn promotions() -> Router<AppState> {
    ViewSet::for_model(Promotion::SCHEMA)
        .filter_fields(&["code", "visible"])
        .search_fields(&["code"])
        .limit_offset_pagination()
        .tenant_router("/api/v1/promotions")
        .with_state(())
}

/// The card code is the primary key, and a create keeps it (#1671).
fn gift_cards() -> Router<AppState> {
    ViewSet::for_model(GiftCard::SCHEMA)
        .tenant_router("/api/v1/gift-cards")
        .with_state(())
}

/// Server-rendered promotion pages. They apply the global scope like
/// the API (#1746), and every POST needs the CSRF token (#1669).
fn promotion_pages() -> Router<AppState> {
    let tera = views::promotion_templates();
    let s = Promotion::SCHEMA;
    Router::new()
        .merge(ListView::for_model(s).tenant_router("/promos", tera.clone()))
        .merge(CreateView::for_model(s).success_url("/promos").tenant_router("/promos", tera.clone()))
        .merge(DetailView::for_model(s).tenant_router("/promos", tera.clone()))
        .merge(UpdateView::for_model(s).success_url("/promos").tenant_router("/promos", tera.clone()))
        .merge(DeleteView::for_model(s).success_url("/promos").tenant_router("/promos", tera))
        .with_state(())
}

/// A retried payment POST must not charge twice, and must not replay
/// another caller's or another tenant's answer (#1668).
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
        .route("/_soak/whoami", get(whoami))
        .route("/_soak/webhooks/probe", post(|Json(b): Json<serde_json::Value>| probes::webhook_probe(b)))
        .route(
            "/_soak/hook-sink/{nonce}",
            get(|Path(n): Path<String>| probes::hook_sink_count(n))
                .post(|Path(n): Path<String>| probes::hook_sink_hit(n)),
        )
        .route("/_soak/scopes/seed", post(|t: Tenant<DefaultTenantDb>| async move { probes::scopes_seed(t.pool()).await }))
        .route("/_soak/scopes/row/{pk}", get(scopes_row))
        .route("/_soak/scopes/shortcuts", post(|t: Tenant<DefaultTenantDb>| async move { probes::scopes_shortcuts(t.pool()).await }))
        .route("/_soak/audit/probe", post(|t: Tenant<DefaultTenantDb>| async move { probes::audit_probe(t.pool()).await }))
        .route("/_soak/dml/bounded", post(|t: Tenant<DefaultTenantDb>| async move { probes::dml_bounded(t.pool()).await }))
        .route("/_soak/dml/atomic", post(|t: Tenant<DefaultTenantDb>| async move { probes::dml_atomic(t.pool()).await }))
        .route("/_soak/dbcache", post(dbcache))
        .route("/_soak/sso/seed", post(sso_seed))
}

async fn scopes_row(t: Tenant<DefaultTenantDb>, Path(pk): Path<i64>) -> ProbeResult {
    probes::scopes_row(t.pool(), pk).await
}

/// The registry is MySQL on `saas-my`, which is where long keys used
/// to truncate (#1674).
async fn dbcache(State(st): State<AppState>, Json(b): Json<serde_json::Value>) -> ProbeResult {
    probes::dbcache(&st.registry, b).await
}

/// Who the tenant session cookie belongs to, via `SessionUser`, which
/// must resolve on every backend.
async fn whoami(t: Tenant<DefaultTenantDb>, user: SessionUser) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "tenant": t.org.slug,
        "user": user.0.map(|u| u.username),
    }))
}

/// SSO providers against the soak's fake IdP, and emails on the three
/// SSO users `bootstrap.sh` created. Idempotent.
async fn sso_seed(t: Tenant<DefaultTenantDb>) -> ProbeResult {
    use rustango::sql::{Auto, FetcherPool as _};
    use rustango::sso::SsoProvider;
    use rustango::tenancy::auth::User;
    probes::gate()?;
    let pool = t.pool();
    let err = |e: &dyn std::fmt::Display| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    let issuer = std::env::var("SOAK_IDP_ISSUER").unwrap_or_else(|_| "http://idp:9000".into());
    // `idp-strict` never links by email; `idp-link` opts in.
    for (slug, allow_email_link) in [("idp-strict", false), ("idp-link", true)] {
        let have: Vec<SsoProvider> = SsoProvider::objects()
            .filter("slug", slug)
            .fetch(pool)
            .await
            .map_err(|e| err(&e))?;
        if !have.is_empty() {
            continue;
        }
        let mut row = SsoProvider {
            id: Auto::default(),
            slug: slug.into(),
            label: format!("Sign in with {slug}"),
            kind: "oidc".into(),
            issuer_url: Some(issuer.clone()),
            client_id: "soak-client".into(),
            client_secret: rustango::casts::Cast::new("soak-client-secret".into()),
            enabled: true,
            sort_order: 0,
            scopes: None,
            allow_email_link,
            created_at: Auto::default(),
            updated_at: Auto::default(),
        };
        row.insert_pool(pool).await.map_err(|e| err(&e))?;
    }
    let mut users = serde_json::Map::new();
    for name in ["sso-user", "sso-staff", "sso-super", "sso-other"] {
        let found: Vec<User> = User::objects()
            .filter("username", name)
            .fetch(pool)
            .await
            .map_err(|e| err(&e))?;
        let Some(mut u) = found.into_iter().next() else {
            continue;
        };
        u.email = Some(format!("{name}@{}.example.test", t.org.slug));
        u.save_pool(pool).await.map_err(|e| err(&e))?;
        let id = u.id.get().copied().unwrap_or_default();
        if name == "sso-staff" {
            // Staff: holds permissions, including on the link table itself.
            for code in ["rustango_sso_links.add", "rustango_sso_links.change", "rustango_sso_links.view"] {
                rustango::tenancy::permissions::set_user_perm_pool(id, code, true, pool)
                    .await
                    .map_err(|e| err(&e))?;
            }
        }
        users.insert(name.into(), serde_json::json!({ "id": id, "email": u.email }));
    }
    Ok(Json(serde_json::json!({ "issuer": issuer, "users": users })))
}

/// The proxies whose `X-Forwarded-For` this deployment believes, from
/// `TRUSTED_PROXIES` (comma-separated CIDRs). Unset trusts none.
fn real_ip_layer() -> rustango::real_ip::RealIpLayer {
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
    layer.trust_proxies(nets).expect("TRUSTED_PROXIES holds valid CIDRs")
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
            "scoped_cache_max": st.pool_cfg.max_cached_scoped_pools,
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
