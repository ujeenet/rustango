//! Cookbook Chapter 9 — ViewSet (DRF-shape API for any model).
//!
//! Live in-process tests via `tower::ServiceExt::oneshot` against
//! a `ViewSet::for_model(...).router(...)` mounted on a real PG pool.
//! Exercises list / retrieve / create / update / destroy routes.
//!
//! Run: `DATABASE_URL=... cargo test --test cookbook_chapter09_viewsets -- --test-threads=1`

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use cookbook_blog::apps::blog::models::Author;
use http_body_util::BodyExt;
use rustango::core::Model as _;
use rustango::sql::sqlx;
use rustango::viewset::ViewSet;
use tower::ServiceExt;

fn url() -> Option<String> { std::env::var("DATABASE_URL").ok() }

async fn pool() -> Option<sqlx::PgPool> {
    Some(sqlx::PgPool::connect(&url()?).await.expect("connect"))
}

async fn fresh_author_table(pool: &sqlx::PgPool) {
    sqlx::query("DROP TABLE IF EXISTS cookbook_author CASCADE")
        .execute(pool).await.unwrap();
    sqlx::query(
        r#"CREATE TABLE cookbook_author (
            id BIGSERIAL PRIMARY KEY,
            name VARCHAR(80) NOT NULL,
            email VARCHAR(200) NOT NULL UNIQUE,
            bio VARCHAR(500) NULL,
            joined_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )"#,
    ).execute(pool).await.unwrap();
}

fn router(pool: sqlx::PgPool) -> axum::Router {
    ViewSet::for_model(Author::SCHEMA)
        .filter_fields(&["name", "email"])
        .search_fields(&["name", "email", "bio"])
        .ordering(&[("id", false)])
        .router("/authors", pool)
}

async fn json_request(
    router: axum::Router, method: Method, uri: &str, body: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(s) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(s.to_owned())
        }
        None => Body::empty(),
    };
    let resp = router.oneshot(req.body(body).unwrap()).await.expect("response");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

// §9.112 / 9.113 — list returns paginated payload + create writes a new row.
#[tokio::test]
async fn viewset_list_create_round_trip() {
    let Some(pool) = pool().await else { return };
    fresh_author_table(&pool).await;

    // Empty list initially.
    let (status, body) = json_request(router(pool.clone()), Method::GET, "/authors", None).await;
    assert_eq!(status, StatusCode::OK);
    let results = body.get("results").and_then(|r| r.as_array()).expect("results array");
    assert!(results.is_empty(), "fresh table → empty results");

    // Create a new author.
    let payload = r#"{"name": "ada", "email": "ada@example.com", "bio": "first"}"#;
    let (status, body) = json_request(router(pool.clone()), Method::POST, "/authors", Some(payload)).await;
    assert!(
        status == StatusCode::CREATED || status == StatusCode::OK,
        "create returned {status}; body: {body}"
    );
    let id = body.get("id").and_then(serde_json::Value::as_i64).expect("created id");
    assert!(id > 0);

    // List now has one entry.
    let (_, body) = json_request(router(pool.clone()), Method::GET, "/authors", None).await;
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["name"], "ada");
}

// §9.113 — retrieve by pk.
#[tokio::test]
async fn viewset_retrieve_returns_single_object_by_pk() {
    let Some(pool) = pool().await else { return };
    fresh_author_table(&pool).await;

    let payload = r#"{"name": "bob", "email": "bob@example.com"}"#;
    let (_, body) = json_request(router(pool.clone()), Method::POST, "/authors", Some(payload)).await;
    let id = body["id"].as_i64().unwrap();

    let (status, body) = json_request(
        router(pool.clone()), Method::GET, &format!("/authors/{id}"), None,
    ).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "bob");
    assert_eq!(body["email"], "bob@example.com");
}

// §9.113 — update + destroy lifecycle.
#[tokio::test]
async fn viewset_update_then_destroy() {
    let Some(pool) = pool().await else { return };
    fresh_author_table(&pool).await;

    let payload = r#"{"name": "carl", "email": "carl@example.com"}"#;
    let (_, body) = json_request(router(pool.clone()), Method::POST, "/authors", Some(payload)).await;
    let id = body["id"].as_i64().unwrap();

    // PATCH partial update.
    let patch = r#"{"name": "carl-renamed", "email": "carl@example.com"}"#;
    let (status, body) = json_request(
        router(pool.clone()), Method::PUT, &format!("/authors/{id}"), Some(patch),
    ).await;
    assert!(
        status == StatusCode::OK || status == StatusCode::ACCEPTED,
        "update returned {status}; body: {body}"
    );

    // Confirm via GET.
    let (_, body) = json_request(router(pool.clone()), Method::GET, &format!("/authors/{id}"), None).await;
    assert_eq!(body["name"], "carl-renamed");

    // DELETE.
    let (status, _) = json_request(router(pool.clone()), Method::DELETE, &format!("/authors/{id}"), None).await;
    assert!(
        status == StatusCode::NO_CONTENT || status == StatusCode::OK,
        "destroy returned {status}"
    );

    // GET now 404.
    let (status, _) = json_request(router(pool.clone()), Method::GET, &format!("/authors/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// §9.115 — filter_fields wires `?name=…` query param to QuerySet filter.
#[tokio::test]
async fn viewset_filter_query_param_narrows_list() {
    let Some(pool) = pool().await else { return };
    fresh_author_table(&pool).await;

    for body in [
        r#"{"name": "alice", "email": "alice@example.com"}"#,
        r#"{"name": "bob", "email": "bob@example.com"}"#,
        r#"{"name": "carol", "email": "carol@example.com"}"#,
    ] {
        json_request(router(pool.clone()), Method::POST, "/authors", Some(body)).await;
    }

    let (status, body) = json_request(
        router(pool.clone()), Method::GET, "/authors?name=bob", None,
    ).await;
    assert_eq!(status, StatusCode::OK);
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "filter ?name=bob → exactly one row");
    assert_eq!(results[0]["name"], "bob");
}
