//! Hand-written handlers. **Twin file** in `platform_commerce_saas`.

use axum::extract::State;
use axum::response::Html;
use rustango::sql::FetcherPool as _;

use super::models::Product;
use super::urls::AppState;

/// The storefront.
///
/// Deliberately a server-rendered page rather than JSON: it is what the
/// Redis page cache is pointed at, and a cache that only ever serves
/// JSON to a load generator proves less than one serving the page a
/// browser would get.
pub async fn storefront(State(st): State<AppState>) -> Html<String> {
    let products = Product::objects()
        .filter("active", true)
        .order_by(&[("sku", false)])
        .limit(50)
        .fetch(&st.pool)
        .await
        .unwrap_or_default();

    tracing::debug!(products = products.len(), "rendering storefront");
    let mut body =
        String::from("<!doctype html>\n<title>Commerce</title>\n<h1>Catalogue</h1>\n<ul>\n");
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
