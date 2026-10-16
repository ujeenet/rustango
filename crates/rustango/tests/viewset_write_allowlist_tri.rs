//! A `ViewSet` writes only the fields it was built with.

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    any(feature = "admin", feature = "tenancy")
))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::core::Model as _;
use rustango::sql::{Auto, FetcherPool as _, Pool};
use rustango::viewset::ViewSet;
use rustango::{tri_dialect_test, Model};
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(table = "vs1401_acct", app = "vs1401")]
pub struct Acct {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
    #[rustango(default = "0")]
    pub salary: i64,
    #[rustango(default = "false")]
    pub is_admin: bool,
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Acct>(pool).await;
}

fn app(pool: &Pool) -> axum::Router {
    ViewSet::for_model(Acct::SCHEMA, &["id", "title"])
        .allow_anonymous()
        .router_pool("/accts", pool.clone())
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

async fn only(pool: &Pool) -> Acct {
    let mut rows: Vec<Acct> = Acct::objects().fetch(pool).await.expect("fetch");
    assert_eq!(rows.len(), 1);
    rows.remove(0)
}

/// Create, PUT and PATCH ignore a posted column outside the list.
async fn unlisted_columns_are_not_written(pool: &Pool) {
    let evil = r#"{"title":"a","salary":999,"is_admin":true}"#;
    let (status, body) = send(pool, Method::POST, "/accts", evil).await;
    assert_eq!(status, StatusCode::CREATED, "create: {body}");
    let a = only(pool).await;
    assert_eq!((a.salary, a.is_admin), (0, false), "create mass-assigned");
    let id = *a.id.get().expect("pk");

    let uri = format!("/accts/{id}");
    let evil = r#"{"title":"b","salary":999,"is_admin":true}"#;
    for method in [Method::PUT, Method::PATCH] {
        let (status, body) = send(pool, method.clone(), &uri, evil).await;
        assert_eq!(status, StatusCode::OK, "{method}: {body}");
        let a = only(pool).await;
        assert_eq!(a.title, "b");
        assert_eq!((a.salary, a.is_admin), (0, false), "{method} mass-assigned");
    }
}

tri_dialect_test! {
    setup: setup,
    scenarios: [unlisted_columns_are_not_written],
}
