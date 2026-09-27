//! `ViewSet` writes on a model whose PK the client supplies (#1671).

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    any(feature = "admin", feature = "tenancy")
))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::core::Model as _;
use rustango::sql::{CounterPool as _, FetcherPool as _, Pool};
use rustango::{tri_dialect_test, Model};
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(table = "vs_natural_pk_tag")]
#[rustango(app = "vs_natural_pk_tri")]
pub struct Tag {
    #[rustango(primary_key, max_length = 64)]
    pub slug: String,
    #[rustango(max_length = 200)]
    pub name: String,
}

fn app(pool: &Pool) -> axum::Router {
    rustango::viewset::ViewSet::for_model(Tag::SCHEMA).router_pool("/tags", pool.clone())
}

async fn send(pool: &Pool, method: Method, uri: &str, body: &str) -> (StatusCode, String) {
    let resp = app(pool)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn create_keeps_the_client_pk_and_retrieves_by_it(pool: &Pool) {
    let (status, body) = send(
        pool,
        Method::POST,
        "/tags",
        r#"{"slug":"rust","name":"Rust"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create: {body}");
    assert!(body.contains(r#""slug":"rust""#), "create body: {body}");

    let (status, body) = send(pool, Method::GET, "/tags/rust", "").await;
    assert_eq!(status, StatusCode::OK, "retrieve: {body}");
    assert!(body.contains(r#""name":"Rust""#), "retrieve body: {body}");
}

async fn bulk_create_keeps_the_client_pks(pool: &Pool) {
    let (status, body) = send(
        pool,
        Method::POST,
        "/tags",
        r#"[{"slug":"a","name":"A"},{"slug":"b","name":"B"}]"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "bulk create: {body}");
    let (status, _) = send(pool, Method::GET, "/tags/b", "").await;
    assert_eq!(status, StatusCode::OK);
}

async fn create_without_the_pk_is_400_and_writes_nothing(pool: &Pool) {
    let (status, body) = send(pool, Method::POST, "/tags", r#"{"name":"Rust"}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "missing pk: {body}");
    assert_eq!(Tag::objects().count(pool).await.expect("count"), 0);
}

async fn update_cannot_change_the_pk(pool: &Pool) {
    let (status, _) = send(
        pool,
        Method::POST,
        "/tags",
        r#"{"slug":"rust","name":"Rust"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = send(
        pool,
        Method::PUT,
        "/tags/rust",
        r#"{"slug":"moved","name":"Renamed"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "update: {body}");

    let rows: Vec<Tag> = Tag::objects().fetch(pool).await.expect("fetch");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].slug, "rust", "the PK must not move");
    assert_eq!(rows[0].name, "Renamed");
}

/// A SQLite text PK column without `NOT NULL` accepts NULL, so the missing
/// PK has to be rejected before the INSERT.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_nullable_pk_column_still_rejects_a_missing_pk() {
    let pool = rustango::testkit::matrix::Backend::Sqlite
        .pool()
        .await
        .expect("sqlite");
    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };
    rustango::sql::sqlx::query(
        "CREATE TABLE vs_natural_pk_tag (slug TEXT PRIMARY KEY, name TEXT NOT NULL)",
    )
    .execute(sq)
    .await
    .unwrap();

    let (status, body) = send(&pool, Method::POST, "/tags", r#"{"name":"Rust"}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "missing pk: {body}");
    assert_eq!(Tag::objects().count(&pool).await.expect("count"), 0);
}

tri_dialect_test! {
    model: Tag,
    scenarios: [
        create_keeps_the_client_pk_and_retrieves_by_it,
        bulk_create_keeps_the_client_pks,
        create_without_the_pk_is_400_and_writes_nothing,
        update_cannot_change_the_pk,
    ],
}
