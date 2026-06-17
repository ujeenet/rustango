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

// ---------------------------------------------------------------------------
// Declarative field validators: max_length / min / max / choices, inherited
// from the model's FieldSchema unless a per-field attr overrides.
// ---------------------------------------------------------------------------

#[derive(Model, Debug, Clone, serde::Serialize, serde::Deserialize)]
#[rustango(table = "vs_in_widget")]
#[rustango(app = "vs_in_app")]
pub struct Widget {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 8)]
    pub code: String,
    #[rustango(max_length = 50)]
    pub note: String,
    #[rustango(min = 1, max = 3)]
    pub priority: i64,
    #[rustango(max_length = 20, choices = "draft:Draft, live:Live")]
    pub status: String,
}

#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Widget)]
struct WidgetSerializer {
    pub code: String, // inherits max_length = 8 from the model
    #[serializer(max_length = 4)] // overrides the model's 50
    pub note: String,
    pub priority: i64,  // inherits min = 1, max = 3
    pub status: String, // inherits choices
}

async fn widget_router() -> axum::Router {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE vs_in_widget (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            code TEXT NOT NULL, note TEXT NOT NULL, \
            priority INTEGER NOT NULL, status TEXT NOT NULL)",
    )
    .execute(&sq)
    .await
    .expect("create");
    rustango::viewset::ViewSet::for_model(Widget::SCHEMA)
        .serializer::<WidgetSerializer>()
        .router_pool("/widgets", Pool::Sqlite(sq))
}

/// Helper: POST a widget body, return (status, body).
async fn post_widget(app: &axum::Router, body: &str) -> (StatusCode, serde_json::Value) {
    let resp = app.clone().oneshot(post("/widgets", body)).await.unwrap();
    let status = resp.status();
    (status, json_body(resp).await)
}

#[tokio::test]
async fn max_length_inherited_from_model() {
    let app = widget_router().await;
    // code = 9 chars > the model's max_length = 8.
    let (status, v) = post_widget(
        &app,
        r#"{"code":"abcdefghi","note":"ok","priority":1,"status":"draft"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "inherited max_length: {v}");
    assert!(
        v["code"][0]
            .as_str()
            .unwrap_or("")
            .contains("at most 8 characters"),
        "model max_length inherited: {v}"
    );
}

#[tokio::test]
async fn max_length_attr_overrides_model() {
    let app = widget_router().await;
    // note = 5 chars > the serializer attr max_length = 4 (model allows 50).
    let (status, v) = post_widget(
        &app,
        r#"{"code":"ok","note":"toolong","priority":1,"status":"draft"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "override max_length: {v}");
    assert!(
        v["note"][0]
            .as_str()
            .unwrap_or("")
            .contains("at most 4 characters"),
        "serializer attr overrides model max_length: {v}"
    );
}

#[tokio::test]
async fn min_max_inherited_from_model() {
    let app = widget_router().await;
    // priority = 9 > model max = 3.
    let (status, v) = post_widget(
        &app,
        r#"{"code":"ok","note":"ok","priority":9,"status":"draft"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        v["priority"][0].as_str().unwrap_or("").contains("≤ 3"),
        "max inherited: {v}"
    );
    // priority = 0 < model min = 1.
    let (status, v) = post_widget(
        &app,
        r#"{"code":"ok","note":"ok","priority":0,"status":"draft"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        v["priority"][0].as_str().unwrap_or("").contains("≥ 1"),
        "min inherited: {v}"
    );
}

#[tokio::test]
async fn choices_inherited_from_model() {
    let app = widget_router().await;
    let (status, v) = post_widget(
        &app,
        r#"{"code":"ok","note":"ok","priority":1,"status":"bogus"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        v["status"][0]
            .as_str()
            .unwrap_or("")
            .contains("valid choice"),
        "model choices inherited: {v}"
    );
}

#[tokio::test]
async fn valid_widget_passes_all_constraints() {
    let app = widget_router().await;
    let (status, v) = post_widget(
        &app,
        r#"{"code":"ok","note":"abc","priority":2,"status":"live"}"#,
    )
    .await;
    assert!(
        status.is_success(),
        "all constraints satisfied → success: {status} {v}"
    );
    assert_eq!(v["code"], "ok");
    assert_eq!(v["priority"], 2);
}

// ── source-renamed writable field: input uses the serializer field name ──
//
// Regression: a `#[serializer(source = "body")]` writable field renamed on
// OUTPUT (`body` → `content`) must also accept the serializer field name
// (`content`) on INPUT and persist it to the model column (`body`). Before the
// fix the write path read the raw body by model column, so posting `content`
// 400'd with "required field `body` missing".

#[derive(Model, Debug, Clone, serde::Serialize, serde::Deserialize)]
#[rustango(table = "vs_in_doc")]
#[rustango(app = "vs_in_app")]
pub struct Doc {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub body: String,
}

#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Doc)]
struct DocSerializer {
    pub id: Auto<i64>,
    pub title: String,
    /// JSON key `content`, model column `body`.
    #[serializer(source = "body")]
    pub content: String,
}

async fn doc_router() -> axum::Router {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE vs_in_doc (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            title TEXT NOT NULL, \
            body TEXT NOT NULL)",
    )
    .execute(&sq)
    .await
    .expect("create");
    rustango::viewset::ViewSet::for_model(Doc::SCHEMA)
        .serializer::<DocSerializer>()
        .router_pool("/docs", Pool::Sqlite(sq))
}

#[tokio::test]
async fn source_renamed_field_accepts_serializer_name_on_create() {
    let app = doc_router().await;
    // Client posts the SERIALIZER field name `content` (DRF shape), not `body`.
    let resp = app
        .clone()
        .oneshot(post(
            "/docs",
            r#"{"title":"Hello","content":"the body text"}"#,
        ))
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "posting the serializer field name should succeed, got {}",
        resp.status()
    );
    let v = json_body(resp).await;
    // The value round-trips: written to `body`, rendered back as `content`.
    assert_eq!(
        v["content"], "the body text",
        "source-renamed field must persist on write + render on read: {v}"
    );
    assert_eq!(v["title"], "Hello");
}

#[tokio::test]
async fn source_renamed_field_updates_on_partial_update() {
    let app = doc_router().await;
    let created = app
        .clone()
        .oneshot(post("/docs", r#"{"title":"Orig","content":"orig body"}"#))
        .await
        .unwrap();
    assert!(created.status().is_success());

    let patched = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PATCH)
                .uri("/docs/1")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"content":"patched body"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        patched.status().is_success(),
        "PATCH with the serializer field name should succeed, got {}",
        patched.status()
    );
    let v = json_body(patched).await;
    assert_eq!(
        v["content"], "patched body",
        "PATCH on a source-renamed field must update the model column: {v}"
    );
    assert_eq!(v["title"], "Orig", "untouched field preserved");
}
