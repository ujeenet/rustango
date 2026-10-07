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

#[derive(Model, Debug, Clone)]
#[rustango(table = "vs_natural_pk_token")]
#[rustango(app = "vs_natural_pk_tri")]
pub struct Token {
    #[rustango(primary_key)]
    pub id: uuid::Uuid,
    #[rustango(max_length = 200)]
    pub name: String,
}

/// A server-side v7 PK and a database-computed column (#1725).
#[derive(Model, Debug, Clone)]
#[rustango(table = "vs_natural_pk_doc")]
#[rustango(app = "vs_natural_pk_tri")]
pub struct Doc {
    #[rustango(primary_key, default_uuid_v7)]
    pub id: rustango::sql::Auto<uuid::Uuid>,
    pub qty: i64,
    #[rustango(generated_as = "qty * 2")]
    pub twice: i64,
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Tag>(pool).await;
    rustango::testkit::matrix::fresh_table::<Token>(pool).await;
    rustango::testkit::matrix::fresh_table::<Doc>(pool).await;
}

fn app(pool: &Pool) -> axum::Router {
    use rustango::viewset::ViewSet;
    ViewSet::for_model(Tag::SCHEMA)
        .router_pool("/tags", pool.clone())
        .merge(ViewSet::for_model(Token::SCHEMA).router_pool("/tokens", pool.clone()))
        .merge(ViewSet::for_model(Doc::SCHEMA).router_pool("/docs", pool.clone()))
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"))
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

/// `go` sorts before `rust`: a read-back that matches every row (MySQL's
/// `WHERE slug = 0` from `LAST_INSERT_ID()`) returns `go` for `rust`.
async fn create_keeps_the_client_pk_and_retrieves_by_it(pool: &Pool) {
    for (slug, name) in [("go", "Go"), ("rust", "Rust")] {
        let (status, body) = send(
            pool,
            Method::POST,
            "/tags",
            &format!(r#"{{"slug":"{slug}","name":"{name}"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "create {slug}: {body}");
        let row = json(&body);
        assert_eq!(row["slug"], slug, "create {slug} body: {body}");
        assert_eq!(row["name"], name, "create {slug} body: {body}");
    }

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
    let slugs: Vec<String> = json(&body)
        .as_array()
        .unwrap_or_else(|| panic!("bulk body is not an array: {body}"))
        .iter()
        .map(|r| r["slug"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(slugs, ["a", "b"], "bulk body: {body}");
    let (status, _) = send(pool, Method::GET, "/tags/b", "").await;
    assert_eq!(status, StatusCode::OK);
}

/// A client-supplied Uuid PK is read back by that value.
async fn create_with_a_uuid_pk_round_trips(pool: &Pool) {
    let id = "6f1c2a4e-9b7d-4c3a-8e21-0d5f4b6a7c89";
    let (status, body) = send(
        pool,
        Method::POST,
        "/tokens",
        &format!(r#"{{"id":"{id}","name":"T"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create: {body}");
    assert_eq!(json(&body)["id"], id, "create body: {body}");

    let (status, body) = send(pool, Method::GET, &format!("/tokens/{id}"), "").await;
    assert_eq!(status, StatusCode::OK, "retrieve: {body}");
    assert_eq!(json(&body)["name"], "T", "retrieve body: {body}");
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

/// #2075 — a duplicate key is a `409 Conflict`, single or bulk.
async fn a_duplicate_key_is_409(pool: &Pool) {
    let row = r#"{"slug":"rust","name":"Rust"}"#;
    let (status, _) = send(pool, Method::POST, "/tags", row).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = send(pool, Method::POST, "/tags", row).await;
    assert_eq!(status, StatusCode::CONFLICT, "duplicate: {body}");
    assert_eq!(json(&body)["error"], "conflict", "{body}");

    let bulk = r#"[{"slug":"go","name":"Go"},{"slug":"go","name":"Go"}]"#;
    let (status, body) = send(pool, Method::POST, "/tags", bulk).await;
    assert_eq!(status, StatusCode::CONFLICT, "bulk duplicate: {body}");
    assert_eq!(Tag::objects().count(pool).await.expect("count"), 1);
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

/// Create fills a `default_uuid_v7` PK and leaves a generated column to the database.
async fn create_fills_a_v7_pk_and_skips_generated_columns(pool: &Pool) {
    let (status, body) = send(pool, Method::POST, "/docs", r#"{"qty":3}"#).await;
    assert_eq!(status, StatusCode::CREATED, "create: {body}");
    let row = json(&body);
    let id = row["id"]
        .as_str()
        .unwrap_or_else(|| panic!("no id: {body}"));
    assert_eq!(
        uuid::Uuid::parse_str(id).expect("uuid").get_version_num(),
        7
    );
    assert_eq!(row["twice"], 6, "create body: {body}");
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        create_keeps_the_client_pk_and_retrieves_by_it,
        bulk_create_keeps_the_client_pks,
        create_without_the_pk_is_400_and_writes_nothing,
        update_cannot_change_the_pk,
        a_duplicate_key_is_409,
        create_with_a_uuid_pk_round_trips,
        create_fills_a_v7_pk_and_skips_generated_columns,
    ],
}
