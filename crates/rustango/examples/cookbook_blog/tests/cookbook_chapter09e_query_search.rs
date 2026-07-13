//! Cookbook Chapter 9e — searching with the HTTP `QUERY` method (RFC 10008).
//!
//! `QUERY` is the "safe GET with a body": safe + idempotent like `GET`, but
//! the search criteria travel in the request body. That lets a search
//! endpoint accept criteria too big or too structured for a querystring
//! (long filter lists, nested criteria) without pretending to be a `POST`.
//!
//! This chapter builds one product-search handler that serves **both**
//! transports from the same code:
//!
//! - `GET /products?q=cable&max_price=20` — the flat, bookmarkable form.
//! - `QUERY /products` with a JSON body `{"q":"cable","tags":["usb","hdmi"]}`
//!   — arrays / structured criteria that a querystring can't express well.
//!
//! The trick is two rustango pieces working together:
//!
//! | Piece | Role |
//! |---|---|
//! | [`QueryRouterExt::query`] | routes `QUERY` alongside `GET` on one path |
//! | [`Params<T>`] | reads `T` from the querystring on GET and from the body on QUERY |
//!
//! No database, no network — the handler filters an in-memory catalog and
//! the test drives it with [`TestClient`], so this chapter runs anywhere.
//!
//! Run: `cargo test --test cookbook_chapter09e_query_search`
//!
//! [`QueryRouterExt::query`]: rustango::http_query::QueryRouterExt::query
//! [`Params<T>`]: rustango::params::Params
//! [`TestClient`]: rustango::test_client::TestClient

use axum::routing::get;
use axum::Router;
use rustango::http_query::QueryRouterExt as _;
use rustango::params::Params;
use rustango::test_client::TestClient;
use serde::Deserialize;

// --------------------------------------------------------------------------
// The catalog + the search criteria.
// --------------------------------------------------------------------------

#[derive(Clone)]
struct Product {
    name: &'static str,
    price: u32,
    tags: &'static [&'static str],
}

fn catalog() -> Vec<Product> {
    vec![
        Product { name: "USB-C cable", price: 12, tags: &["usb", "cable"] },
        Product { name: "HDMI cable", price: 18, tags: &["hdmi", "cable"] },
        Product { name: "USB hub", price: 35, tags: &["usb", "hub"] },
        Product { name: "Laptop stand", price: 40, tags: &["desk"] },
    ]
}

/// Search criteria. The SAME struct deserializes from a GET querystring
/// (`?q=cable&max_price=20`) and from a QUERY body — urlencoded (flat) or
/// JSON (where `tags` can be a real array).
#[derive(Debug, Default, Deserialize)]
struct ProductQuery {
    /// Case-insensitive substring match on the product name.
    #[serde(default)]
    q: Option<String>,
    /// Upper price bound (inclusive).
    #[serde(default)]
    max_price: Option<u32>,
    /// Match any of these tags. Expressible as a JSON array on QUERY;
    /// urlencoded is single-value, so pass one tag or use a JSON body.
    #[serde(default)]
    tags: Vec<String>,
}

/// One handler, both transports. `Params` sources the criteria from the
/// querystring on GET/HEAD and from the request body on QUERY.
async fn search(Params(query): Params<ProductQuery>) -> axum::Json<serde_json::Value> {
    let names: Vec<&'static str> = catalog()
        .into_iter()
        .filter(|p| {
            query
                .q
                .as_deref()
                .is_none_or(|needle| p.name.to_lowercase().contains(&needle.to_lowercase()))
        })
        .filter(|p| query.max_price.is_none_or(|max| p.price <= max))
        .filter(|p| query.tags.is_empty() || query.tags.iter().any(|t| p.tags.contains(&t.as_str())))
        .map(|p| p.name)
        .collect();
    axum::Json(serde_json::json!({ "results": names }))
}

fn app() -> Router {
    // `get(search).query(search)` mounts the same handler on GET and QUERY.
    Router::new().route("/products", get(search).query(search))
}

fn names(v: &serde_json::Value) -> Vec<String> {
    v["results"]
        .as_array()
        .expect("results array")
        .iter()
        .map(|n| n.as_str().unwrap().to_owned())
        .collect()
}

// --------------------------------------------------------------------------
// §9e.1 — GET and QUERY (urlencoded) are interchangeable.
// --------------------------------------------------------------------------
#[tokio::test]
async fn get_and_query_urlencoded_agree() {
    let client = TestClient::new(app());

    // GET with a querystring.
    let via_get = client.get("/products?q=cable&max_price=15").send().await;
    assert_eq!(via_get.status, 200);
    assert_eq!(names(&via_get.json_value()), vec!["USB-C cable"]);

    // QUERY with the identical criteria in an urlencoded body — same rows.
    let via_query = client
        .query("/products")
        .form(&[("q", "cable"), ("max_price", "15")])
        .send()
        .await;
    assert_eq!(via_query.status, 200);
    assert_eq!(
        names(&via_query.json_value()),
        names(&via_get.json_value()),
        "GET and QUERY must return the same results for the same criteria"
    );
}

// --------------------------------------------------------------------------
// §9e.2 — QUERY unlocks a JSON body: arrays / structured criteria.
// --------------------------------------------------------------------------
#[tokio::test]
async fn query_json_body_matches_multiple_tags() {
    let client = TestClient::new(app());

    // `tags` is a real array here — awkward to express in a querystring,
    // natural in a QUERY JSON body.
    let resp = client
        .query("/products")
        .json(&serde_json::json!({ "tags": ["usb", "hdmi"] }))
        .send()
        .await;
    assert_eq!(resp.status, 200);
    let mut got = names(&resp.json_value());
    got.sort();
    // Everything tagged usb OR hdmi: the two cables + the hub.
    assert_eq!(got, vec!["HDMI cable", "USB hub", "USB-C cable"]);
}

// --------------------------------------------------------------------------
// §9e.3 — an empty QUERY body returns the whole catalog (no criteria).
// --------------------------------------------------------------------------
#[tokio::test]
async fn empty_query_returns_everything() {
    let client = TestClient::new(app());
    let resp = client.query("/products").json(&serde_json::json!({})).send().await;
    assert_eq!(resp.status, 200);
    assert_eq!(names(&resp.json_value()).len(), 4);
}
