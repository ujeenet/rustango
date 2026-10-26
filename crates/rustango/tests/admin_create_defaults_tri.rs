//! An admin create that does not send a defaulted field leaves it to its
//! column default, on every backend.

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
#[rustango(table = "acd_item")]
#[allow(dead_code)]
pub struct Item {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub title: String,
    #[rustango(max_length = 16, default = "'draft'")]
    pub status: String,
    #[rustango(default = "7")]
    pub rank: i64,
    #[rustango(max_length = 16, default = "'n/a'")]
    pub note: Option<String>,
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Item>(pool).await;
}

async fn post(pool: &Pool, form: &str) -> (StatusCode, String) {
    let app = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .build();
    let req = Request::builder()
        .method("POST")
        .uri("/acd_item")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(form.to_owned()))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn item(pool: &Pool, title: &str) -> Item {
    Item::objects()
        .fetch(pool)
        .await
        .expect("fetch")
        .into_iter()
        .find(|i| i.title == title)
        .unwrap_or_else(|| panic!("no item {title}"))
}

async fn absent_defaulted_fields_take_the_column_default(pool: &Pool) {
    let (status, html) = post(pool, "title=absent").await;
    assert!(status.is_redirection(), "{status}: {html}");
    let i = item(pool, "absent").await;
    assert_eq!(
        (i.status.as_str(), i.rank, i.note.as_deref()),
        ("draft", 7, Some("n/a"))
    );
    // A sent empty value is kept: NULL here, not the default.
    let (status, html) = post(pool, "title=cleared&status=s&rank=1&note=").await;
    assert!(status.is_redirection(), "{status}: {html}");
    assert_eq!(item(pool, "cleared").await.note, None);
    // A typed value wins.
    let (status, html) = post(pool, "title=typed&status=live&rank=3&note=x").await;
    assert!(status.is_redirection(), "{status}: {html}");
    let i = item(pool, "typed").await;
    assert_eq!(
        (i.status.as_str(), i.rank, i.note.as_deref()),
        ("live", 3, Some("x"))
    );
}

tri_dialect_test!(
    setup: setup,
    scenarios: [absent_defaulted_fields_take_the_column_default],
);
