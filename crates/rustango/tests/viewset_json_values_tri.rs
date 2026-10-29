//! A `ViewSet` JSON body stores values as sent: `""` is not `null`, a
//! decimal number keeps its digits, and a non-finite float is refused.

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    any(feature = "admin", feature = "tenancy")
))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rust_decimal::Decimal;
use rustango::core::Model as _;
use rustango::sql::{Auto, FetcherPool as _, Pool};
use rustango::viewset::ViewSet;
use rustango::{tri_dialect_test, Model};
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(table = "vsjv_item", app = "vsjv")]
pub struct Item {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub nickname: Option<String>,
    pub amount: Option<Decimal>,
    pub ratio: Option<f64>,
}

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Item>(pool).await;
}

async fn send(pool: &Pool, method: Method, uri: &str, body: &str) -> (StatusCode, String) {
    let app = ViewSet::for_model(Item::SCHEMA, &["id", "nickname", "amount", "ratio"])
        .allow_anonymous()
        .router_pool("/items", pool.clone());
    let resp = app
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

async fn rows(pool: &Pool) -> Vec<Item> {
    let mut rows: Vec<Item> = Item::objects().fetch(pool).await.expect("fetch");
    rows.sort_by_key(|r| *r.id.get().expect("pk"));
    rows
}

async fn clear(pool: &Pool) {
    for r in rows(pool).await {
        r.delete_pool(pool).await.expect("clear");
    }
}

/// Create, bulk create, PUT and PATCH keep `""` apart from `null`.
async fn empty_string_is_not_null(pool: &Pool) {
    let nick = |rows: &[Item]| rows.iter().map(|r| r.nickname.clone()).collect::<Vec<_>>();
    let (status, body) = send(pool, Method::POST, "/items", r#"{"nickname":""}"#).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = send(pool, Method::POST, "/items", r#"{"nickname":null}"#).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let bulk = r#"[{"nickname":""},{"nickname":null}]"#;
    let (status, body) = send(pool, Method::POST, "/items", bulk).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let got = rows(pool).await;
    assert_eq!(
        nick(&got),
        [Some(String::new()), None, Some(String::new()), None]
    );

    let (a, b) = (
        got[0].id.get().copied().unwrap(),
        got[1].id.get().copied().unwrap(),
    );
    for method in [Method::PUT, Method::PATCH] {
        let (status, body) = send(
            pool,
            method.clone(),
            &format!("/items/{b}"),
            r#"{"nickname":""}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{method}: {body}");
        let (status, body) = send(
            pool,
            method.clone(),
            &format!("/items/{a}"),
            r#"{"nickname":null}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{method}: {body}");
        let got = rows(pool).await;
        assert_eq!(
            (got[0].nickname.clone(), got[1].nickname.clone()),
            (None, Some(String::new())),
            "{method}"
        );
        // Swap back for the next method.
        send(
            pool,
            Method::PATCH,
            &format!("/items/{a}"),
            r#"{"nickname":""}"#,
        )
        .await;
        send(
            pool,
            Method::PATCH,
            &format!("/items/{b}"),
            r#"{"nickname":null}"#,
        )
        .await;
    }
    clear(pool).await;
}

/// The value the ORM stores for `text` written as an exact `Decimal`.
async fn stored_exact(pool: &Pool, text: &str) -> Option<Decimal> {
    let mut item = Item {
        id: Auto::default(),
        nickname: None,
        amount: Some(text.parse().expect("decimal")),
        ratio: None,
    };
    item.insert_pool(pool).await.expect("insert");
    let id = *item.id.get().expect("pk");
    let got = rows(pool)
        .await
        .into_iter()
        .find(|r| r.id.get() == Some(&id))
        .unwrap();
    clear(pool).await;
    got.amount
}

/// A decimal sent as a JSON number is stored as the ORM stores its exact
/// text, not as the nearest f64.
async fn decimal_number_keeps_its_digits(pool: &Pool) {
    for text in ["1.234567890123456789", "12345678901234567.89"] {
        let want = stored_exact(pool, text).await;
        let (status, body) = send(
            pool,
            Method::POST,
            "/items",
            &format!(r#"{{"amount":{text}}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let bulk = format!(r#"[{{"amount":{text}}}]"#);
        let (status, body) = send(pool, Method::POST, "/items", &bulk).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let id = rows(pool).await[0].id.get().copied().unwrap();
        let (status, body) = send(
            pool,
            Method::PATCH,
            &format!("/items/{id}"),
            &format!(r#"{{"amount":{text}}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let got: Vec<_> = rows(pool).await.into_iter().map(|r| r.amount).collect();
        assert_eq!(got, [want, want], "{text}");
        clear(pool).await;
    }
}

/// NaN and infinity are a 400, not a NULL or a driver error.
async fn non_finite_float_is_refused(pool: &Pool) {
    for bad in ["NaN", "inf", "-infinity"] {
        let (status, body) = send(
            pool,
            Method::POST,
            "/items",
            &format!(r#"{{"ratio":"{bad}"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
    }
    assert!(rows(pool).await.is_empty());
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        empty_string_is_not_null,
        decimal_number_keeps_its_digits,
        non_finite_float_is_refused,
    ],
}
