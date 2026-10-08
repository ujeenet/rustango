//! `ViewSet` `filter_fields` query params on every backend.

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    any(feature = "admin", feature = "tenancy")
))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rustango::core::Model as _;
use rustango::sql::{Auto, Pool};
use rustango::{tri_dialect_test, Model};
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(table = "vs_filters_item")]
#[rustango(app = "viewset_filters_tri")]
pub struct Item {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 100)]
    pub name: String,
    pub category_id: Option<i64>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Item>(pool).await;
    for (name, category_id, at) in [
        ("alpha", Some(1), "2024-01-01T00:00:00Z"),
        ("beta", Some(2), "2024-01-31T23:30:00Z"),
        ("gamma", None, "2024-02-01T00:00:00Z"),
    ] {
        Item {
            id: Auto::default(),
            name: name.into(),
            category_id,
            created_at: at.parse().expect("instant"),
        }
        .insert_pool(pool)
        .await
        .expect("seed");
    }
}

fn app(pool: &Pool) -> axum::Router {
    rustango::viewset::ViewSet::for_model(Item::SCHEMA)
        .filter_fields(&["category_id", "name", "created_at"])
        .router_pool("/items", pool.clone())
}

/// Status and the result names, in id order.
async fn get(pool: &Pool, uri: &str) -> (StatusCode, Vec<String>, String) {
    let resp = app(pool)
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&bytes).into_owned();
    let names = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v["results"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .map(|r| r["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    (status, names, body)
}

/// #2226: `?category_id=` used to compare to NULL and return no rows.
async fn an_empty_value_is_no_filter(pool: &Pool) {
    let (status, names, body) = get(pool, "/items?category_id=").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(names, ["alpha", "beta", "gamma"], "{body}");
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        an_empty_value_is_no_filter,
    ],
}
