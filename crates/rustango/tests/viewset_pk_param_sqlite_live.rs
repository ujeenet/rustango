//! `ViewSet::pk_param` — rename the detail-route capture (#1194).
//!
//! axum permits only one capture name per path position across a router, so a
//! hand-written route mounted beside a ViewSet that spells the same position
//! differently (`/{id}`, `/{token}`) panics at startup — and the panic points
//! at axum, not at the ViewSet. Renaming the ViewSet's capture is the escape
//! hatch; these tests pin that it works, that the route still resolves, and
//! that the generated OpenAPI agrees with the route it documents.

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "serializer"))]

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use rustango::core::Model as _;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone, serde::Serialize, serde::Deserialize)]
#[rustango(table = "vs_pk_note")]
#[rustango(app = "vs_pk_app")]
pub struct Note {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

async fn pool_with_row() -> Pool {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite");
    sqlx::query(
        "CREATE TABLE vs_pk_note (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL)",
    )
    .execute(&sq)
    .await
    .unwrap();
    sqlx::query("INSERT INTO vs_pk_note (title) VALUES ('hello')")
        .execute(&sq)
        .await
        .unwrap();
    Pool::Sqlite(sq)
}

/// The renamed capture still routes: `GET /notes/{id}/1` resolves and returns
/// the row, so renaming changes the spelling and nothing else.
#[tokio::test]
async fn renamed_capture_still_resolves_the_detail_route() {
    let pool = pool_with_row().await;
    let app = rustango::viewset::ViewSet::for_model(Note::SCHEMA)
        .pk_param("id")
        .router_pool("/notes", pool);

    let res = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/notes/1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "detail route must resolve");
}

/// The regression this exists for: a sibling route using a *different* capture
/// name at the same position. With the ViewSet defaulting to `{pk}` this
/// panics inside axum at startup; matching the names makes it mount.
#[tokio::test]
async fn sibling_route_with_matching_capture_name_mounts() {
    let pool = pool_with_row().await;
    let vs = rustango::viewset::ViewSet::for_model(Note::SCHEMA)
        .pk_param("id")
        .router_pool("/notes", pool);

    // A hand-written neighbour spelling the same position `{id}`.
    let app = vs.merge(axum::Router::new().route(
        "/notes/{id}/archive",
        axum::routing::post(|| async { "archived" }),
    ));

    let res = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/notes/1/archive")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "sibling route must mount + serve"
    );
}

/// The generated spec must document the capture the server actually binds —
/// otherwise the OpenAPI describes a path variable that does not exist.
#[test]
fn openapi_path_and_parameter_follow_the_rename() {
    let vs = rustango::viewset::ViewSet::for_model(Note::SCHEMA).pk_param("id");
    let paths = vs.openapi_paths("/notes", "Note");

    let item = paths
        .iter()
        .find(|(p, _)| p.contains('{'))
        .expect("an item path");
    assert_eq!(item.0, "/notes/{id}", "path must use the renamed capture");

    // Path-level `parameters` (shared by every operation on the item path).
    let json = serde_json::to_value(&item.1).unwrap();
    let params = json["parameters"].as_array().expect("path parameters");
    assert!(
        params
            .iter()
            .any(|p| p["name"] == "id" && p["in"] == "path"),
        "the path parameter must be named `id`, got: {params:?}"
    );
}

/// Default is unchanged: no call to `pk_param` still yields `{pk}`.
#[test]
fn default_capture_is_still_pk() {
    let vs = rustango::viewset::ViewSet::for_model(Note::SCHEMA);
    assert_eq!(vs.pk_param_name(), "pk");
    let paths = vs.openapi_paths("/notes", "Note");
    assert!(paths.iter().any(|(p, _)| p == "/notes/{pk}"));
}
