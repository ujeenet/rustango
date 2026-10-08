//! `ViewSet` `filter_fields` query params on every backend, and the
//! nullable-cursor handling.

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    any(feature = "admin", feature = "tenancy")
))]

use axum::body::Body;
use std::collections::HashMap;

use axum::http::{Request, StatusCode};
use rustango::core::Model as _;
use rustango::core::{Filter, ModelSchema, Op, SqlValue, WhereExpr};
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
    pub price: i64,
}

async fn seed(pool: &Pool, name: &str, category_id: Option<i64>, at: &str, price: i64) {
    Item {
        id: Auto::default(),
        name: name.into(),
        category_id,
        created_at: at.parse().expect("instant"),
        price,
    }
    .insert_pool(pool)
    .await
    .expect("seed");
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Item>(pool).await;
    seed(pool, "alpha", Some(1), "2024-01-01T00:00:00Z", 10).await;
    seed(pool, "beta", Some(2), "2024-01-31T23:30:00Z", 20).await;
    seed(pool, "gamma", None, "2024-02-01T00:00:00Z", 30).await;
}

/// A custom backend owning `price__min` on a field in `filter_fields`.
fn price_min(params: &HashMap<String, String>, _: &'static ModelSchema) -> Vec<WhereExpr> {
    params
        .get("price__min")
        .and_then(|v| v.parse::<i64>().ok())
        .map(|min| WhereExpr::Predicate(Filter::new("price", Op::Gte, SqlValue::I64(min))))
        .into_iter()
        .collect()
}

fn app(pool: &Pool) -> axum::Router {
    use rustango::viewset::ViewSet;
    ViewSet::for_model(Item::SCHEMA)
        .filter_fields(&["category_id", "name", "created_at"])
        .router_pool("/items", pool.clone())
        .merge(
            ViewSet::for_model(Item::SCHEMA)
                .filter_fields(&["price"])
                .filter_backend(price_min)
                .router_pool("/priced", pool.clone()),
        )
        .merge(
            ViewSet::for_model(Item::SCHEMA)
                .filter_fields(&["category_id"])
                .cursor_pagination("category_id")
                .router_pool("/by_cat", pool.clone()),
        )
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

/// #2227: a bad value or an unknown lookup used to drop the filter.
async fn a_bad_filter_is_400_naming_the_param(pool: &Pool) {
    for uri in [
        "/items?category_id=abc",
        "/items?category_id__in=1,abc",
        "/items?category_id__frobulate=1",
        "/items?name__regex=a",
        "/items?created_at__gt=2024-01-31",
        "/items?name__year=2024",
    ] {
        let (status, _, body) = get(pool, uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
        let param = uri.split(['?', '=']).nth(1).unwrap();
        assert!(body.contains(param), "{uri}: {body}");
    }
}

/// #2227: ORM lookups the ViewSet did not know.
async fn orm_lookups_filter(pool: &Pool) {
    for (uri, want) in [
        ("/items?name__iexact=ALPHA", &["alpha"][..]),
        ("/items?category_id__range=2,5", &["beta"]),
        ("/items?created_at__date=2024-01-31", &["beta"]),
        ("/items?created_at__month=1", &["alpha", "beta"]),
        (
            "/items?created_at__year__gte=2024",
            &["alpha", "beta", "gamma"],
        ),
        (
            "/items?created_at__date__gte=2024-01-31",
            &["beta", "gamma"],
        ),
        // A plain date is the whole UTC day on a datetime.
        ("/items?created_at__gte=2024-01-31", &["beta", "gamma"]),
        ("/items?created_at__lte=2024-01-31", &["alpha", "beta"]),
    ] {
        let (status, names, body) = get(pool, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert_eq!(names, want, "{uri}: {body}");
    }
}

/// #2264: with a filter backend, an unknown lookup is the backend's key.
async fn a_backend_owns_unknown_lookups(pool: &Pool) {
    let (status, names, body) = get(pool, "/priced?price__min=20").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(names, ["beta", "gamma"], "{body}");
    // A lookup the ViewSet knows still refuses a bad value.
    let (status, _, body) = get(pool, "/priced?price__gte=abc").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

/// #2264: LIKE on a non-string column was a DB error on PostgreSQL.
async fn a_like_lookup_on_a_number_is_400(pool: &Pool) {
    for uri in [
        "/items?category_id__iexact=1",
        "/items?category_id__contains=1",
    ] {
        let (status, _, body) = get(pool, uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
    }
}

/// #2230: a nullable cursor still pages; only a NULL at the page end fails.
async fn a_nullable_cursor_fails_only_on_a_null_row(pool: &Pool) {
    let (status, names, body) = get(pool, "/by_cat?category_id__isnull=false&page_size=1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(names, ["alpha"], "{body}");
    assert!(body.contains(r#""next":""#), "{body}");

    seed(pool, "delta", None, "2024-03-01T00:00:00Z", 40).await;
    #[cfg(feature = "runtime")]
    let (buf, _guard) = {
        let buf = rustango::testkit::CaptureWriter::default();
        let writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        (buf, tracing::subscriber::set_default(subscriber))
    };
    let (status, _, body) = get(pool, "/by_cat?category_id__isnull=true&page_size=1").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    // The cause is logged, not sent.
    #[cfg(feature = "runtime")]
    assert!(
        buf.contents().contains("is NULL in this row"),
        "{}",
        buf.contents()
    );
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        an_empty_value_is_no_filter,
        a_bad_filter_is_400_naming_the_param,
        orm_lookups_filter,
        a_backend_owns_unknown_lookups,
        a_like_lookup_on_a_number_is_400,
        a_nullable_cursor_fails_only_on_a_null_row,
    ],
}

/// #2230: logged at build, not refused, until 0.61.0.
#[cfg(feature = "runtime")]
#[test]
fn a_nullable_cursor_column_is_logged_at_build_time() {
    let buf = rustango::testkit::CaptureWriter::default();
    let writer = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .with_max_level(tracing::Level::ERROR)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let _ =
            rustango::viewset::ViewSet::for_model(Item::SCHEMA).cursor_pagination("category_id");
    });
    let logs = buf.contents();
    assert!(
        logs.contains("ERROR") && logs.contains("is nullable"),
        "{logs}"
    );
}
