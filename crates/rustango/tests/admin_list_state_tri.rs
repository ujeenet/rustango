//! Admin list paging order on every backend (#1917).

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

const PREFIX: &str = "/adm";

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "adminls_item",
    display = "title",
    admin(
        list_display = "title, flag",
        list_filter = "flag",
        list_per_page = 2,
        ordering = "rank",
        date_hierarchy = "made_on",
        actions = "delete_selected"
    )
)]
#[allow(dead_code)]
pub struct Item {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
    pub flag: bool,
    pub rank: i64,
    pub made_on: chrono::NaiveDate,
}

fn kind_filters(value: &str) -> Vec<Filter> {
    match value {
        "low" => vec![Filter::new("rank", Op::Eq, SqlValue::I64(0))],
        _ => Vec::new(),
    }
}
rustango::register_admin_list_filter!(
    "adminls_item",
    "kind",
    "Kind",
    &[("low", "Low")],
    kind_filters,
);

async fn get(pool: &Pool, uri: &str) -> String {
    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix(PREFIX)
        .build();
    let res = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8_lossy(&bytes).into_owned();
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    body
}

async fn seed(pool: &Pool, title: &str, flag: bool) -> i64 {
    *seed_item(pool, title, flag).await.id.get().expect("pk")
}

async fn seed_item(pool: &Pool, title: &str, flag: bool) -> Item {
    let mut item = Item {
        id: Auto::default(),
        title: title.into(),
        flag,
        rank: 0,
        made_on: chrono::NaiveDate::from_ymd_opt(2024, 3, 9).unwrap(),
    };
    item.insert_pool(pool).await.expect("insert");
    item
}

/// Equal `rank`s tie; the PK breaks the tie, even after an UPDATE moved
/// row one to the end of the PG heap.
async fn equal_sort_keys_page_in_pk_order(pool: &Pool) {
    let mut first = seed_item(pool, "row-a", false).await;
    for t in ["row-b", "row-c", "row-d"] {
        seed(pool, t, false).await;
    }
    first.save_pool(pool).await.expect("touch row one");
    let page1 = get(pool, "/adminls_item").await;
    assert!(
        page1.contains("row-a") && page1.contains("row-b"),
        "{page1}"
    );
    let page2 = get(pool, "/adminls_item?page=2").await;
    assert!(
        page2.contains("row-c") && page2.contains("row-d"),
        "{page2}"
    );
}

tri_dialect_test! {
    model: Item,
    scenarios: [
        equal_sort_keys_page_in_pk_order,
    ],
}
