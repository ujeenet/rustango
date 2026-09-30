//! `register_admin_queryset!` scopes by-pk reads and facet counts on
//! every backend (#1859).

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "admin"
))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::core::{Filter, Op, SqlValue};
use rustango::sql::{Auto, Pool};
use rustango::{tri_dialect_test, Model};
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "rowscope_item",
    display = "title",
    admin(list_display = "title", list_filter = "owner_id")
)]
#[allow(dead_code)]
pub struct Item {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
    pub owner_id: i64,
}

fn owner_one(_: &axum::http::request::Parts) -> Vec<Filter> {
    vec![Filter::new("owner_id", Op::Eq, SqlValue::I64(1))]
}
rustango::register_admin_queryset!("rowscope_item", owner_one);

async fn get(pool: &Pool, uri: &str) -> (StatusCode, String) {
    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .build();
    let res = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn seed(pool: &Pool, title: &str, owner_id: i64) -> i64 {
    let mut item = Item {
        id: Auto::default(),
        title: title.into(),
        owner_id,
    };
    item.insert_pool(pool).await.expect("insert");
    *item.id.get().expect("pk")
}

async fn hidden_rows_are_404_and_uncounted(pool: &Pool) {
    let mine = seed(pool, "mine", 1).await;
    let theirs = seed(pool, "theirs", 77).await;
    assert_eq!(
        get(pool, &format!("/rowscope_item/{mine}")).await.0,
        StatusCode::OK
    );
    let (status, body) = get(pool, &format!("/rowscope_item/{theirs}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (status, body) = get(pool, "/rowscope_item").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("owner_id=1"), "own facet: {body}");
    assert!(!body.contains("owner_id=77"), "scoped facet: {body}");
}

tri_dialect_test! {
    model: Item,
    scenarios: [hidden_rows_are_404_and_uncounted],
}
