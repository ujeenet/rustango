//! ModelForm and the admin report the PK the INSERT wrote (#1894).

#![cfg(all(
    any(feature = "postgres", feature = "mysql", feature = "sqlite"),
    feature = "admin"
))]

use std::collections::HashMap;

use axum::body::Body;
use axum::http::{header, Method, Request};
use rustango::core::{Model as _, SqlValue};
use rustango::forms::ModelForm;
use rustango::sql::Pool;
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

const UUID_A: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Tag>(pool).await;
    rustango::testkit::matrix::fresh_table::<Token>(pool).await;
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

tri_dialect_test! {
    setup: setup,
    scenarios: [
        model_form_returns_the_written_pk,
        admin_create_redirects_to_the_new_pk,
    ],
}
