//! ModelForm and the admin report the PK the INSERT wrote (#1894), and
//! refuse before writing when MySQL cannot read it back (#1978).

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "admin"
))]

use std::collections::HashMap;

use axum::body::Body;
use axum::http::{header, Method, Request};
use rustango::core::{Model as _, SqlValue};
use rustango::forms::ModelForm;
use rustango::sql::{CounterPool as _, Pool};
use rustango::{tri_dialect_test, Model};
use tower::ServiceExt as _;

#[derive(Model, Debug, Clone)]
#[rustango(table = "pk1894_tag", app = "pk1894")]
pub struct Tag {
    #[rustango(primary_key, max_length = 64)]
    pub slug: String,
    #[rustango(max_length = 32)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "pk1894_token", app = "pk1894")]
pub struct Token {
    #[rustango(primary_key)]
    pub id: uuid::Uuid,
    #[rustango(max_length = 32)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "pk1978_coupon", app = "pk1894")]
pub struct Coupon {
    #[rustango(
        primary_key,
        auto_uuid,
        default = "'7c9e6679-7425-40de-944b-e07fc1f90ae7'"
    )]
    pub id: rustango::sql::Auto<uuid::Uuid>,
    #[rustango(max_length = 32)]
    pub name: String,
}

/// Integer PK beside a generated `f64`, which joins RETURNING.
#[derive(Model, Debug, Clone)]
#[rustango(table = "pk1978_line", app = "pk1894")]
pub struct Line {
    #[rustango(primary_key)]
    pub id: rustango::sql::Auto<i64>,
    pub price: f64,
    #[rustango(generated_as = "price * 2")]
    pub doubled: f64,
}

const UUID_A: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
const UUID_DEFAULT: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Tag>(pool).await;
    rustango::testkit::matrix::fresh_table::<Token>(pool).await;
    rustango::testkit::matrix::fresh_table::<Coupon>(pool).await;
    rustango::testkit::matrix::fresh_table::<Line>(pool).await;
}

async fn model_form_returns_the_written_pk(pool: &Pool) {
    let data = HashMap::from([("name".to_owned(), "Rust".to_owned())]);
    let mut prep = ModelForm::new(Tag::SCHEMA, data.clone())
        .prepare_save()
        .expect("valid");
    prep.set("slug", "rust");
    let pk = prep.commit_pool(pool).await.expect("insert tag");
    assert_eq!(pk, SqlValue::String("rust".into()));

    let uuid = uuid::Uuid::parse_str(UUID_A).unwrap();
    let mut prep = ModelForm::new(Token::SCHEMA, data)
        .prepare_save()
        .expect("valid");
    prep.set("id", uuid);
    let pk = prep.commit_pool(pool).await.expect("insert token");
    assert_eq!(pk, SqlValue::Uuid(uuid));
}

async fn location(app: axum::Router, uri: &str, body: String) -> String {
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let loc = resp
        .headers()
        .get(header::LOCATION)
        .map(|v| v.to_str().unwrap().to_owned());
    loc.unwrap_or_else(|| panic!("{uri}: no redirect, status {status}"))
}

async fn admin_create_redirects_to_the_new_pk(pool: &Pool) {
    let admin = || {
        rustango::admin::Builder::new(pool.clone())
            .admin_prefix("")
            .build()
    };
    let got = location(
        admin(),
        "/pk1894_tag",
        "slug=rust&name=Rust&_continue=1".into(),
    )
    .await;
    assert_eq!(got, "/pk1894_tag/rust");
    let got = location(
        admin(),
        "/pk1894_token",
        format!("id={UUID_A}&name=T&_continue=1"),
    )
    .await;
    assert_eq!(got, format!("/pk1894_token/{UUID_A}"));
}

async fn post(app: axum::Router, uri: &str, body: &str) -> axum::response::Response {
    app.oneshot(
        Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body.to_owned()))
            .unwrap(),
    )
    .await
    .unwrap()
}

/// A DB-default UUID PK: read back where RETURNING exists; on MySQL
/// refused before the INSERT, so a re-submit can't duplicate the row.
async fn db_default_pk_is_read_or_refused_before_the_insert(pool: &Pool) {
    let admin = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("")
        .build();
    let resp = post(admin, "/pk1978_coupon", "name=C&_continue=1").await;
    let loc = resp
        .headers()
        .get(header::LOCATION)
        .map(|v| v.to_str().unwrap().to_owned());
    let form = ModelForm::new(Coupon::SCHEMA, HashMap::from([("name".into(), "F".into())]))
        .prepare_save()
        .expect("valid");
    let committed = form.commit_pool(pool).await;
    let rows = Coupon::objects().count(pool).await.expect("count");
    if pool.dialect().name() == "mysql" {
        assert_eq!(loc, None, "MySQL cannot know the new PK");
        assert!(committed.is_err(), "{committed:?}");
        assert_eq!(rows, 0, "refused before the INSERT");
    } else {
        assert_eq!(loc, Some(format!("/pk1978_coupon/{UUID_DEFAULT}")));
        // The second INSERT hits the same default PK.
        assert!(committed.is_err(), "{committed:?}");
        assert_eq!(rows, 1);
    }
}

/// Only the PK decides the refusal: non-PK RETURNING columns still insert.
async fn integer_pk_inserts_beside_non_integer_returning_columns(pool: &Pool) {
    let mut line = Line {
        id: rustango::sql::Auto::Unset,
        price: 1.5,
        doubled: 0.0,
    };
    line.insert_pool(pool).await.expect("insert");
    assert!(
        matches!(line.id, rustango::sql::Auto::Set(_)),
        "{:?}",
        line.id
    );
    assert_eq!(Line::objects().count(pool).await.expect("count"), 1);
    // MySQL has no RETURNING, so the placeholder stays until a re-read.
    let want = if pool.dialect().name() == "mysql" {
        0.0
    } else {
        3.0
    };
    assert_eq!(line.doubled, want);
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        model_form_returns_the_written_pk,
        admin_create_redirects_to_the_new_pk,
        db_default_pk_is_read_or_refused_before_the_insert,
        integer_pk_inserts_beside_non_integer_returning_columns,
    ],
}
