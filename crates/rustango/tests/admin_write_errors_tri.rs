//! Admin write errors on every backend: a bad action is a 400 (#2346).

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "admin"
))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::sql::{Auto, Pool};
use rustango::{tri_dialect_test, Model};
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "wrerr_parent",
    display = "name",
    admin(list_display = "name", actions = "delete_selected")
)]
#[allow(dead_code)]
pub struct Parent {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32, unique)]
    pub name: String,
}

async fn setup(pool: &Pool) {
    use rustango::testkit::matrix::fresh_table;
    fresh_table::<Parent>(pool).await;
}

async fn post(pool: &Pool, uri: &str, form: &str) -> (StatusCode, String) {
    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .build();
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(form.to_owned()))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn seed_parent(pool: &Pool, name: &str) -> i64 {
    let mut p = Parent {
        id: Auto::default(),
        name: name.into(),
    };
    p.insert_pool(pool).await.expect("insert parent");
    *p.id.get().expect("pk")
}

/// An action the model does not list is the client's error (#2346).
async fn unknown_action_is_a_400(pool: &Pool) {
    let p = seed_parent(pool, "p").await;
    let (status, body) = post(
        pool,
        "/wrerr_parent/__action",
        &format!("action=nope&_selected={p}"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        unknown_action_is_a_400,
    ],
}
