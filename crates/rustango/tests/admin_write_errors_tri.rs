//! Admin write errors on every backend: a bad action is a 400 (#2346);
//! a refused write shows a plain message, never the driver's text (#2345);
//! deleting a referenced row is a 409 naming the referrer (#2340).

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "admin"
))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::sql::{Auto, FetcherPool as _, Pool};
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

#[derive(Model, Debug, Clone)]
#[rustango(table = "wrerr_child", admin(list_display = "note"))]
#[allow(dead_code)]
pub struct Child {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(fk = "wrerr_parent", on = "id")]
    pub parent_id: i64,
    pub qty: i32,
    #[rustango(max_length = 32, unique)]
    pub note: String,
}

async fn setup(pool: &Pool) {
    use rustango::testkit::matrix::{drop_table, fresh_table};
    drop_table(pool, "wrerr_child").await;
    fresh_table::<Parent>(pool).await;
    fresh_table::<Child>(pool).await;
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

/// What a driver message would put on the page.
fn assert_no_driver_text(body: &str) {
    for raw in [
        "violates",
        "constraint failed",
        "Duplicate entry",
        "foreign key constraint",
        "wrerr_parent_name_key",
        "wrerr_parent.name",
        "wrerr_child_parent_id_fkey",
    ] {
        assert!(!body.contains(raw), "driver text `{raw}` leaked");
    }
    assert!(body.contains("error id "), "no correlation id");
}

/// A duplicate on create and on edit names the field, not the constraint.
async fn unique_refusal_is_a_plain_message(pool: &Pool) {
    seed_parent(pool, "taken").await;
    let other = seed_parent(pool, "other").await;
    for uri in ["/wrerr_parent".to_owned(), format!("/wrerr_parent/{other}")] {
        let (status, body) = post(pool, &uri, "name=taken").await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert_no_driver_text(&body);
        assert!(
            body.contains("A Parent with this name already exists."),
            "{uri}: {body}"
        );
    }
}

/// A missing FK target is a plain message too.
async fn fk_refusal_is_a_plain_message(pool: &Pool) {
    let (status, body) = post(pool, "/wrerr_child", "parent_id=9999&qty=1&note=n").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_no_driver_text(&body);
    assert!(body.contains("does not exist"), "{body}");
}

async fn seed_child(pool: &Pool, parent_id: i64, note: &str) {
    let mut c = Child {
        id: Auto::default(),
        parent_id,
        qty: 1,
        note: note.into(),
    };
    c.insert_pool(pool).await.expect("insert child");
}

async fn parent_count(pool: &Pool) -> usize {
    Parent::objects().fetch(pool).await.expect("fetch").len()
}

/// Single delete and `delete_selected` of a referenced row: 409, row kept.
async fn deleting_a_referenced_row_is_a_409(pool: &Pool) {
    let p = seed_parent(pool, "p").await;
    seed_child(pool, p, "c").await;
    for (uri, form) in [
        (format!("/wrerr_parent/{p}/delete"), String::new()),
        (
            "/wrerr_parent/__action".to_owned(),
            format!("action=delete_selected&_selected={p}"),
        ),
    ] {
        let (status, body) = post(pool, &uri, &form).await;
        assert_eq!(status, StatusCode::CONFLICT, "{uri}: {body}");
        assert!(body.contains("wrerr_child"), "{uri}: {body}");
        assert_eq!(parent_count(pool).await, 1, "{uri}");
    }
    // Control: an unreferenced row still goes.
    let q = seed_parent(pool, "q").await;
    let (status, body) = post(pool, &format!("/wrerr_parent/{q}/delete"), "").await;
    assert!(status.is_redirection(), "{status}: {body}");
    assert_eq!(parent_count(pool).await, 1);
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        unknown_action_is_a_400,
        unique_refusal_is_a_plain_message,
        fk_refusal_is_a_plain_message,
        deleting_a_referenced_row_is_a_409,
    ],
}
