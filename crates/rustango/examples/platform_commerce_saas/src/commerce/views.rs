//! Hand-written handlers, multi-tenant.
//!
//! **Near-twin** of `platform_commerce/src/commerce/views.rs`: the only
//! difference is where the pool comes from. Single-tenant closes over
//! one; here the `Tenant` extractor supplies the request's own.

use axum::response::Html;
use rustango::extractors::Tenant;
use rustango::sql::FetcherPool as _;
use rustango::tenancy::DefaultTenantDb;

use super::models::Product;

/// The storefront.
///
/// Server-rendered rather than JSON on purpose: it is what the Redis
/// page cache is pointed at, and a cache that only ever serves JSON to
/// a load generator proves less than one serving the page a browser
/// would get.
///
/// The cache itself is a `CachePageLayer` wrapped around this route in
/// `urls.rs`, built from `[cache]` in the config tiers. Nothing here
/// knows about it — which is the point, and also why the claim above
/// went unnoticed while it was false: for several commits this app
/// carried the `cache-redis` feature, ran a Redis container, and cached
/// nothing at all.
pub async fn storefront(t: Tenant<DefaultTenantDb>) -> Html<String> {
    let products = Product::objects()
        .filter("active", true)
        .order_by(&[("sku", false)])
        .limit(50)
        .fetch(t.pool())
        .await
        .unwrap_or_default();

    tracing::debug!(tenant = %t.org.slug, products = products.len(), "rendering storefront");
    let mut body = format!(
        "<!doctype html>\n<title>Commerce — {}</title>\n<h1>Catalogue — {}</h1>\n<ul>\n",
        t.org.slug, t.org.slug
    );
    for p in &products {
        body.push_str(&format!(
            "  <li><code>{}</code> — {} — {}.{:02}</li>\n",
            p.sku,
            p.name,
            p.price_cents / 100,
            p.price_cents % 100
        ));
    }
    body.push_str("</ul>\n");
    Html(body)
}
