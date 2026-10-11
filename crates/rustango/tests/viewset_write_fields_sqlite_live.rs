//! #1845 — a ViewSet writes only the fields it exposes, and `OwnedBy` pins
//! the owner: a client cannot plant a row in, or move one into, another
//! account. `?ordering=` falls back to the fields the serializer renders.

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "serializer"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::core::Model as _;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::tenancy::Principal;
use rustango::viewset::{OwnedBy, ViewSet};
use rustango::{Model, Serializer};
use serde_json::Value;
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone, serde::Serialize, serde::Deserialize)]
#[rustango(table = "vs_wf_doc")]
#[rustango(app = "vs_wf_app")]
pub struct Doc {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub owner_id: i64,
    #[rustango(max_length = 200)]
    pub title: String,
    pub salary: i64,
}

/// Renders no `salary`, so it must not be sortable either.
#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Doc)]
struct DocSerializer {
    pub title: String,
}

async fn pool() -> sqlx::SqlitePool {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE vs_wf_doc (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            owner_id INTEGER NOT NULL DEFAULT 0, \
            title TEXT NOT NULL, \
            salary INTEGER NOT NULL DEFAULT 0)",
    )
    .execute(&sq)
    .await
    .expect("create");
    // Salary order is the reverse of id order.
    for (owner, title, salary) in [(1, "a", 30), (1, "b", 20), (2, "c", 10)] {
        sqlx::query("INSERT INTO vs_wf_doc (owner_id, title, salary) VALUES (?, ?, ?)")
            .bind(owner)
            .bind(title)
            .bind(salary)
            .execute(&sq)
            .await
            .expect("seed");
    }
    sq
}

async fn row(sq: &sqlx::SqlitePool, id: i64) -> Option<(i64, String, i64)> {
    sqlx::query_as("SELECT owner_id, title, salary FROM vs_wf_doc WHERE id = ?")
        .bind(id)
        .fetch_optional(sq)
        .await
        .expect("read")
}

async fn count(sq: &sqlx::SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM vs_wf_doc")
        .fetch_one(sq)
        .await
        .expect("count")
}

fn req(method: Method, uri: &str, body: &str, uid: Option<i64>) -> Request<Body> {
    let mut r = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    if let Some(uid) = uid {
        r.extensions_mut().insert(Principal::user(uid, false, None));
    }
    r
}

async fn send(app: &axum::Router, r: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(r).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn fields_limits_what_update_writes() {
    let sq = pool().await;
    let app = ViewSet::for_model(Doc::SCHEMA)
        .fields(&["id", "title"])
        .router_pool("/docs", Pool::Sqlite(sq.clone()));

    let (status, _) = send(
        &app,
        req(
            Method::PATCH,
            "/docs/1",
            r#"{"owner_id":999,"salary":5}"#,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "nothing writable was sent");
    assert_eq!(row(&sq, 1).await, Some((1, "a".into(), 30)));

    let body = r#"{"title":"x","owner_id":999,"salary":5}"#;
    let (status, _) = send(&app, req(Method::PUT, "/docs/1", body, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(row(&sq, 1).await, Some((1, "x".into(), 30)));
}

#[tokio::test]
async fn fields_limits_what_create_writes() {
    let sq = pool().await;
    let app = ViewSet::for_model(Doc::SCHEMA)
        .fields(&["id", "title"])
        .router_pool("/docs", Pool::Sqlite(sq.clone()));
    let body = r#"{"title":"n","owner_id":7,"salary":99}"#;
    let (status, created) = send(&app, req(Method::POST, "/docs", body, None)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_i64().expect("id");
    assert_eq!(row(&sq, id).await, Some((0, "n".into(), 0)));
}

fn owned(sq: &sqlx::SqlitePool) -> axum::Router {
    ViewSet::for_model(Doc::SCHEMA)
        .filter_backend(OwnedBy::column("owner_id"))
        .router_pool("/docs", Pool::Sqlite(sq.clone()))
}

#[tokio::test]
async fn owned_by_pins_the_owner_on_create() {
    let sq = pool().await;
    let app = owned(&sq);
    let body = r#"{"title":"planted","owner_id":2,"salary":1}"#;
    let (status, created) = send(&app, req(Method::POST, "/docs", body, Some(1))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["owner_id"], 1, "owner is the caller: {created}");

    // No owner in the body at all: still the caller's row.
    let (status, created) = send(
        &app,
        req(
            Method::POST,
            "/docs",
            r#"{"title":"t","salary":1}"#,
            Some(1),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["owner_id"], 1);

    let bulk = r#"[{"title":"b1","owner_id":2,"salary":1},{"title":"b2","owner_id":2,"salary":1}]"#;
    let (status, created) = send(&app, req(Method::POST, "/docs", bulk, Some(1))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created[0]["owner_id"], 1, "{created}");
    assert_eq!(created[1]["owner_id"], 1, "{created}");
}

#[tokio::test]
async fn owned_by_does_not_move_a_row_on_update() {
    let sq = pool().await;
    let app = owned(&sq);
    let (status, _) = send(
        &app,
        req(Method::PATCH, "/docs/1", r#"{"owner_id":2}"#, Some(1)),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "the owner is not writable");
    assert_eq!(row(&sq, 1).await, Some((1, "a".into(), 30)));

    let body = r#"{"owner_id":2,"title":"moved","salary":30}"#;
    let (status, updated) = send(&app, req(Method::PUT, "/docs/1", body, Some(1))).await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(row(&sq, 1).await, Some((1, "moved".into(), 30)));
}

#[tokio::test]
async fn owned_by_refuses_an_unauthenticated_create() {
    let sq = pool().await;
    let app = owned(&sq);
    let body = r#"{"title":"anon","owner_id":2,"salary":1}"#;
    let (status, _) = send(&app, req(Method::POST, "/docs", body, None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(count(&sq).await, 3, "nothing written");
}

#[tokio::test]
async fn owned_by_superuser_sees_all_may_set_the_owner() {
    let sq = pool().await;
    let app = ViewSet::for_model(Doc::SCHEMA)
        .filter_backend(OwnedBy::column("owner_id").superuser_sees_all())
        .router_pool("/docs", Pool::Sqlite(sq.clone()));
    let mut r = req(Method::PATCH, "/docs/1", r#"{"owner_id":2}"#, None);
    r.extensions_mut().insert(Principal::admin(9));
    let (status, _) = send(&app, r).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(row(&sq, 1).await, Some((2, "a".into(), 30)));
}

#[tokio::test]
async fn ordering_falls_back_to_the_serializer_fields() {
    let sq = pool().await;
    let app = ViewSet::for_model(Doc::SCHEMA)
        .serializer::<DocSerializer>()
        .ordering(&[("id", false)])
        .router_pool("/docs", Pool::Sqlite(sq));
    let titles = |v: &Value| -> Vec<String> {
        v["results"]
            .as_array()
            .expect("results")
            .iter()
            .map(|r| r["title"].as_str().unwrap().to_owned())
            .collect()
    };
    let (_, by_salary) = send(&app, req(Method::GET, "/docs?ordering=salary", "", None)).await;
    assert_eq!(
        titles(&by_salary),
        ["a", "b", "c"],
        "salary is not a sort key"
    );
    let (_, by_title) = send(&app, req(Method::GET, "/docs?ordering=-title", "", None)).await;
    assert_eq!(
        titles(&by_title),
        ["c", "b", "a"],
        "rendered fields still sort"
    );
}

#[tokio::test]
async fn owned_by_refuses_an_unauthenticated_update() {
    let sq = pool().await;
    let app = owned(&sq);
    let (status, _) = send(
        &app,
        req(Method::PATCH, "/docs/1", r#"{"title":"x"}"#, None),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(row(&sq, 1).await, Some((1, "a".into(), 30)));
}

/// A second backend that pins `owner_id` to a fixed value.
struct PinOwner(i64);

impl rustango::viewset::ViewSetFilter for PinOwner {
    fn filter(
        &self,
        _p: &std::collections::HashMap<String, String>,
        _s: &'static rustango::core::ModelSchema,
    ) -> Vec<rustango::core::WhereExpr> {
        Vec::new()
    }

    fn write_pins(
        &self,
        _parts: &axum::http::request::Parts,
        _s: &'static rustango::core::ModelSchema,
    ) -> Vec<rustango::viewset::WritePin> {
        vec![rustango::viewset::WritePin::field("owner_id", self.0)]
    }
}

#[tokio::test]
async fn two_pins_on_one_field_write_it_once_or_deny() {
    let sq = pool().await;
    let agree = ViewSet::for_model(Doc::SCHEMA)
        .filter_backend(OwnedBy::column("owner_id"))
        .filter_backend(PinOwner(1))
        .router_pool("/docs", Pool::Sqlite(sq.clone()));
    let body = r#"{"title":"t","salary":1}"#;
    let (status, created) = send(&agree, req(Method::POST, "/docs", body, Some(1))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["owner_id"], 1);

    let disagree = ViewSet::for_model(Doc::SCHEMA)
        .filter_backend(OwnedBy::column("owner_id"))
        .filter_backend(PinOwner(2))
        .router_pool("/docs", Pool::Sqlite(sq.clone()));
    let (status, _) = send(&disagree, req(Method::POST, "/docs", body, Some(1))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(count(&sq).await, 4, "only the agreeing create wrote");
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "vs_wf_item")]
#[rustango(app = "vs_wf_app")]
#[allow(dead_code)]
pub struct Item {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 20)]
    pub title: String,
    #[rustango(
        max_length = 10,
        choices = "draft:Draft, live:Live",
        default = "'draft'"
    )]
    pub status: String,
    #[rustango(max_length = 60, validators = "email")]
    pub contact: Option<String>,
    #[rustango(max_length = 10, default = "'n/a'")]
    pub note: Option<String>,
    #[rustango(default = "true")]
    pub is_public: bool,
    pub settings: serde_json::Value,
    #[rustango(auto_now_add)]
    pub created_at: Auto<chrono::DateTime<chrono::Utc>>,
    #[rustango(auto_now)]
    pub updated_at: Auto<chrono::DateTime<chrono::Utc>>,
}

async fn item_pool() -> sqlx::SqlitePool {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE vs_wf_item (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            title TEXT NOT NULL, \
            status TEXT NOT NULL DEFAULT 'draft', \
            contact TEXT, \
            note TEXT DEFAULT 'n/a', \
            is_public BOOLEAN NOT NULL DEFAULT true, \
            settings TEXT NOT NULL, \
            created_at TEXT NOT NULL, \
            updated_at TEXT NOT NULL)",
    )
    .execute(&sq)
    .await
    .expect("create");
    sq
}

fn items(sq: &sqlx::SqlitePool) -> axum::Router {
    ViewSet::for_model(Item::SCHEMA).router_pool("/items", Pool::Sqlite(sq.clone()))
}

/// Create one item through the API; returns its id.
async fn new_item(app: &axum::Router) -> i64 {
    let body = r#"{"title":"t","status":"live","note":"x","is_public":false,"settings":{"a":1}}"#;
    let (status, v) = send(app, req(Method::POST, "/items", body, None)).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    v["id"].as_i64().expect("id")
}

/// #2529: a value the model's rules reject is a 422 per field, as a serializer's is.
#[tokio::test]
async fn model_validation_failure_is_a_422_field_error() {
    let sq = item_pool().await;
    let app = items(&sq);
    for (body, field) in [
        (r#"{"title":"t","status":"bogus","settings":{}}"#, "status"),
        (
            r#"{"title":"t","contact":"not-an-email","settings":{}}"#,
            "contact",
        ),
        (
            r#"{"title":"twenty-one-characters","settings":{}}"#,
            "title",
        ),
    ] {
        let (status, v) = send(&app, req(Method::POST, "/items", body, None)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}: {v}");
        assert!(v["details"][field].is_array(), "{body}: {v}");
    }
    let bulk = r#"[{"title":"t","settings":{}},{"title":"t","status":"bogus","settings":{}}]"#;
    let (status, v) = send(&app, req(Method::POST, "/items", bulk, None)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v["details"]["status"].is_array(), "{v}");
    assert!(
        v["message"].as_str().unwrap_or("").contains("bulk entry 1"),
        "{v}"
    );
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vs_wf_item")
        .fetch_one(&sq)
        .await
        .expect("count");
    assert_eq!(n, 0, "no entry of a refused bulk is written");

    let id = new_item(&app).await;
    let uri = format!("/items/{id}");
    let (status, v) = send(
        &app,
        req(Method::PATCH, &uri, r#"{"status":"bogus"}"#, None),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v["details"]["status"].is_array(), "{v}");
}

async fn updated_at(sq: &sqlx::SqlitePool, id: i64) -> String {
    sqlx::query_scalar("SELECT updated_at FROM vs_wf_item WHERE id = ?")
        .bind(id)
        .fetch_one(sq)
        .await
        .expect("read")
}

/// #2527: PUT and PATCH restamp an `auto_now` column.
#[tokio::test]
async fn update_restamps_auto_now() {
    let sq = item_pool().await;
    let app = items(&sq);
    let id = new_item(&app).await;
    let uri = format!("/items/{id}");
    let full = r#"{"title":"put","status":"live","note":"x","is_public":true,"settings":{}}"#;
    for (method, body) in [
        (Method::PATCH, r#"{"title":"patched"}"#),
        (Method::PUT, full),
    ] {
        sqlx::query(
            "UPDATE vs_wf_item SET created_at = '2000-01-01T00:00:00Z', \
             updated_at = '2000-01-01T00:00:00Z' WHERE id = ?",
        )
        .bind(id)
        .execute(&sq)
        .await
        .expect("age the row");
        let (status, v) = send(&app, req(method.clone(), &uri, body, None)).await;
        assert_eq!(status, StatusCode::OK, "{method}: {v}");
        assert!(
            !updated_at(&sq, id).await.starts_with("2000-"),
            "{method} left updated_at stale"
        );
        let created: String = sqlx::query_scalar("SELECT created_at FROM vs_wf_item WHERE id = ?")
            .bind(id)
            .fetch_one(&sq)
            .await
            .expect("read");
        assert!(created.starts_with("2000-"), "{method} moved auto_now_add");
    }

    // An empty PATCH is still "no fields to update", not a bare restamp.
    let (status, v) = send(&app, req(Method::PATCH, &uri, "{}", None)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
}

/// A form-encoded create without a Bool is an unticked box, default or not.
#[tokio::test]
async fn form_create_without_a_bool_is_false() {
    let sq = item_pool().await;
    let app = items(&sq);
    let r = Request::builder()
        .method(Method::POST)
        .uri("/items")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("title=u&settings=%7B%7D"))
        .unwrap();
    let (status, v) = send(&app, r).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let stored: (String, bool) =
        sqlx::query_as("SELECT status, is_public FROM vs_wf_item WHERE id = ?")
            .bind(v["id"].as_i64().expect("id"))
            .fetch_one(&sq)
            .await
            .expect("read");
    assert_eq!(stored, ("draft".into(), false));
}

/// #2528: a create that leaves out a defaulted field gets the column default.
#[tokio::test]
async fn create_applies_column_defaults() {
    let sq = item_pool().await;
    let app = items(&sq);
    let (status, v) = send(
        &app,
        req(
            Method::POST,
            "/items",
            r#"{"title":"x","settings":{}}"#,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let stored: (String, Option<String>, bool, Option<String>) =
        sqlx::query_as("SELECT status, note, is_public, contact FROM vs_wf_item WHERE id = ?")
            .bind(v["id"].as_i64().expect("id"))
            .fetch_one(&sq)
            .await
            .expect("read");
    assert_eq!(stored, ("draft".into(), Some("n/a".into()), true, None));
}

/// #2528: a defaulted field is not `required` in the create body.
#[cfg(feature = "openapi")]
#[test]
fn create_spec_does_not_require_defaulted_fields() {
    let (_, item) = ViewSet::for_model(Item::SCHEMA)
        .openapi_paths("/x", "X")
        .into_iter()
        .find(|(p, _)| p == "/x")
        .unwrap();
    let v = serde_json::to_value(item).unwrap();
    let post = &v["post"]["requestBody"]["content"]["application/json"]["schema"];
    assert_eq!(
        post["required"],
        serde_json::json!(["title", "settings"]),
        "{post}"
    );
}

/// #2530: JSON null on a NOT NULL field and a junk Bool are 400s, not coerced.
#[tokio::test]
async fn json_body_is_decoded_by_field_type() {
    let sq = item_pool().await;
    let app = items(&sq);
    let id = new_item(&app).await;
    let uri = format!("/items/{id}");
    let stored = || async {
        sqlx::query_as::<_, (String, String, bool, Option<String>)>(
            "SELECT title, settings, is_public, contact FROM vs_wf_item WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&sq)
        .await
        .expect("read")
    };
    let before = stored().await;
    for body in [
        r#"{"settings":null}"#,
        r#"{"title":null}"#,
        r#"{"is_public":null}"#,
        r#"{"is_public":"nope"}"#,
    ] {
        let (status, v) = send(&app, req(Method::PATCH, &uri, body, None)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {v}");
        assert_eq!(stored().await, before, "{body} wrote");
    }

    // Null on a nullable field is NULL; a JSON string is a JSON string.
    let set = r#"{"contact":"a@b.co"}"#;
    let (status, v) = send(&app, req(Method::PATCH, &uri, set, None)).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let body = r#"{"contact":null,"settings":"hello"}"#;
    let (status, v) = send(&app, req(Method::PATCH, &uri, body, None)).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let (_, settings, _, contact) = stored().await;
    assert_eq!((settings.as_str(), contact), (r#""hello""#, None));

    let form = Request::builder()
        .method(Method::PATCH)
        .uri(&uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("is_public=nope"))
        .unwrap();
    let (status, v) = send(&app, form).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
}
