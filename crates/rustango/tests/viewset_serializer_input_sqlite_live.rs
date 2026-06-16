//! Live integration test for the serializer **input** path on a
//! ViewSet (tri-dialect, here on SQLite):
//!   * the serializer's `validate()` runs on create/update → 400 with
//!     DRF-shape `{field: [msgs]}` field errors,
//!   * `read_only` fields a client posts are ignored, not written.
//!
//! Pairs with `viewset_serializer_render_sqlite_live.rs` (the output
//! half) to cover the full DRF marriage of ViewSets + serializers.

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "serializer"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::core::Model as _;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::{Model, Serializer};
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone, serde::Serialize, serde::Deserialize)]
#[rustango(table = "vs_in_item")]
#[rustango(app = "vs_in_app")]
pub struct Item {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 120)]
    pub name: String,
    #[rustango(max_length = 120)]
    pub slug: String,
    /// Server-controlled — clients must not be able to set it.
    pub internal_score: i64,
}

#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Item)]
struct ItemSerializer {
    #[serializer(validate = "name_min_len")]
    pub name: String,
    pub slug: String,
    /// In output, ignored on write.
    #[serializer(read_only)]
    pub internal_score: i64,
}

impl ItemSerializer {
    fn name_min_len(n: &String) -> Result<(), String> {
        if n.chars().count() < 3 {
            Err("name must be at least 3 characters".to_owned())
        } else {
            Ok(())
        }
    }
}

async fn router() -> axum::Router {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE vs_in_item (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            name TEXT NOT NULL, \
            slug TEXT NOT NULL, \
            internal_score INTEGER NOT NULL DEFAULT 0)",
    )
    .execute(&sq)
    .await
    .expect("create");
    rustango::viewset::ViewSet::for_model(Item::SCHEMA)
        .serializer::<ItemSerializer>()
        .router_pool("/items", Pool::Sqlite(sq))
}

fn post(uri: &str, json: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json.to_owned()))
        .unwrap()
}

async fn json_body(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("body bytes");
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

#[tokio::test]
async fn create_runs_serializer_validate_and_400s_on_failure() {
    let app = router().await;

    // name "ab" is 2 chars → fails the field validator.
    let resp = app
        .clone()
        .oneshot(post("/items", r#"{"name":"ab","slug":"thing"}"#))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "short name should 400"
    );
    let v = json_body(resp).await;
    let name_errs = v["name"].as_array().expect("DRF field-error shape: {v}");
    assert!(
        name_errs
            .iter()
            .any(|m| m.as_str() == Some("name must be at least 3 characters")),
        "validation message should surface under the `name` key: {v}"
    );

    // name "abc" passes.
    let resp = app
        .clone()
        .oneshot(post("/items", r#"{"name":"abc","slug":"thing"}"#))
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "valid name should succeed, got {}",
        resp.status()
    );
    let v = json_body(resp).await;
    assert_eq!(v["name"], "abc");
}

#[tokio::test]
async fn read_only_field_is_ignored_on_create() {
    let app = router().await;

    // Client tries to set the server-controlled `internal_score`.
    let resp = app
        .clone()
        .oneshot(post(
            "/items",
            r#"{"name":"widget","slug":"w","internal_score":999}"#,
        ))
        .await
        .unwrap();
    assert!(resp.status().is_success(), "create should succeed");

    let v = json_body(resp).await;
    assert_eq!(v["name"], "widget");
    // read_only → not written → DB default 0, NOT the posted 999.
    assert_eq!(
        v["internal_score"], 0,
        "read_only field must be ignored on write (DB default, not 999): {v}"
    );
}

#[tokio::test]
async fn partial_update_ignores_read_only_field() {
    let app = router().await;

    // Seed a row.
    let resp = app
        .clone()
        .oneshot(post("/items", r#"{"name":"orig","slug":"o"}"#))
        .await
        .unwrap();
    assert!(resp.status().is_success());

    // PATCH attempts to bump internal_score (read_only) + rename.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PATCH)
                .uri("/items/1")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"renamed","internal_score":777}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "patch should succeed, got {}",
        resp.status()
    );

    let v = json_body(resp).await;
    assert_eq!(v["name"], "renamed", "writable field updated: {v}");
    assert_eq!(
        v["internal_score"], 0,
        "read_only field must stay at its server value: {v}"
    );
}
