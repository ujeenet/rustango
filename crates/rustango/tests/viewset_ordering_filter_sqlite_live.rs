//! End-to-end live test for `ViewSet::ordering` + new
//! `ordering_fields(...)` whitelist on SQLite (Django-parity #439 —
//! DRF `OrderingFilter`).
//!
//! The DSL (`ViewSet::ordering` for the default sort) + the `?ordering=`
//! query-param parse have shipped since v0.30. This PR closed the
//! remaining DRF gap: the `ordering_fields` whitelist that limits which
//! columns clients can sort by. Live tests cover both the new
//! whitelist enforcement and the previously-untested asc/desc query
//! parsing on a non-PG dialect.
//!
//! Covers:
//! - default ordering (from `.ordering(...)`) applies when no
//!   `?ordering=` is supplied
//! - explicit `?ordering=field` overrides the default (ASC)
//! - `?ordering=-field` flips to DESC via the `-` prefix
//! - comma-separated multi-field ordering chains correctly
//! - `ordering_fields` whitelist silently drops off-list field names
//!   (mirrors DRF's defensive default for unknown columns)

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "serializer"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::core::Model as _;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;
use serde_json::Value;
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone, serde::Serialize, serde::Deserialize)]
#[rustango(table = "vs_order_post")]
#[rustango(app = "vs_order_app")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub rating: i32,
    /// Imagine this is a sensitive column we don't want clients
    /// sorting on — used to exercise the whitelist drop.
    #[rustango(max_length = 200)]
    pub secret_score: String,
}

async fn fresh_pool() -> Pool {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE vs_order_post (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            title TEXT NOT NULL, \
            rating INTEGER NOT NULL, \
            secret_score TEXT NOT NULL)",
    )
    .execute(&sq)
    .await
    .expect("create");
    Pool::Sqlite(sq)
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("body bytes");
    serde_json::from_slice(&bytes).expect("json")
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

async fn post_row(app: &axum::Router, title: &str, rating: i32) {
    let payload =
        serde_json::json!({ "title": title, "rating": rating, "secret_score": "x" }).to_string();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/posts")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(payload))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(resp.status().is_success(), "POST {title:?} failed");
}

fn ids(body: &Value) -> Vec<i64> {
    body["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|r| r["id"].as_i64().expect("id i64"))
        .collect()
}

#[tokio::test]
async fn default_ordering_applies_when_no_query_param() {
    let pool = fresh_pool().await;
    let app = rustango::viewset::ViewSet::for_model(Post::SCHEMA)
        .page_size(50)
        .ordering(&[("rating", true)])
        .router_pool("/posts", pool);

    post_row(&app, "a", 3).await; // id 1
    post_row(&app, "b", 1).await; // id 2
    post_row(&app, "c", 2).await; // id 3

    // Default ordering = rating DESC → 1(=3), 3(=2), 2(=1).
    let resp = app.clone().oneshot(get("/posts")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(ids(&body), vec![1, 3, 2]);
}

#[tokio::test]
async fn explicit_ordering_overrides_default_and_handles_desc_prefix() {
    let pool = fresh_pool().await;
    let app = rustango::viewset::ViewSet::for_model(Post::SCHEMA)
        .page_size(50)
        .ordering(&[("rating", true)]) // default DESC on rating
        .router_pool("/posts", pool);

    post_row(&app, "a", 3).await; // id 1
    post_row(&app, "b", 1).await; // id 2
    post_row(&app, "c", 2).await; // id 3

    // `?ordering=rating` (ASC) flips the default.
    let body = body_json(
        app.clone()
            .oneshot(get("/posts?ordering=rating"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(ids(&body), vec![2, 3, 1]);

    // `?ordering=-rating` mirrors the default.
    let body = body_json(
        app.clone()
            .oneshot(get("/posts?ordering=-rating"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(ids(&body), vec![1, 3, 2]);
}

#[tokio::test]
async fn comma_separated_multi_field_ordering() {
    let pool = fresh_pool().await;
    let app = rustango::viewset::ViewSet::for_model(Post::SCHEMA)
        .page_size(50)
        .router_pool("/posts", pool);

    // Same rating, different titles
    post_row(&app, "charlie", 1).await; // id 1
    post_row(&app, "alpha", 1).await; // id 2
    post_row(&app, "bravo", 2).await; // id 3

    // ORDER BY rating ASC, title ASC → rating=1 first (alpha, charlie),
    // then rating=2 (bravo).
    let body = body_json(
        app.clone()
            .oneshot(get("/posts?ordering=rating,title"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(ids(&body), vec![2, 1, 3]);
}

#[tokio::test]
async fn ordering_fields_whitelist_drops_off_list_names() {
    let pool = fresh_pool().await;
    let app = rustango::viewset::ViewSet::for_model(Post::SCHEMA)
        .page_size(50)
        .ordering(&[("id", false)])
        .ordering_fields(&["title", "rating"]) // `secret_score` NOT whitelisted
        .router_pool("/posts", pool);

    post_row(&app, "a", 3).await; // id 1
    post_row(&app, "b", 1).await; // id 2
    post_row(&app, "c", 2).await; // id 3

    // Attempt to sort by `secret_score` — silently dropped (the
    // resulting ORDER BY list is empty, so default ordering kicks in
    // would NOT happen because `?ordering=` was supplied; the rows
    // come back in implementation order, which sqlite returns by
    // insert order for our case). We assert that the row count + ids
    // are still correct (the request didn't 4xx) and that ANY
    // explicit order on a whitelisted field still works:
    let body = body_json(
        app.clone()
            .oneshot(get("/posts?ordering=secret_score"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(body["results"].as_array().expect("results").len(), 3);

    // Whitelisted column still works.
    let body = body_json(
        app.clone()
            .oneshot(get("/posts?ordering=-rating"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(ids(&body), vec![1, 3, 2]);

    // Mixed valid + invalid — valid one is honored, invalid silently dropped.
    let body = body_json(
        app.clone()
            .oneshot(get("/posts?ordering=secret_score,rating"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(ids(&body), vec![2, 3, 1]); // rating ASC
}

// ===================================================================
// #1282 — with no explicit `ordering_fields`, the allowlist defaults to
// the fields the ViewSet EXPOSES, not every column on the model.
//
// The old default skipped the allowlist check entirely when
// `ordering_fields` was empty, so a ViewSet restricted to
// `fields = "id, title, rating"` still honoured
// `?ordering=secret_score` — a sort oracle over a column the API never
// returns. DRF defaults to the serializer's readable fields.
// ===================================================================

/// Rows are seeded so that sorting by `secret_score` gives a *different*
/// order from the default — if the unexposed sort were honoured, the ids
/// would come back in secret order, which is exactly the leak.
async fn seed_divergent(app: &axum::Router) {
    for (title, rating, secret) in [("a", 1, "zzz"), ("b", 2, "mmm"), ("c", 3, "aaa")] {
        let payload = serde_json::json!({
            "title": title, "rating": rating, "secret_score": secret
        })
        .to_string();
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/posts")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(resp.status().is_success());
    }
}

#[tokio::test]
async fn ordering_on_an_unexposed_field_is_dropped_by_default() {
    let pool = fresh_pool().await;
    let app = rustango::viewset::ViewSet::for_model(Post::SCHEMA)
        .page_size(50)
        .fields(&["id", "title", "rating"]) // secret_score NOT exposed
        .ordering(&[("id", false)])
        .router_pool("/posts", pool);

    seed_divergent(&app).await;

    // secret_score ASC would be c(aaa), b(mmm), a(zzz) => [3, 2, 1].
    // It must be ignored, leaving the default id ASC => [1, 2, 3].
    let resp = app
        .clone()
        .oneshot(get("/posts?ordering=secret_score"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        ids(&body),
        vec![1, 2, 3],
        "sorting by an unexposed column must be dropped, not honoured — \
         got the secret order, which leaks its values"
    );
}

/// The restriction must not break ordering on fields that ARE exposed.
#[tokio::test]
async fn ordering_on_an_exposed_field_still_works() {
    let pool = fresh_pool().await;
    let app = rustango::viewset::ViewSet::for_model(Post::SCHEMA)
        .page_size(50)
        .fields(&["id", "title", "rating"])
        .router_pool("/posts", pool);

    seed_divergent(&app).await;

    let resp = app
        .clone()
        .oneshot(get("/posts?ordering=-rating"))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(ids(&body), vec![3, 2, 1], "exposed field must still sort");
}

/// When `fields` is unset the ViewSet exposes everything, so ordering
/// stays permissive — the fix must not tighten that case.
#[tokio::test]
async fn ordering_stays_open_when_no_fields_restriction_is_set() {
    let pool = fresh_pool().await;
    let app = rustango::viewset::ViewSet::for_model(Post::SCHEMA)
        .page_size(50)
        .router_pool("/posts", pool);

    seed_divergent(&app).await;

    let resp = app
        .clone()
        .oneshot(get("/posts?ordering=secret_score"))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(
        ids(&body),
        vec![3, 2, 1],
        "with no `fields` restriction every column is exposed anyway"
    );
}
