//! `ViewSet::max_page_size` — the page ceiling is the app's, not a hard-coded
//! 1000 (#1196).
//!
//! An app that sets `page_size(20)` has sized its serializer, joins and
//! response budget around 20 rows. With the old fixed ceiling any client could
//! ask for 1000 and get a 50× amplification of all of it — and if the
//! serializer does per-row work that touches the database, that is an N+1
//! becoming a thousand queries in one request. It was reachable by any
//! authenticated caller, which made it the cheapest way to make a rustango app
//! do expensive work.

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "serializer"))]

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use rustango::core::Model as _;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::viewset::ViewSet;
use rustango::Model;
use serde_json::Value;
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone, serde::Serialize, serde::Deserialize)]
#[rustango(table = "vs_mps_note")]
#[rustango(app = "vs_mps_app")]
pub struct Note {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

/// 250 rows — more than the new default ceiling (100), so a clamp is visible.
async fn pool_with_rows() -> Pool {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite");
    sqlx::query(
        "CREATE TABLE vs_mps_note (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL)",
    )
    .execute(&sq)
    .await
    .unwrap();
    for i in 0..250 {
        sqlx::query("INSERT INTO vs_mps_note (title) VALUES (?)")
            .bind(format!("n{i}"))
            .execute(&sq)
            .await
            .unwrap();
    }
    Pool::Sqlite(sq)
}

async fn count_returned(app: axum::Router, uri: &str) -> usize {
    let res = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "{uri} should succeed");
    let bytes = axum::body::to_bytes(res.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    // Page-number pagination wraps rows in `results`.
    json["results"]
        .as_array()
        .map(Vec::len)
        .or_else(|| json.as_array().map(Vec::len))
        .unwrap_or_else(|| panic!("no row array in response: {json}"))
}

/// The regression: a client asking for 1000 no longer gets 1000.
#[tokio::test]
async fn client_cannot_exceed_the_default_ceiling() {
    let app = ViewSet::for_model(Note::SCHEMA)
        .page_size(20)
        .router_pool("/notes", pool_with_rows().await);
    let n = count_returned(app, "/notes?page_size=1000").await;
    assert_eq!(
        n, 100,
        "?page_size=1000 must clamp to the 100 default, not return 1000 rows"
    );
}

/// The app can lower the ceiling below the default.
#[tokio::test]
async fn app_can_lower_the_ceiling() {
    let app = ViewSet::for_model(Note::SCHEMA)
        .page_size(20)
        .max_page_size(25)
        .router_pool("/notes", pool_with_rows().await);
    let n = count_returned(app, "/notes?page_size=1000").await;
    assert_eq!(n, 25, "the app's ceiling must win");
}

/// …and raise it deliberately when a consumer needs bigger pages.
#[tokio::test]
async fn app_can_raise_the_ceiling() {
    let app = ViewSet::for_model(Note::SCHEMA)
        .max_page_size(200)
        .router_pool("/notes", pool_with_rows().await);
    let n = count_returned(app, "/notes?page_size=200").await;
    assert_eq!(n, 200, "a raised ceiling must be honoured");
}

/// `?limit=` must respect the same ceiling — otherwise limit/offset is an
/// unbounded way around it.
#[tokio::test]
async fn limit_offset_respects_the_same_ceiling() {
    let app = ViewSet::for_model(Note::SCHEMA)
        .max_page_size(30)
        .pagination(rustango::viewset::PaginationStyle::LimitOffset)
        .router_pool("/notes", pool_with_rows().await);
    let n = count_returned(app, "/notes?limit=1000").await;
    assert_eq!(n, 30, "?limit= must clamp to max_page_size too");
}

/// A default larger than the ceiling can't smuggle a bigger page through the
/// no-parameter path.
#[tokio::test]
async fn default_page_size_is_clamped_to_the_ceiling() {
    let app = ViewSet::for_model(Note::SCHEMA)
        .page_size(500)
        .max_page_size(50)
        .router_pool("/notes", pool_with_rows().await);
    let n = count_returned(app, "/notes").await;
    assert_eq!(n, 50, "the default must be clamped by the ceiling as well");
}

async fn status_of(app: axum::Router, uri: &str) -> StatusCode {
    app.oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

/// `?page=i64::MAX` is an empty page, not a wrapped negative OFFSET (#1865).
#[tokio::test]
async fn huge_page_number_is_an_empty_page() {
    let app = ViewSet::for_model(Note::SCHEMA)
        .page_size(20)
        .router_pool("/notes", pool_with_rows().await);
    let n = count_returned(app, &format!("/notes?page={}", i64::MAX)).await;
    assert_eq!(n, 0);
}

/// An `__in` list over the cap is a 400; one at the cap still runs (#1865).
#[tokio::test]
async fn in_list_over_the_cap_is_a_400() {
    let cap = rustango::list_params::MAX_IN_VALUES;
    let ids = |n: usize| (1..=n).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    let app = ViewSet::for_model(Note::SCHEMA)
        .filter_fields(&["id"])
        .router_pool("/notes", pool_with_rows().await);
    let at_cap = status_of(app.clone(), &format!("/notes?id__in={}", ids(cap))).await;
    assert_eq!(at_cap, StatusCode::OK);
    let over = status_of(app, &format!("/notes?id__in={}", ids(cap + 1))).await;
    assert_eq!(over, StatusCode::BAD_REQUEST);
}

/// Seventeen filterable columns, so one request can carry 34 `__in` lists.
#[derive(Model, Debug, Clone)]
#[rustango(table = "vs_mps_wide")]
#[rustango(app = "vs_mps_app")]
#[allow(dead_code)]
pub struct Wide {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub f0: i64,
    pub f1: i64,
    pub f2: i64,
    pub f3: i64,
    pub f4: i64,
    pub f5: i64,
    pub f6: i64,
    pub f7: i64,
    pub f8: i64,
    pub f9: i64,
    pub f10: i64,
    pub f11: i64,
    pub f12: i64,
    pub f13: i64,
    pub f14: i64,
    pub f15: i64,
    pub f16: i64,
}

/// The `__in` lists of one request share SQLite's 32766-bind limit: a 400, not a 500.
#[cfg(feature = "admin")]
#[tokio::test]
async fn in_lists_over_the_bind_total_are_a_400() {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
    let pool = Pool::Sqlite(sq);
    rustango::testkit::create_tables_for::<Wide>(&pool)
        .await
        .unwrap();
    let names: Vec<String> = (0..17).map(|i| format!("f{i}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let app = ViewSet::for_model(Wide::SCHEMA)
        .filter_fields(&refs)
        .router_pool("/wide", pool);
    let ids = (1..=rustango::list_params::MAX_IN_VALUES)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let body = |lists: usize| {
        (0..lists)
            .map(|i| {
                let op = if i % 2 == 0 { "in" } else { "not_in" };
                format!("f{}__{op}={ids}", i / 2)
            })
            .collect::<Vec<_>>()
            .join("&")
    };
    let query = |lists: usize| {
        Request::builder()
            .method(Method::from_bytes(b"QUERY").unwrap())
            .uri("/wide")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(body(lists)))
            .unwrap()
    };
    let ok = app.clone().oneshot(query(31)).await.unwrap().status();
    assert_eq!(ok, StatusCode::OK);
    let over = app.oneshot(query(33)).await.unwrap().status();
    assert_eq!(over, StatusCode::BAD_REQUEST);
}
