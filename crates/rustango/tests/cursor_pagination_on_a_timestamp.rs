//! Cursor pagination must work on a timestamp, and must refuse an
//! unusable column where it is configured (#1459).
//!
//! `cursor_pagination_desc("placed_at")` used to be accepted at build
//! time and then return **500 on every request**:
//!
//! ```text
//! {"error":"cursor pagination requires an integer field (i16/i32/i64)"}
//! ```
//!
//! Two defects in one. The restriction was undocumented — `docs/viewsets.md`
//! and the method's own doc comment said "a stable,
//! monotonically-ordered column (typically `id`)", which a `TIMESTAMPTZ`
//! is, and which is the canonical cursor in the DRF API this is shaped
//! after. And a configuration error surfaced as a server error, once per
//! request, forever: the ViewSet built, the process started, health
//! checks passed, and the endpoint was dead.
//!
//! Found by the commerce soak, which had pointed cursor pagination at an
//! order's `placed_at` for exactly the reason the docs describe.

#![cfg(all(feature = "sqlite", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use chrono::{DateTime, Utc};
use rustango::core::Model as _;
use rustango::sql::{Auto, Pool};
use rustango::viewset::ViewSet;
use rustango::Model;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(table = "cursor_ts_event", display = "label")]
#[allow(dead_code)]
pub struct Event {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 40)]
    pub label: String,
    /// The cursor column. A timestamp on an append-only table is the
    /// case cursor pagination exists to serve.
    pub occurred_at: DateTime<Utc>,
    /// Deliberately a float: nothing may accept it as a cursor, because
    /// floats do not round-trip exactly through a token.
    pub score: f64,
}

const DDL: &str = "CREATE TABLE cursor_ts_event (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    label       TEXT NOT NULL,
    occurred_at TEXT NOT NULL,
    score       REAL NOT NULL
)";

async fn seeded_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    rustango::sql::raw_execute_pool(&pool, DDL, Vec::new())
        .await
        .expect("create");
    for i in 0..7 {
        let sql = format!(
            "INSERT INTO cursor_ts_event (label, occurred_at, score) \
             VALUES ('e{i}', '2026-09-1{}T10:00:00+00:00', {}.5)",
            i + 1,
            i
        );
        rustango::sql::raw_execute_pool(&pool, &sql, Vec::new())
            .await
            .expect("insert");
    }
    pool
}

async fn get(pool: &Pool, uri: &str) -> (StatusCode, serde_json::Value) {
    let app = ViewSet::for_model(Event::SCHEMA)
        .cursor_pagination("occurred_at")
        .page_size(3)
        .router_pool("/events", pool.clone());
    let res = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("request");
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .expect("body");
    let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn a_timestamp_cursor_answers_instead_of_500ing() {
    let pool = seeded_pool().await;
    let (status, body) = get(&pool, "/events").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a timestamp cursor must serve a page; it 500'd on every request before \
         #1459. Body: {body}"
    );
    assert_eq!(
        body["results"].as_array().map(Vec::len),
        Some(3),
        "page_size(3) should bound the page: {body}"
    );
    assert!(
        body["next"].is_string(),
        "seven rows at page_size 3 must offer a next cursor: {body}"
    );
}

/// The cursor has to *advance*, not just exist. A token that decodes to
/// the wrong type would still be a string in the response while paging
/// forever over page one.
#[tokio::test]
async fn the_timestamp_cursor_actually_walks_the_table() {
    let pool = seeded_pool().await;

    let (_, first) = get(&pool, "/events").await;
    let labels_1: Vec<String> = first["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["label"].as_str().unwrap_or_default().to_owned())
        .collect();
    let token = first["next"].as_str().expect("next cursor").to_owned();

    let (status, second) = get(&pool, &format!("/events?cursor={token}")).await;
    assert_eq!(status, StatusCode::OK, "second page: {second}");
    let labels_2: Vec<String> = second["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["label"].as_str().unwrap_or_default().to_owned())
        .collect();

    assert_eq!(labels_1, vec!["e0", "e1", "e2"], "first page, ascending");
    assert_eq!(
        labels_2,
        vec!["e3", "e4", "e5"],
        "the cursor must advance past the first page, not repeat it"
    );
}

/// A bad cursor is the caller's fault: 400, not 500.
#[tokio::test]
async fn a_malformed_cursor_is_a_client_error() {
    let pool = seeded_pool().await;
    let (status, _) = get(&pool, "/events?cursor=not-a-real-token").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// The other half of #1459: an unusable column must fail where it is
/// **configured**, so the mistake is found at start-up rather than by
/// every request for the life of the deployment.
#[test]
#[should_panic(expected = "cannot be a cursor")]
fn a_float_column_is_refused_at_build_time() {
    let _ = ViewSet::for_model(Event::SCHEMA).cursor_pagination("score");
}

#[test]
#[should_panic(expected = "has no field")]
fn an_unknown_column_is_refused_at_build_time() {
    let _ = ViewSet::for_model(Event::SCHEMA).cursor_pagination("no_such_column");
}
