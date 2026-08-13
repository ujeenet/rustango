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
